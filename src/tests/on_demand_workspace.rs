use niri_config::{Action, Config, WorkspaceReference};
use smithay::backend::input::InputTime;
use wayland_client::protocol::wl_surface::WlSurface;

use crate::tests::client::ClientId;
use crate::tests::fixture::Fixture;

const CONFIG: &str = r#"
workspace "main"
workspace "agent" {
    on-demand
}

window-rule {
    match app-id="^agent-"
    open-on-workspace "agent"
    open-focused false
}
"#;

fn set_up() -> (Fixture, ClientId) {
    let config = Config::parse_mem(CONFIG).unwrap();
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    (f, id)
}

fn open(f: &mut Fixture, id: ClientId, app_id: &str) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id(app_id);
    window.commit();
    f.roundtrip(id);
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    surface
}

fn close(f: &mut Fixture, id: ClientId, surface: &WlSurface) {
    let window = f.client(id).window(surface);
    window.attach_null();
    window.commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
}

fn act(f: &mut Fixture, action: Action) {
    f.niri_state()
        .do_action(action, false, InputTime::from_micros(0));
    f.niri_complete_animations();
}

/// Workspaces in order: name, "w" (unnamed with windows) or "_" (unnamed, empty); active starred.
fn layout(f: &mut Fixture) -> String {
    let niri = f.niri();
    let mut out = Vec::new();
    for (mon, idx, ws) in niri.layout.workspaces() {
        let active = mon.is_some_and(|mon| mon.active_workspace_idx() == idx);
        let label = match ws.name() {
            Some(name) => name.clone(),
            None if ws.has_windows() => String::from("w"),
            None => String::from("_"),
        };
        out.push(if active { format!("*{label}") } else { label });
    }
    out.join(" ")
}

#[test]
fn comes_with_its_first_window_and_goes_with_its_last() {
    let (mut f, id) = set_up();
    assert_eq!(layout(&mut f), "*main _", "not created at startup");

    let _owner = open(&mut f, id, "foot");
    act(&mut f, Action::FocusWorkspace(WorkspaceReference::Index(2)));
    let _owner2 = open(&mut f, id, "foot");
    act(&mut f, Action::FocusWorkspaceUp);
    assert_eq!(layout(&mut f), "*main w _");

    let a = open(&mut f, id, "agent-a");
    assert_eq!(
        layout(&mut f),
        "*main w agent _",
        "created above the last empty workspace"
    );
    let b = open(&mut f, id, "agent-b");
    assert_eq!(layout(&mut f), "*main w agent _", "reused while it exists");

    close(&mut f, id, &a);
    assert_eq!(layout(&mut f), "*main w agent _");
    close(&mut f, id, &b);
    assert_eq!(layout(&mut f), "*main w _", "removed with its last window");

    let _c = open(&mut f, id, "agent-c");
    assert_eq!(layout(&mut f), "*main w agent _", "created again");
}

#[test]
fn stays_while_active_like_an_unnamed_workspace() {
    let (mut f, id) = set_up();
    let _owner = open(&mut f, id, "foot");
    let a = open(&mut f, id, "agent-a");
    act(
        &mut f,
        Action::FocusWorkspace(WorkspaceReference::Name(String::from("agent"))),
    );
    close(&mut f, id, &a);
    assert_eq!(layout(&mut f), "main *agent _");
    act(&mut f, Action::FocusWorkspaceUp);
    assert_eq!(layout(&mut f), "*main _");
}
