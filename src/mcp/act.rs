//! `look`, `click`, `type`, `key`, `scroll` and `drag` tools.

use std::fmt::Write as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

use niri_ipc::{
    AgentButton, AgentMod, AgentOp, CapturePopup, OutputCapture, Request, Response, ScreenHit,
    ScreenWindow, WindowCapture,
};
use serde_json::Value;

use super::elements::{
    contains, format_elements, popup_area, Element, Elements, MAX_ELEMENT_LINES,
};
use super::{Content, Server, Tool, ToolOutcome};

/// Longest text one `type` call sends.
const MAX_TYPE_CHARS: usize = 20_000;

fn window_arg(args: &Value) -> Result<u64, ToolOutcome> {
    args.get("window")
        .and_then(Value::as_u64)
        .ok_or_else(|| ToolOutcome::error("bad_window: window must be a window id"))
}

/// What a tool acts on.
enum On {
    Window(u64),
    /// The whole screen during a hand-over, on this output (`None`: niri's default).
    Screen(Option<String>),
}

/// `window`, or `screen: true` with an optional `output`.
fn on_arg(args: &Value) -> Result<On, ToolOutcome> {
    let screen = match args.get("screen") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| bad("screen must be true or false"))?,
    };
    let output = match args.get("output") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| bad("output must be an output name"))?
                .to_owned(),
        ),
    };
    if !screen {
        if output.is_some() {
            return Err(bad("output goes with screen: true"));
        }
        return window_arg(args).map(On::Window);
    }
    if args.get("window").is_some_and(|v| !v.is_null()) {
        return Err(bad("give window or screen: true, not both"));
    }
    Ok(On::Screen(output))
}

pub(super) fn bad(msg: impl std::fmt::Display) -> ToolOutcome {
    ToolOutcome::error(format!("bad_args: {msg}"))
}

/// niri's own error (it starts with a code). An older running niri cannot parse newer requests.
pub(super) fn niri_error(e: String) -> ToolOutcome {
    if e == "error parsing request" {
        return ToolOutcome::error(
            "niri_refused: the running niri is older than `niri mcp` and does not know this \
             request; ask the owner to log out and back in",
        );
    }
    ToolOutcome::error(e)
}

pub(super) fn unreachable(e: std::io::Error) -> ToolOutcome {
    ToolOutcome::error(format!("niri_unreachable: cannot reach niri: {e}"))
}

fn no_tree(why: &str) -> Elements {
    Elements {
        list: Vec::new(),
        cut: false,
        note: Some(format!("no accessibility tree: {why}")),
    }
}

/// Text from an app, on one line.
fn one_line(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// A private path for a capture file.
fn capture_path(s: &Server, name: &str) -> Result<PathBuf, ToolOutcome> {
    let Some(base) = s.profile_base.clone() else {
        return Err(ToolOutcome::error("XDG_RUNTIME_DIR is not set"));
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&base)
        .map_err(|e| ToolOutcome::error(format!("cannot create {}: {e}", base.display())))?;
    Ok(base.join(name))
}

/// Reads a capture file and removes it.
fn take_png(path: &Path) -> Result<Vec<u8>, ToolOutcome> {
    let png = std::fs::read(path);
    let _ = std::fs::remove_file(path);
    png.map_err(|e| ToolOutcome::error(format!("cannot read capture: {e}")))
}

fn look_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let on = match on_arg(args) {
        Ok(on) => on,
        Err(e) => return e,
    };
    let max_edge = match args.get("max_edge") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
            Some(n) if n > 0 => Some(n),
            _ => return bad("max_edge must be a positive integer"),
        },
    };
    let result = match on {
        On::Window(id) => look_window(s, id, max_edge),
        On::Screen(output) => look_screen(s, output, max_edge),
    };
    result.unwrap_or_else(|e| e)
}

fn look_window(s: &mut Server, id: u64, max_edge: Option<u32>) -> Result<ToolOutcome, ToolOutcome> {
    // A new capture invalidates the old list, whatever happens next.
    s.elements.remove(&id);
    let path = capture_path(s, &format!("look-{id}.png"))?;
    let req = Request::CaptureWindow {
        id,
        path: path.display().to_string(),
        max_edge,
        settle_ms: None,
    };
    let cap = match (s.ipc)(req) {
        Ok(Ok(Response::WindowCapture(c))) => c,
        // No tree read after a refused capture (paused, blocked_out, ...).
        Ok(Ok(other)) => return Err(ToolOutcome::error(format!("unexpected_reply: {other:?}"))),
        Ok(Err(e)) => return Err(niri_error(e)),
        Err(e) => return Err(unreachable(e)),
    };
    let png = take_png(&path)?;

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
        one_line(&title),
        cap.width,
        cap.height,
        cap.workspace_visible,
        cap.scale,
        format_elements(&elements, 1, MAX_ELEMENT_LINES),
    );
    s.elements
        .insert(id, (cap.width, cap.height, elements.list));
    Ok(ToolOutcome {
        content: vec![Content::Image(png), Content::Text(text)],
        is_error: false,
    })
}

fn look_screen(
    s: &mut Server,
    output: Option<String>,
    max_edge: Option<u32>,
) -> Result<ToolOutcome, ToolOutcome> {
    // A new capture invalidates the old list, whatever happens next.
    s.screen_elements = None;
    let path = capture_path(s, "look-screen.png")?;
    let req = Request::CaptureOutput {
        output,
        path: path.display().to_string(),
        max_edge,
        settle_ms: None,
    };
    let cap = match (s.ipc)(req) {
        Ok(Ok(Response::OutputCapture(c))) => c,
        Ok(Ok(other)) => return Err(ToolOutcome::error(format!("unexpected_reply: {other:?}"))),
        Ok(Err(e)) => return Err(niri_error(e)),
        Err(e) => return Err(unreachable(e)),
    };
    let png = take_png(&path)?;
    let mut text = format!(
        "screen {}, {}x{} px, scale={}\n",
        cap.output, cap.width, cap.height, cap.scale
    );
    let mut list = Vec::new();
    if cap.overview_open {
        text.push_str("elements are unavailable in the overview; click by coordinates\n");
    } else {
        read_screen_elements(s, &cap, &mut text, &mut list);
    }
    s.screen_elements = Some((cap.output, cap.width, cap.height, list));
    Ok(ToolOutcome {
        content: vec![Content::Image(png), Content::Text(text)],
        is_error: false,
    })
}

/// A window of an output capture as a capture of its own, for `read_elements`.
fn window_capture(cap: &OutputCapture, w: &ScreenWindow) -> WindowCapture {
    // A window capture counts its origin and popups in logical px from the window's surface
    // origin, an output capture in image px. Outside the overview there is no zoom.
    let s = cap.scale;
    WindowCapture {
        id: w.id,
        path: cap.path.clone(),
        width: cap.width,
        height: cap.height,
        scale: s,
        origin: (-w.surface_origin.0 / s, -w.surface_origin.1 / s),
        geometry_loc: w.geometry_loc,
        window_size: w.window_size,
        workspace_visible: true,
        app_id: None,
        title: None,
        popups: w
            .popups
            .iter()
            .map(|p| CapturePopup {
                x: p.x / s,
                y: p.y / s,
                width: p.width / s,
                height: p.height / s,
            })
            .collect(),
    }
}

/// Reads each window's elements, keeps those where the window shows (its tile or its popups, not
/// under a window above it) and numbers them in one list after a header line per window.
fn read_screen_elements(
    s: &mut Server,
    cap: &OutputCapture,
    text: &mut String,
    list: &mut Vec<Element>,
) {
    let wins = match (s.ipc)(Request::Windows) {
        Ok(Ok(Response::Windows(w))) => w,
        _ => Vec::new(),
    };
    let caps: Vec<WindowCapture> = cap.windows.iter().map(|w| window_capture(cap, w)).collect();
    let shows = |i: usize, at| {
        contains(cap.windows[i].rect, at)
            || caps[i]
                .popups
                .iter()
                .any(|p| contains(popup_area(p, &caps[i]), at))
    };
    for (i, sw) in cap.windows.iter().enumerate() {
        let me = wins.iter().find(|w| w.id == sw.id);
        let title = me.and_then(|w| w.title.clone()).unwrap_or_default();
        let app_id = me.and_then(|w| w.app_id.as_deref()).unwrap_or("-");
        let _ = writeln!(
            text,
            "window {} {} {:?}",
            sw.id,
            one_line(app_id),
            one_line(&title)
        );
        let mut elements = match me.and_then(|w| w.pid).and_then(|p| u32::try_from(p).ok()) {
            Some(pid) => {
                let same = wins
                    .iter()
                    .filter(|w| w.pid == me.and_then(|m| m.pid))
                    .count();
                (s.read_elements)(pid, &title, same == 1, &caps[i])
            }
            None => no_tree("niri did not report a pid for the window"),
        };
        elements
            .list
            .retain(|e| shows(i, e.center()) && !(0..i).any(|j| shows(j, e.center())));
        let shown = list.len();
        text.push_str(&format_elements(
            &elements,
            shown + 1,
            MAX_ELEMENT_LINES.saturating_sub(shown),
        ));
        list.append(&mut elements.list);
    }
}

/// A point given as an element number or as pixel coordinates (exactly one).
fn point(
    s: &Server,
    on: &On,
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
            let looked = match on {
                On::Window(id) => s.elements.get(id).map(|(w, h, list)| (*w, *h, list)),
                // Another output's list is not this one's.
                On::Screen(output) => s
                    .screen_elements
                    .as_ref()
                    .filter(|(looked, ..)| output.as_ref().is_none_or(|o| o == looked))
                    .map(|(_, w, h, list)| (*w, *h, list)),
            };
            let Some((w, h, list)) = looked else {
                return Err(ToolOutcome::error(match on {
                    On::Window(_) => "stale_elements: look at the window again",
                    On::Screen(_) => "stale_elements: look at the screen again",
                }));
            };
            let Some(el) = list.get(n as usize - 1) else {
                return Err(bad(format!("no element {n} in the last look")));
            };
            // The centre of the part inside the image.
            let (x, y, ew, eh) = el.rect;
            let (x0, y0) = (x.max(0.), y.max(0.));
            let (x1, y1) = ((x + ew).min(f64::from(w)), (y + eh).min(f64::from(h)));
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

/// Sends the operation to a window, or to the screen during a hand-over, expiring the elements it
/// may change.
fn act(s: &mut Server, on: On, op: AgentOp) -> ToolOutcome {
    let output = match on {
        On::Window(id) => return send(s, id, op),
        On::Screen(output) => output,
    };
    // Coordinates and element numbers are of the last look's output.
    let looked = s.screen_elements.take().map(|(output, ..)| output);
    let (verb, keys) = match op {
        AgentOp::Click { .. } => ("clicked", false),
        AgentOp::Move { .. } => ("hovered over", false),
        AgentOp::Scroll { .. } => ("scrolled", false),
        AgentOp::Drag { .. } => ("dragged in", false),
        AgentOp::Key { .. } => ("pressed keys in", true),
        AgentOp::Type { .. } => ("typed into", true),
    };
    let req = Request::AgentScreen {
        output: output.or(looked),
        op,
    };
    match (s.ipc)(req) {
        Ok(Ok(Response::AgentScreenDone(hit))) => ToolOutcome::text(match hit {
            ScreenHit::Window { id } => format!("{verb} window {id}"),
            ScreenHit::Layer { namespace } => format!("{verb} layer {}", one_line(&namespace)),
            ScreenHit::Overview => format!("{verb} overview"),
            ScreenHit::Nothing if keys => "nothing has the keyboard focus".into(),
            ScreenHit::Nothing => "nothing there".into(),
        }),
        Ok(Ok(other)) => ToolOutcome::error(format!("unexpected_reply: {other:?}")),
        Ok(Err(e)) => niri_error(e),
        Err(e) => unreachable(e),
    }
}

fn click_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let on = match on_arg(args) {
        Ok(on) => on,
        Err(e) => return e,
    };
    let (x, y) = match point(s, &on, args, "element", "x", "y") {
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
    act(
        s,
        on,
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
    let on = match on_arg(args) {
        Ok(on) => on,
        Err(e) => return e,
    };
    let (x, y) = match point(s, &on, args, "element", "x", "y") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let num = |k| args.get(k).and_then(Value::as_f64).unwrap_or(0.);
    act(
        s,
        on,
        AgentOp::Scroll {
            x,
            y,
            dx: num("dx"),
            dy: num("dy"),
        },
    )
}

fn drag_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let on = match on_arg(args) {
        Ok(on) => on,
        Err(e) => return e,
    };
    let (x0, y0) = match point(s, &on, args, "from_element", "x0", "y0") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let (x1, y1) = match point(s, &on, args, "to_element", "x1", "y1") {
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
    act(
        s,
        on,
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
    let on = match on_arg(args) {
        Ok(on) => on,
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
    act(s, on, AgentOp::Type { text: text.into() })
}

fn key_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let on = match on_arg(args) {
        Ok(on) => on,
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
    act(
        s,
        on,
        AgentOp::Key {
            chord: chord.into(),
            repeat,
        },
    )
}

fn hover_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let on = match on_arg(args) {
        Ok(on) => on,
        Err(e) => return e,
    };
    let (x, y) = match point(s, &on, args, "element", "x", "y") {
        Ok(p) => p,
        Err(e) => return e,
    };
    act(s, on, AgentOp::Move { x, y })
}

pub const TOOLS: [Tool; 7] = [
    Tool {
        name: "look",
        description: "Capture a window, or with screen: true the whole screen during a hand-over: returns the image and a numbered list of clickable elements. Use it first, and again after every action, before you act.",
        schema: r#"{"type":"object","properties":{"window":{"type":"integer","description":"window id"},"screen":{"type":"boolean","description":"capture the whole screen instead, during a hand-over"},"output":{"type":"string","description":"output for screen: true, default the agent cursor's, else the active one"},"max_edge":{"type":"integer","description":"scale the image so its longest edge is at most this many pixels"}}}"#,
        handler: look_tool,
    },
    Tool {
        name: "click",
        description: "Click in a window. Give an element number from the last look when there is one, otherwise x and y pixels of the last image. Look again afterwards.",
        schema: r#"{"type":"object","properties":{"window":{"type":"integer"},"screen":{"type":"boolean","description":"act on the whole screen during a hand-over, in pixels of the last screen look"},"output":{"type":"string","description":"output for screen: true, default the agent cursor's, else the active one"},"element":{"type":"integer","description":"element number from the last look"},"x":{"type":"number"},"y":{"type":"number"},"button":{"type":"string","enum":["left","right"]},"count":{"type":"integer","description":"1 to 3, 2 is a double click"},"mods":{"type":"array","items":{"type":"string","enum":["ctrl","shift","alt","super"]}}}}"#,
        handler: click_tool,
    },
    Tool {
        name: "hover",
        description: "Move the agent cursor over an element number from the last look, or x and y pixels of the last image, without clicking. Use it for tooltips and hover menus. Look again afterwards.",
        schema: r#"{"type":"object","properties":{"window":{"type":"integer"},"screen":{"type":"boolean","description":"act on the whole screen during a hand-over, in pixels of the last screen look"},"output":{"type":"string","description":"output for screen: true, default the agent cursor's, else the active one"},"element":{"type":"integer","description":"element number from the last look"},"x":{"type":"number"},"y":{"type":"number"}}}"#,
        handler: hover_tool,
    },
    Tool {
        name: "type",
        description: "Type text into the focused field of a window. Click the field first. Use `key` for Enter, Tab and shortcuts.",
        schema: r#"{"type":"object","properties":{"window":{"type":"integer"},"screen":{"type":"boolean","description":"send to whatever has the keyboard focus, during a hand-over"},"text":{"type":"string"}},"required":["text"]}"#,
        handler: type_tool,
    },
    Tool {
        name: "key",
        description: "Press a key or chord such as Return, Escape or ctrl+l in a window. Use it for Enter, Tab and shortcuts.",
        schema: r#"{"type":"object","properties":{"window":{"type":"integer"},"screen":{"type":"boolean","description":"send to whatever has the keyboard focus, during a hand-over"},"chord":{"type":"string"},"repeat":{"type":"integer","description":"times to press, default 1"}},"required":["chord"]}"#,
        handler: key_tool,
    },
    Tool {
        name: "scroll",
        description: "Scroll at an element or at x and y pixels of the last image. Positive dy scrolls down. Look again afterwards.",
        schema: r#"{"type":"object","properties":{"window":{"type":"integer"},"screen":{"type":"boolean","description":"act on the whole screen during a hand-over, in pixels of the last screen look"},"output":{"type":"string","description":"output for screen: true, default the agent cursor's, else the active one"},"element":{"type":"integer"},"x":{"type":"number"},"y":{"type":"number"},"dx":{"type":"number"},"dy":{"type":"number"}}}"#,
        handler: scroll_tool,
    },
    Tool {
        name: "drag",
        description: "Drag from one point to another, each given as an element number or as x and y pixels of the last image. Use it for sliders, selections and moving items.",
        schema: r#"{"type":"object","properties":{"window":{"type":"integer"},"screen":{"type":"boolean","description":"act on the whole screen during a hand-over, in pixels of the last screen look"},"output":{"type":"string","description":"output for screen: true, default the agent cursor's, else the active one"},"from_element":{"type":"integer"},"x0":{"type":"number"},"y0":{"type":"number"},"to_element":{"type":"integer"},"x1":{"type":"number"},"y1":{"type":"number"},"steps":{"type":"integer","description":"intermediate motion steps, 1 to 1000, default 10"}}}"#,
        handler: drag_tool,
    },
];

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    use niri_ipc::{AgentDone, ScreenHit, Window, WindowCapture};
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
        server_answering(move |r| match r {
            Request::CaptureWindow { id, path, .. } => {
                std::fs::write(path, b"\x89PNGfake").unwrap();
                Ok(Response::WindowCapture(cap(*id, path)))
            }
            Request::Windows => Ok(Response::Windows(vec![win(7, 42)])),
            _ => agent_reply.clone(),
        })
    }

    /// A server whose IPC requests `answer` answers, logged.
    fn server_answering(
        mut answer: impl FnMut(&Request) -> Result<Response, String> + 'static,
    ) -> (Server, Log, PathBuf) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("niri-mcp-act-{}-{n}", std::process::id()));
        let log: Log = Rc::default();
        let l = log.clone();
        let ipc: Ipc = Box::new(move |r| {
            let reply = answer(&r);
            l.borrow_mut().push(r);
            Ok(reply)
        });
        let mut s = Server::new(ipc);
        s.profile_base = Some(dir.clone());
        s.read_elements = fake_elements;
        (s, log, dir)
    }

    fn titled(id: u64, pid: i32, title: &str) -> Window {
        let mut w = win(id, pid);
        w.title = Some(title.into());
        w
    }

    /// A screen window at `rect` (image px) whose surface origin is the rect's corner.
    fn screen_win(id: u64, rect: [f64; 4], popups: Value) -> Value {
        json!({
            "id": id, "rect": rect, "surface_origin": [rect[0], rect[1]],
            "geometry_loc": [0.0, 0.0], "window_size": [1, 1], "popups": popups,
        })
    }

    /// A server whose output DP-1 (400x300 image, scale 0.5) shows `windows`, topmost first,
    /// windows 7 ("T7") and 8 ("T8") exist, and screen ops hit window 8.
    fn screen_server(windows: Value, overview_open: bool) -> (Server, Log, PathBuf) {
        let (mut s, log, dir) = server_answering(move |r| match r {
            Request::CaptureOutput { path, .. } => {
                std::fs::write(path, b"\x89PNGfake").unwrap();
                Ok(Response::OutputCapture(
                    serde_json::from_value(json!({
                        "path": path, "width": 400, "height": 300, "scale": 0.5,
                        "output": "DP-1", "overview_open": overview_open,
                        "windows": windows, "layers": [],
                    }))
                    .unwrap(),
                ))
            }
            Request::Windows => Ok(Response::Windows(vec![
                titled(7, 42, "T7"),
                titled(8, 43, "T8"),
            ])),
            _ => Ok(Response::AgentScreenDone(ScreenHit::Window { id: 8 })),
        });
        s.read_elements = screen_elements;
        (s, log, dir)
    }

    /// Buttons "a", "b" and "c" of 20x10 logical px at x 10, 60 and 200, y 10 from the window's
    /// surface origin, named after the window and placed in the capture as `read_elements` does.
    fn screen_elements(_: u32, title: &str, _: bool, c: &WindowCapture) -> Elements {
        let s = c.scale;
        let el = |x: f64, n: &str| Element {
            role: "button".into(),
            name: format!("{title} {n}"),
            states: vec![],
            rect: (
                (x - c.origin.0) * s,
                (10. - c.origin.1) * s,
                20. * s,
                10. * s,
            ),
        };
        Elements {
            list: vec![el(10., "a"), el(60., "b"), el(200., "c")],
            cut: false,
            note: None,
        }
    }

    fn element_lines(o: &ToolOutcome) -> Vec<&str> {
        text(o)
            .lines()
            .filter(|l| l.starts_with(|c: char| c.is_ascii_digit()))
            .collect()
    }

    fn screen_ops(log: &Log) -> Vec<(Option<String>, AgentOp)> {
        log.borrow()
            .iter()
            .filter_map(|r| match r {
                Request::AgentScreen { output, op } => Some((output.clone(), op.clone())),
                _ => None,
            })
            .collect()
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
    fn hover_sends_move() {
        let (mut s, log, dir) = server(done());
        look_tool(&mut s, &json!({"window": 7}));
        let o = hover_tool(&mut s, &json!({"window": 7, "element": 2}));
        assert_eq!(text(&o), "ok (capture_age_ms=12)");
        assert!(!hover_tool(&mut s, &json!({"window": 7, "x": 3, "y": 4})).is_error);
        assert!(matches!(
            &agent_ops(&log)[..],
            [AgentOp::Move { x: x0, y: y0 }, AgentOp::Move { x: 3., y: 4. }]
                if *x0 == 110. && *y0 == 15.
        ));
        assert!(hover_tool(&mut s, &json!({"window": 7})).is_error);
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

    #[test]
    fn screen_look_merges_windows() {
        let (mut s, log, dir) = screen_server(
            json!([
                screen_win(7, [0., 0., 200., 300.], json!([])),
                screen_win(8, [200., 0., 200., 300.], json!([])),
            ]),
            false,
        );
        let o = look_tool(&mut s, &json!({"screen": true}));
        assert!(!o.is_error, "{}", text(&o));
        assert!(matches!(&o.content[0], Content::Image(p) if p == b"\x89PNGfake"));
        // Each window's logical element places land at its surface origin, at scale 0.5.
        assert_eq!(
            text(&o).lines().collect::<Vec<_>>(),
            [
                "screen DP-1, 400x300 px, scale=0.5",
                "window 7 agent-foot \"T7\"",
                "1 button \"T7 a\" [] 5,5 10x5",
                "2 button \"T7 b\" [] 30,5 10x5",
                "3 button \"T7 c\" [] 100,5 10x5",
                "window 8 agent-foot \"T8\"",
                "4 button \"T8 a\" [] 205,5 10x5",
                "5 button \"T8 b\" [] 230,5 10x5",
                "6 button \"T8 c\" [] 300,5 10x5",
            ]
        );
        let Request::CaptureOutput { output, path, .. } = &log.borrow()[0] else {
            panic!()
        };
        assert_eq!(*output, None);
        assert!(path.starts_with(&dir.display().to_string()));
        assert!(!std::path::Path::new(path).exists());
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn screen_look_drops_covered_elements() {
        // Window 7 covers x 0-20 and its popup x 30-82 (with the shadow allowance) of window 8.
        let popup = json!([{"x": 30., "y": 0., "width": 20., "height": 20.}]);
        let (mut s, _, dir) = screen_server(
            json!([
                screen_win(7, [0., 0., 20., 100.], popup),
                screen_win(8, [0., 0., 400., 300.], json!([])),
            ]),
            false,
        );
        let o = look_tool(&mut s, &json!({"screen": true}));
        // 7's "c" is off its tile and popup; 8's "a" and "b" are under 7's tile and popup.
        assert_eq!(
            element_lines(&o),
            [
                "1 button \"T7 a\" [] 5,5 10x5",
                "2 button \"T7 b\" [] 30,5 10x5",
                "3 button \"T8 c\" [] 100,5 10x5",
            ]
        );
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn screen_look_in_overview_has_no_elements() {
        let (mut s, log, dir) = screen_server(
            json!([screen_win(7, [0., 0., 200., 300.], json!([]))]),
            true,
        );
        s.read_elements = |_, _, _, _| panic!("no tree is read in the overview");
        let o = look_tool(&mut s, &json!({"screen": true, "output": "DP-1"}));
        assert!(!o.is_error, "{}", text(&o));
        assert!(text(&o).contains("elements are unavailable in the overview; click by coordinates"));
        assert!(element_lines(&o).is_empty());
        let o = click_tool(&mut s, &json!({"screen": true, "element": 1}));
        assert_eq!(text(&o), "bad_args: no element 1 in the last look");
        assert!(screen_ops(&log).is_empty());
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn screen_click_element_sends_agent_screen() {
        let (mut s, log, dir) = screen_server(
            json!([
                screen_win(7, [0., 0., 200., 300.], json!([])),
                screen_win(8, [200., 0., 200., 300.], json!([])),
            ]),
            false,
        );
        look_tool(&mut s, &json!({"screen": true}));
        let o = click_tool(&mut s, &json!({"screen": true, "element": 4}));
        assert_eq!(text(&o), "clicked window 8");
        // The centre of 8's "a" in screen px, on the output the look captured.
        assert!(matches!(
            &screen_ops(&log)[..],
            [(Some(out), AgentOp::Click { x, y, button: AgentButton::Left, count: 1, .. })]
                if out == "DP-1" && *x == 210. && *y == 7.5
        ));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn screen_elements_expire_after_op() {
        let (mut s, log, dir) = screen_server(
            json!([screen_win(7, [0., 0., 200., 300.], json!([]))]),
            false,
        );
        look_tool(&mut s, &json!({"screen": true}));
        assert!(!click_tool(&mut s, &json!({"screen": true, "element": 1})).is_error);
        let o = click_tool(&mut s, &json!({"screen": true, "element": 1}));
        assert!(text(&o).starts_with("stale_elements"), "{}", text(&o));
        // Keys change the screen too.
        look_tool(&mut s, &json!({"screen": true}));
        key_tool(&mut s, &json!({"screen": true, "chord": "Escape"}));
        let o = hover_tool(&mut s, &json!({"screen": true, "element": 1}));
        assert!(text(&o).starts_with("stale_elements"), "{}", text(&o));
        // A look of another output's list is not this one's.
        look_tool(&mut s, &json!({"screen": true}));
        let o = click_tool(
            &mut s,
            &json!({"screen": true, "output": "HDMI-A-1", "element": 1}),
        );
        assert!(text(&o).starts_with("stale_elements"), "{}", text(&o));
        assert_eq!(screen_ops(&log).len(), 2);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn window_and_screen_both_given_is_bad_args() {
        let (mut s, log, _) = server(done());
        let args = json!({
            "window": 7, "screen": true, "x": 1, "y": 1, "x0": 1, "y0": 1, "x1": 2, "y1": 2,
            "text": "a", "chord": "a",
        });
        for tool in &TOOLS {
            let o = (tool.handler)(&mut s, &args);
            assert!(
                o.is_error && text(&o).starts_with("bad_args"),
                "{}",
                tool.name
            );
        }
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn screen_ops_name_the_hit() {
        let mut hits = vec![
            ScreenHit::Nothing,
            ScreenHit::Nothing,
            ScreenHit::Overview,
            ScreenHit::Layer {
                namespace: "bar\n1".into(),
            },
            ScreenHit::Window { id: 12 },
        ];
        let (mut s, log, _) =
            server_answering(move |_| Ok(Response::AgentScreenDone(hits.pop().unwrap())));
        let at = json!({"screen": true, "x": 1, "y": 2});
        assert_eq!(text(&click_tool(&mut s, &at)), "clicked window 12");
        assert_eq!(text(&click_tool(&mut s, &at)), "clicked layer bar1");
        assert_eq!(text(&scroll_tool(&mut s, &at)), "scrolled overview");
        assert_eq!(text(&hover_tool(&mut s, &at)), "nothing there");
        let o = type_tool(&mut s, &json!({"screen": true, "text": "a"}));
        assert_eq!(text(&o), "nothing has the keyboard focus");
        // Without a look, coordinates go to niri's default output.
        assert!(matches!(
            &screen_ops(&log)[0],
            (None, AgentOp::Click { x, y, .. }) if *x == 1. && *y == 2.
        ));
    }
}
