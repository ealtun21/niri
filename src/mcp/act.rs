//! `look`, `click`, `type`, `key`, `scroll` and `drag` tools.

use std::os::unix::fs::DirBuilderExt as _;

use niri_ipc::{AgentButton, AgentMod, AgentOp, Request, Response};
use serde_json::Value;

use super::elements::{format_elements, Elements, MAX_ELEMENT_LINES};
use super::{Content, Server, Tool, ToolOutcome};

/// Longest text one `type` call sends.
const MAX_TYPE_CHARS: usize = 20_000;

fn window_arg(args: &Value) -> Result<u64, ToolOutcome> {
    args.get("window")
        .and_then(Value::as_u64)
        .ok_or_else(|| ToolOutcome::error("bad_window: window must be a window id"))
}

fn bad(msg: impl std::fmt::Display) -> ToolOutcome {
    ToolOutcome::error(format!("bad_args: {msg}"))
}

/// niri's own error (it starts with a code). An older running niri cannot parse newer requests.
fn niri_error(e: String) -> ToolOutcome {
    if e == "error parsing request" {
        return ToolOutcome::error(
            "niri_refused: the running niri is older than `niri mcp` and does not know this \
             request; ask the owner to log out and back in",
        );
    }
    ToolOutcome::error(e)
}

fn unreachable(e: std::io::Error) -> ToolOutcome {
    ToolOutcome::error(format!("niri_unreachable: cannot reach niri: {e}"))
}

fn no_tree(why: &str) -> Elements {
    Elements {
        list: Vec::new(),
        cut: false,
        note: Some(format!("no accessibility tree: {why}")),
    }
}

fn look_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let id = match window_arg(args) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let max_edge = match args.get("max_edge") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
            Some(n) if n > 0 => Some(n),
            _ => return bad("max_edge must be a positive integer"),
        },
    };
    // A new capture invalidates the old list, whatever happens next.
    s.elements.remove(&id);
    let Some(base) = s.profile_base.clone() else {
        return ToolOutcome::error("XDG_RUNTIME_DIR is not set");
    };
    if let Err(e) = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&base)
    {
        return ToolOutcome::error(format!("cannot create {}: {e}", base.display()));
    }
    let path = base.join(format!("look-{id}.png"));
    let req = Request::CaptureWindow {
        id,
        path: path.display().to_string(),
        max_edge,
        settle_ms: None,
    };
    let cap = match (s.ipc)(req) {
        Ok(Ok(Response::WindowCapture(c))) => c,
        // No tree read after a refused capture (paused, blocked_out, ...).
        Ok(Ok(other)) => return ToolOutcome::error(format!("unexpected_reply: {other:?}")),
        Ok(Err(e)) => return niri_error(e),
        Err(e) => return unreachable(e),
    };
    let png = std::fs::read(&path);
    let _ = std::fs::remove_file(&path);
    let png = match png {
        Ok(p) => p,
        Err(e) => return ToolOutcome::error(format!("cannot read capture: {e}")),
    };

    let wins = match (s.ipc)(Request::Windows) {
        Ok(Ok(Response::Windows(w))) => Some(w),
        _ => None,
    };
    if let Some(w) = &wins {
        // Forget element lists of windows that have closed.
        s.elements.retain(|k, _| w.iter().any(|w| w.id == *k));
    }
    let me = wins.as_ref().and_then(|w| w.iter().find(|w| w.id == id));
    let title = me
        .and_then(|w| w.title.clone())
        .or_else(|| cap.title.clone())
        .unwrap_or_default();
    let elements = match me.and_then(|w| w.pid).and_then(|p| u32::try_from(p).ok()) {
        Some(pid) => {
            let same = wins
                .iter()
                .flatten()
                .filter(|w| w.pid == me.and_then(|m| m.pid))
                .count();
            (s.read_elements)(pid, &title, same == 1, &cap)
        }
        None => no_tree("niri did not report a pid for the window"),
    };
    let text = format!(
        "window {id} {:?}, {}x{} px, workspace_visible={}, scale={}\n{}",
        title
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>(),
        cap.width,
        cap.height,
        cap.workspace_visible,
        cap.scale,
        format_elements(&elements, MAX_ELEMENT_LINES),
    );
    s.elements
        .insert(id, (cap.width, cap.height, elements.list));
    ToolOutcome {
        content: vec![Content::Image(png), Content::Text(text)],
        is_error: false,
    }
}

/// A point given as an element number or as pixel coordinates (exactly one).
fn point(
    s: &Server,
    id: u64,
    args: &Value,
    el: &str,
    x: &str,
    y: &str,
) -> Result<(f64, f64), ToolOutcome> {
    let (e, xv, yv) = (args.get(el), args.get(x), args.get(y));
    let some = |v: Option<&Value>| v.is_some_and(|v| !v.is_null());
    match (some(e), some(xv) || some(yv)) {
        (true, true) => Err(bad(format!("give {el} or {x}/{y}, not both"))),
        (false, false) => Err(bad(format!("give {el} or {x} and {y}"))),
        (false, true) => match (xv.and_then(Value::as_f64), yv.and_then(Value::as_f64)) {
            (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Ok((x, y)),
            _ => Err(bad(format!("{x} and {y} must both be numbers"))),
        },
        (true, false) => {
            let Some(n) = e.and_then(Value::as_u64).filter(|n| *n >= 1) else {
                return Err(bad(format!("{el} must be an element number, 1 or more")));
            };
            let Some((w, h, list)) = s.elements.get(&id) else {
                return Err(ToolOutcome::error(
                    "stale_elements: look at the window again",
                ));
            };
            let Some(el) = list.get(n as usize - 1) else {
                return Err(bad(format!("no element {n} in the last look")));
            };
            // The centre of the part inside the image.
            let (x, y, ew, eh) = el.rect;
            let (x0, y0) = (x.max(0.), y.max(0.));
            let (x1, y1) = ((x + ew).min(f64::from(*w)), (y + eh).min(f64::from(*h)));
            if x1 <= x0 || y1 <= y0 {
                return Err(bad(format!("element {n} is outside the image, look again")));
            }
            Ok(((x0 + x1) / 2., (y0 + y1) / 2.))
        }
    }
}

/// Expires the window's elements, then sends the operation.
fn send(s: &mut Server, id: u64, op: AgentOp) -> ToolOutcome {
    s.elements.remove(&id);
    match (s.ipc)(Request::Agent { id, op }) {
        Ok(Ok(Response::AgentDone(d))) => ToolOutcome::text(match d.capture_age_ms {
            Some(ms) => format!("ok (capture_age_ms={ms})"),
            None => "ok".into(),
        }),
        Ok(Ok(_)) => ToolOutcome::text("ok"),
        Ok(Err(e)) => niri_error(e),
        Err(e) => unreachable(e),
    }
}

fn click_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let id = match window_arg(args) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let (x, y) = match point(s, id, args, "element", "x", "y") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let button = match args.get("button").and_then(Value::as_str) {
        None | Some("left") => AgentButton::Left,
        Some("right") => AgentButton::Right,
        Some(_) => return bad("button must be left or right"),
    };
    let count = match args.get("count") {
        None | Some(Value::Null) => 1,
        Some(v) => match v.as_u64() {
            Some(n @ 1..=3) => n as u8,
            _ => return bad("count must be 1, 2 or 3"),
        },
    };
    let mut mods = Vec::new();
    for m in args
        .get("mods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        mods.push(match m.as_str() {
            Some("ctrl") => AgentMod::Ctrl,
            Some("shift") => AgentMod::Shift,
            Some("alt") => AgentMod::Alt,
            Some("super") => AgentMod::Super,
            _ => return bad("mods must be ctrl, shift, alt or super"),
        });
    }
    send(
        s,
        id,
        AgentOp::Click {
            x,
            y,
            button,
            count,
            mods,
        },
    )
}

fn scroll_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let id = match window_arg(args) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let (x, y) = match point(s, id, args, "element", "x", "y") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let num = |k| args.get(k).and_then(Value::as_f64).unwrap_or(0.);
    send(
        s,
        id,
        AgentOp::Scroll {
            x,
            y,
            dx: num("dx"),
            dy: num("dy"),
        },
    )
}

fn drag_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let id = match window_arg(args) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let (x0, y0) = match point(s, id, args, "from_element", "x0", "y0") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let (x1, y1) = match point(s, id, args, "to_element", "x1", "y1") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let steps = match args.get("steps") {
        None | Some(Value::Null) => 10,
        Some(v) => match v.as_u64() {
            Some(n @ 1..=1000) => n as u16,
            _ => return bad("steps must be 1 to 1000"),
        },
    };
    send(
        s,
        id,
        AgentOp::Drag {
            x0,
            y0,
            x1,
            y1,
            steps,
        },
    )
}

fn type_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let id = match window_arg(args) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let Some(text) = args.get("text").and_then(Value::as_str) else {
        return bad("text must be a string");
    };
    if text.chars().count() > MAX_TYPE_CHARS {
        return bad(format!(
            "text is longer than {MAX_TYPE_CHARS} characters, type it in parts"
        ));
    }
    send(s, id, AgentOp::Type { text: text.into() })
}

fn key_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let id = match window_arg(args) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let Some(chord) = args.get("chord").and_then(Value::as_str) else {
        return bad("chord must be a string such as ctrl+l");
    };
    let repeat = match args.get("repeat") {
        None | Some(Value::Null) => 1,
        Some(v) => match v.as_u64().and_then(|n| u16::try_from(n).ok()) {
            Some(n) if n >= 1 => n,
            _ => return bad("repeat must be a positive integer"),
        },
    };
    send(
        s,
        id,
        AgentOp::Key {
            chord: chord.into(),
            repeat,
        },
    )
}

pub const TOOLS: [Tool; 6] = [
    Tool {
        name: "look",
        description: "Capture a window: returns its image and a numbered list of clickable elements. Use it first, and again after every action, before you act on a window.",
        schema: r#"{"type":"object","properties":{"window":{"type":"integer","description":"window id"},"max_edge":{"type":"integer","description":"scale the image so its longest edge is at most this many pixels"}},"required":["window"]}"#,
        handler: look_tool,
    },
    Tool {
        name: "click",
        description: "Click in a window. Give an element number from the last look when there is one, otherwise x and y pixels of the last image. Look again afterwards.",
        schema: r#"{"type":"object","properties":{"window":{"type":"integer"},"element":{"type":"integer","description":"element number from the last look"},"x":{"type":"number"},"y":{"type":"number"},"button":{"type":"string","enum":["left","right"]},"count":{"type":"integer","description":"1 to 3, 2 is a double click"},"mods":{"type":"array","items":{"type":"string","enum":["ctrl","shift","alt","super"]}}},"required":["window"]}"#,
        handler: click_tool,
    },
    Tool {
        name: "type",
        description: "Type text into the focused field of a window. Click the field first. Use `key` for Enter, Tab and shortcuts.",
        schema: r#"{"type":"object","properties":{"window":{"type":"integer"},"text":{"type":"string"}},"required":["window","text"]}"#,
        handler: type_tool,
    },
    Tool {
        name: "key",
        description: "Press a key or chord such as Return, Escape or ctrl+l in a window. Use it for Enter, Tab and shortcuts.",
        schema: r#"{"type":"object","properties":{"window":{"type":"integer"},"chord":{"type":"string"},"repeat":{"type":"integer","description":"times to press, default 1"}},"required":["window","chord"]}"#,
        handler: key_tool,
    },
    Tool {
        name: "scroll",
        description: "Scroll at an element or at x and y pixels of the last image. Positive dy scrolls down. Look again afterwards.",
        schema: r#"{"type":"object","properties":{"window":{"type":"integer"},"element":{"type":"integer"},"x":{"type":"number"},"y":{"type":"number"},"dx":{"type":"number"},"dy":{"type":"number"}},"required":["window"]}"#,
        handler: scroll_tool,
    },
    Tool {
        name: "drag",
        description: "Drag from one point to another, each given as an element number or as x and y pixels of the last image. Use it for sliders, selections and moving items.",
        schema: r#"{"type":"object","properties":{"window":{"type":"integer"},"from_element":{"type":"integer"},"x0":{"type":"number"},"y0":{"type":"number"},"to_element":{"type":"integer"},"x1":{"type":"number"},"y1":{"type":"number"},"steps":{"type":"integer","description":"intermediate motion steps, 1 to 1000, default 10"}},"required":["window"]}"#,
        handler: drag_tool,
    },
];

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    use niri_ipc::{AgentDone, Window, WindowCapture};
    use serde_json::json;

    use super::*;
    use crate::mcp::elements::Element;
    use crate::mcp::Ipc;

    type Log = Rc<RefCell<Vec<Request>>>;

    fn win(id: u64, pid: i32) -> Window {
        serde_json::from_value(json!({
            "id": id, "title": "T", "app_id": "agent-foot", "pid": pid, "workspace_id": 1,
            "is_focused": false, "is_floating": false, "is_urgent": false,
            "layout": {"pos_in_scrolling_layout": null, "tile_size": [1.0, 1.0],
                "window_size": [1, 1], "tile_pos_in_workspace_view": null,
                "window_offset_in_tile": [0.0, 0.0]},
            "focus_timestamp": null,
        }))
        .unwrap()
    }

    fn cap(id: u64, path: &str) -> WindowCapture {
        serde_json::from_value(json!({
            "id": id, "path": path, "width": 200, "height": 100, "scale": 1.0,
            "origin": [0.0, 0.0], "geometry_loc": [0.0, 0.0], "window_size": [1, 1],
            "workspace_visible": true, "app_id": null, "title": null,
        }))
        .unwrap()
    }

    fn fake_elements(_: u32, _: &str, _: bool, _: &WindowCapture) -> Elements {
        let el = |name: &str, rect| Element {
            role: "button".into(),
            name: name.into(),
            states: vec![],
            rect,
        };
        Elements {
            list: vec![
                el("OK", (0., 10., 20., 10.)),
                el("Cancel", (100., 10., 20., 10.)),
                // Half off the 200x100 image, and fully off it.
                el("Edge", (-10., 90., 40., 20.)),
                el("Away", (250., 10., 20., 10.)),
            ],
            cut: false,
            note: None,
        }
    }

    /// `agent_reply` answers `Request::Agent`; captures write a fake PNG.
    fn server(agent_reply: Result<Response, String>) -> (Server, Log, PathBuf) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("niri-mcp-act-{}-{n}", std::process::id()));
        let log: Log = Rc::default();
        let l = log.clone();
        let ipc: Ipc = Box::new(move |r| {
            let reply = match &r {
                Request::CaptureWindow { id, path, .. } => {
                    std::fs::write(path, b"\x89PNGfake").unwrap();
                    Ok(Response::WindowCapture(cap(*id, path)))
                }
                Request::Windows => Ok(Response::Windows(vec![win(7, 42)])),
                _ => agent_reply.clone(),
            };
            l.borrow_mut().push(r);
            Ok(reply)
        });
        let mut s = Server::new(ipc);
        s.profile_base = Some(dir.clone());
        s.read_elements = fake_elements;
        (s, log, dir)
    }

    fn text(o: &ToolOutcome) -> &str {
        match o.content.last().unwrap() {
            Content::Text(t) => t,
            Content::Image(_) => panic!(),
        }
    }

    fn done() -> Result<Response, String> {
        Ok(Response::AgentDone(AgentDone {
            id: 7,
            app_id: None,
            title: None,
            capture_age_ms: Some(12),
        }))
    }

    fn agent_ops(log: &Log) -> Vec<AgentOp> {
        log.borrow()
            .iter()
            .filter_map(|r| match r {
                Request::Agent { op, .. } => Some(op.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn look_returns_image_and_elements_text() {
        let (mut s, log, dir) = server(done());
        let o = look_tool(&mut s, &json!({"window": 7}));
        assert!(!o.is_error);
        assert!(matches!(&o.content[0], Content::Image(p) if p == b"\x89PNGfake"));
        let t = text(&o);
        assert!(
            t.starts_with("window 7 \"T\", 200x100 px, workspace_visible=true"),
            "{t}"
        );
        assert!(t.contains("1 button \"OK\"") && t.contains("2 button \"Cancel\""));
        // The capture file is gone and the request named it under the base dir.
        let Request::CaptureWindow { path, .. } = &log.borrow()[0] else {
            panic!()
        };
        assert_eq!(*path, dir.join("look-7.png").display().to_string());
        assert!(!dir.join("look-7.png").exists());
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn look_error_reads_no_tree() {
        let log: Log = Rc::default();
        let l = log.clone();
        let ipc: Ipc = Box::new(move |r| {
            l.borrow_mut().push(r);
            Ok(Err("paused".into()))
        });
        let mut s = Server::new(ipc);
        let dir = std::env::temp_dir().join(format!("niri-mcp-act-err-{}", std::process::id()));
        s.profile_base = Some(dir.clone());
        let o = look_tool(&mut s, &json!({"window": 7}));
        assert!(o.is_error && text(&o).starts_with("paused"));
        assert_eq!(log.borrow().len(), 1);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn click_element_uses_center() {
        let (mut s, log, dir) = server(done());
        look_tool(&mut s, &json!({"window": 7}));
        let o = click_tool(&mut s, &json!({"window": 7, "element": 2}));
        assert_eq!(text(&o), "ok (capture_age_ms=12)");
        assert!(matches!(
            &agent_ops(&log)[..],
            [AgentOp::Click { x, y, button: AgentButton::Left, count: 1, .. }]
                if *x == 110. && *y == 15.
        ));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn click_element_is_clipped_to_the_image() {
        let (mut s, log, dir) = server(done());
        look_tool(&mut s, &json!({"window": 7}));
        assert!(!click_tool(&mut s, &json!({"window": 7, "element": 3})).is_error);
        look_tool(&mut s, &json!({"window": 7}));
        let o = click_tool(&mut s, &json!({"window": 7, "element": 4}));
        assert_eq!(
            text(&o),
            "bad_args: element 4 is outside the image, look again"
        );
        assert!(matches!(
            &agent_ops(&log)[..],
            [AgentOp::Click { x, y, .. }] if *x == 15. && *y == 95.
        ));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn look_drops_element_lists_of_closed_windows() {
        let (mut s, _, dir) = server(done());
        s.elements.insert(99, (1, 1, Vec::new()));
        look_tool(&mut s, &json!({"window": 7}));
        assert!(!s.elements.contains_key(&99));
        assert!(s.elements.contains_key(&7));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn click_needs_element_or_coordinates() {
        let (mut s, log, dir) = server(done());
        look_tool(&mut s, &json!({"window": 7}));
        for a in [
            json!({"window": 7}),
            json!({"window": 7, "element": 1, "x": 1, "y": 1}),
            json!({"window": 7, "x": 1}),
        ] {
            let o = click_tool(&mut s, &a);
            assert!(o.is_error && text(&o).starts_with("bad_args"), "{a}");
        }
        assert!(agent_ops(&log).is_empty());
        let o = click_tool(&mut s, &json!({"window": 7, "x": 5, "y": 6}));
        assert!(!o.is_error);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn element_ids_expire_after_action() {
        let (mut s, log, dir) = server(done());
        look_tool(&mut s, &json!({"window": 7}));
        assert!(!click_tool(&mut s, &json!({"window": 7, "element": 1})).is_error);
        let o = click_tool(&mut s, &json!({"window": 7, "element": 1}));
        assert!(o.is_error && text(&o).starts_with("stale_elements"));
        assert_eq!(agent_ops(&log).len(), 1);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn failed_action_still_expires_elements() {
        let (mut s, _, dir) = server(Err("human_focused".into()));
        look_tool(&mut s, &json!({"window": 7}));
        assert!(type_tool(&mut s, &json!({"window": 7, "text": "x"})).is_error);
        let o = click_tool(&mut s, &json!({"window": 7, "element": 1}));
        assert!(text(&o).starts_with("stale_elements"));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn type_refuses_overlong_text() {
        let (mut s, log, _) = server(done());
        let long = "a".repeat(MAX_TYPE_CHARS + 1);
        let o = type_tool(&mut s, &json!({"window": 7, "text": long}));
        assert!(
            o.is_error && text(&o).starts_with("bad_args"),
            "{}",
            text(&o)
        );
        assert!(agent_ops(&log).is_empty());
        let ok = "é".repeat(MAX_TYPE_CHARS);
        assert!(!type_tool(&mut s, &json!({"window": 7, "text": ok})).is_error);
    }

    #[test]
    fn older_niri_is_named() {
        let (mut s, _, _) = server(Err("error parsing request".into()));
        let o = key_tool(&mut s, &json!({"window": 7, "chord": "ctrl+l"}));
        assert!(o.is_error && text(&o).starts_with("niri_refused: "));
        assert!(text(&o).contains("log out and back in"), "{}", text(&o));
    }

    #[test]
    fn unreachable_niri_is_coded() {
        let mut s = Server::new(Box::new(|_| Err(std::io::Error::other("gone"))));
        let dir = std::env::temp_dir().join(format!("niri-mcp-act-un-{}", std::process::id()));
        s.profile_base = Some(dir.clone());
        let o = look_tool(&mut s, &json!({"window": 7}));
        assert!(text(&o).starts_with("niri_unreachable: "), "{}", text(&o));
        let o = key_tool(&mut s, &json!({"window": 7, "chord": "a"}));
        assert!(text(&o).starts_with("niri_unreachable: "), "{}", text(&o));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn agent_error_is_tool_error() {
        let (mut s, _, _) = server(Err("human_focused".into()));
        let o = key_tool(&mut s, &json!({"window": 7, "chord": "ctrl+l"}));
        assert!(o.is_error && text(&o).starts_with("human_focused"));
    }

    #[test]
    fn drag_and_scroll_map_to_ops() {
        let (mut s, log, dir) = server(done());
        look_tool(&mut s, &json!({"window": 7}));
        drag_tool(
            &mut s,
            &json!({"window": 7, "from_element": 1, "x1": 3, "y1": 4}),
        );
        scroll_tool(&mut s, &json!({"window": 7, "x": 1, "y": 2, "dy": 5}));
        let ops = agent_ops(&log);
        assert!(matches!(ops[0], AgentOp::Drag { x0, y0, x1, y1, steps: 10 }
            if x0 == 10. && y0 == 15. && x1 == 3. && y1 == 4.));
        assert!(matches!(ops[1], AgentOp::Scroll { dx, dy, .. } if dx == 0. && dy == 5.));
        let _ = std::fs::remove_dir(&dir);
    }
}
