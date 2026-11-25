//! `action` and `state` tools: the owner's desktop during a hand-over.

use std::fmt::Write as _;

use clap::Parser;
use niri_ipc::{Request, Response};
use serde_json::Value;

use super::act::{bad, niri_error, unreachable};
use super::{Server, Tool, ToolOutcome};

#[derive(Parser)]
struct ActionArgs {
    #[command(subcommand)]
    action: niri_ipc::Action,
}

fn action_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let Some(list) = args.get("args").and_then(Value::as_array) else {
        return bad("args must be an array of strings, e.g. [\"focus-workspace\", \"2\"]");
    };
    let Some(list) = list.iter().map(Value::as_str).collect::<Option<Vec<_>>>() else {
        return bad("args must be an array of strings");
    };
    let parsed = match ActionArgs::try_parse_from(["action"].into_iter().chain(list)) {
        Ok(p) => p,
        Err(e) => {
            let msg = e.to_string();
            return bad(msg.lines().next().unwrap_or_default());
        }
    };
    match (s.ipc)(Request::AgentAction {
        action: parsed.action,
    }) {
        Ok(Ok(_)) => ToolOutcome::text("done"),
        Ok(Err(e)) => niri_error(e),
        Err(e) => unreachable(e),
    }
}

/// One request, expecting the response `pick` extracts. `Ok(Err(_))` is niri's own refusal.
fn ask<T>(
    s: &mut Server,
    req: Request,
    pick: impl FnOnce(Response) -> Option<T>,
) -> Result<Result<T, String>, ToolOutcome> {
    match (s.ipc)(req) {
        Ok(Ok(r)) => match pick(r) {
            Some(v) => Ok(Ok(v)),
            None => Err(ToolOutcome::error("unexpected_reply: wrong response type")),
        },
        Ok(Err(e)) => Ok(Err(e)),
        Err(e) => Err(unreachable(e)),
    }
}

/// Names come from apps and the owner: keep them on one line.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

fn state_tool(s: &mut Server, args: &Value) -> ToolOutcome {
    let _ = args;
    macro_rules! get {
        ($req:expr, $variant:ident) => {
            match ask(s, $req, |r| match r {
                Response::$variant(v) => Some(v),
                _ => None,
            }) {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => return niri_error(e),
                Err(o) => return o,
            }
        };
    }
    let mut outputs: Vec<_> = get!(Request::Outputs, Outputs).into_values().collect();
    outputs.sort_by(|a, b| a.name.cmp(&b.name));
    let mut workspaces = get!(Request::Workspaces, Workspaces);
    workspaces.sort_by_key(|w| (w.output.clone(), w.idx));
    let focused = get!(Request::FocusedWindow, FocusedWindow);
    let overview = get!(Request::OverviewState, OverviewState);
    let layouts = get!(Request::KeyboardLayouts, KeyboardLayouts);
    let agent = match ask(s, Request::AgentState, |r| match r {
        Response::AgentState(a) => Some(a),
        _ => None,
    }) {
        Ok(Ok(a)) if a.paused => "paused",
        Ok(Ok(a)) if a.handed_over => "handed over",
        Ok(Ok(_)) => "side by side",
        // An older niri cannot parse the request.
        Ok(Err(_)) => "unknown (log out and back in)",
        Err(o) => return o,
    };

    let focused_output = workspaces
        .iter()
        .find(|w| w.is_focused)
        .and_then(|w| w.output.as_deref());
    let mut t = String::new();
    for o in &outputs {
        let _ = write!(t, "output {}", clean(&o.name));
        if let Some(l) = &o.logical {
            let _ = write!(t, " {}x{} scale={}", l.width, l.height, l.scale);
        }
        if focused_output == Some(o.name.as_str()) {
            t.push_str(" focused");
        }
        t.push('\n');
    }
    for w in &workspaces {
        let _ = write!(t, "workspace {}", w.idx);
        if let Some(n) = &w.name {
            let _ = write!(t, " {:?}", clean(n));
        }
        let _ = write!(
            t,
            " output={}",
            clean(w.output.as_deref().unwrap_or("none"))
        );
        if w.is_active {
            t.push_str(" active");
        }
        if w.is_focused {
            t.push_str(" focused");
        }
        t.push('\n');
    }
    match focused {
        Some(w) => {
            let _ = writeln!(
                t,
                "focused window {} app_id={:?} title={:?}",
                w.id,
                clean(w.app_id.as_deref().unwrap_or("")),
                clean(w.title.as_deref().unwrap_or("")),
            );
        }
        None => t.push_str("focused window: none\n"),
    }
    let _ = writeln!(
        t,
        "overview: {}",
        if overview.is_open { "open" } else { "closed" }
    );
    let _ = writeln!(
        t,
        "layout: {}",
        layouts
            .names
            .get(usize::from(layouts.current_idx))
            .map_or("unknown", |n| n.as_str())
    );
    let _ = write!(t, "agent: {agent}");
    ToolOutcome::text(t)
}

pub const TOOLS: [Tool; 2] = [
    Tool {
        name: "action",
        description: "Run a niri action during a hand-over, with the arguments of `niri msg action` as a list, e.g. [\"focus-workspace\", \"2\"]. Refused unless the owner handed over; quit is never allowed. Use `state` to check.",
        schema: r#"{"type":"object","properties":{"args":{"type":"array","items":{"type":"string"},"description":"action name and arguments as for niri msg action"}},"required":["args"]}"#,
        handler: action_tool,
    },
    Tool {
        name: "state",
        description: "Summarise the desktop: outputs, workspaces, the focused window, overview, keyboard layout and the agent mode (handed over, side by side or paused).",
        schema: r#"{"type":"object","properties":{}}"#,
        handler: state_tool,
    },
];

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use niri_ipc::{Action, AgentStatus, WorkspaceReferenceArg};
    use serde_json::json;

    use super::*;
    use crate::mcp::{Content, Ipc};

    type Log = Rc<RefCell<Vec<Request>>>;

    fn server(reply: impl Fn(&Request) -> niri_ipc::Reply + 'static) -> (Server, Log) {
        let log: Log = Rc::default();
        let l = log.clone();
        let ipc: Ipc = Box::new(move |r| {
            let out = reply(&r);
            l.borrow_mut().push(r);
            Ok(out)
        });
        (Server::new(ipc), log)
    }

    fn text(o: &ToolOutcome) -> &str {
        match o.content.last().unwrap() {
            Content::Text(t) => t,
            Content::Image(_) => panic!(),
        }
    }

    #[test]
    fn action_parses_like_niri_msg() {
        let (mut s, log) = server(|_| Ok(Response::Handled));
        let o = action_tool(&mut s, &json!({"args": ["focus-workspace", "2"]}));
        assert_eq!(text(&o), "done");
        assert!(matches!(
            &log.borrow()[..],
            [Request::AgentAction {
                action: Action::FocusWorkspace {
                    reference: WorkspaceReferenceArg::Index(2)
                }
            }]
        ));
    }

    #[test]
    fn action_bad_args() {
        let (mut s, log) = server(|_| Ok(Response::Handled));
        let o = action_tool(&mut s, &json!({"args": ["no-such-action"]}));
        assert!(
            o.is_error && text(&o).starts_with("bad_args: "),
            "{}",
            text(&o)
        );
        assert!(!text(&o).contains('\n'));
        assert!(action_tool(&mut s, &json!({})).is_error);
        assert!(action_tool(&mut s, &json!({"args": [1]})).is_error);
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn action_reports_not_handed_over() {
        let (mut s, _) = server(|_| Err("not_handed_over".into()));
        let o = action_tool(&mut s, &json!({"args": ["focus-workspace-up"]}));
        assert!(o.is_error && text(&o).starts_with("not_handed_over"));
    }

    #[test]
    fn action_on_old_niri_is_niri_refused() {
        let (mut s, _) = server(|_| Err("error parsing request".into()));
        let o = action_tool(&mut s, &json!({"args": ["focus-workspace-up"]}));
        assert!(text(&o).starts_with("niri_refused") && text(&o).contains("log out"));
    }

    fn desk(agent: Option<AgentStatus>) -> impl Fn(&Request) -> niri_ipc::Reply {
        move |r| {
            Ok(match r {
                Request::Outputs => Response::Outputs(
                    serde_json::from_value(json!({"DP-1": {
                        "name": "DP-1", "make": "m", "model": "m", "serial": null,
                        "physical_size": null, "modes": [], "current_mode": null,
                        "is_custom_mode": false, "vrr_supported": false,
                        "vrr_enabled": false, "max_bpc": null,
                        "logical": {"x": 0, "y": 0, "width": 1920, "height": 1080,
                            "scale": 1.0, "transform": "Normal"},
                    }}))
                    .unwrap(),
                ),
                Request::Workspaces => Response::Workspaces(
                    serde_json::from_value(json!([
                        {"id": 1, "idx": 1, "name": null, "output": "DP-1",
                         "is_urgent": false, "is_active": true, "is_focused": true,
                         "active_window_id": null},
                        {"id": 2, "idx": 2, "name": "web", "output": "DP-1",
                         "is_urgent": false, "is_active": false, "is_focused": false,
                         "active_window_id": null},
                    ]))
                    .unwrap(),
                ),
                Request::FocusedWindow => Response::FocusedWindow(None),
                Request::OverviewState => Response::OverviewState(
                    serde_json::from_value(json!({"is_open": true})).unwrap(),
                ),
                Request::KeyboardLayouts => Response::KeyboardLayouts(
                    serde_json::from_value(json!({"names": ["English (US)"], "current_idx": 0}))
                        .unwrap(),
                ),
                Request::AgentState => match &agent {
                    Some(a) => Response::AgentState(a.clone()),
                    None => return Err("error parsing request".into()),
                },
                _ => Response::Handled,
            })
        }
    }

    #[test]
    fn state_summarises() {
        let a = AgentStatus {
            handed_over: true,
            ..Default::default()
        };
        let (mut s, _) = server(desk(Some(a)));
        let o = state_tool(&mut s, &json!({}));
        assert_eq!(
            text(&o),
            "output DP-1 1920x1080 scale=1 focused\n\
             workspace 1 output=DP-1 active focused\n\
             workspace 2 \"web\" output=DP-1\n\
             focused window: none\n\
             overview: open\n\
             layout: English (US)\n\
             agent: handed over"
        );
    }

    #[test]
    fn state_on_old_niri_says_unknown() {
        let (mut s, _) = server(desk(None));
        let o = state_tool(&mut s, &json!({}));
        assert!(!o.is_error);
        assert!(text(&o).ends_with("agent: unknown (log out and back in)"));
    }
}
