use niri_config::Config;

use super::*;
use crate::ui::mru::WindowMru;

/// Opens one window on `ws_name`, which is declared hidden if `hidden` is set, and returns
/// whether the window MRU switcher offers it.
fn mru_offers_window_on(ws_name: &str, hidden: bool) -> bool {
    let hidden = if hidden { "hidden" } else { "" };
    let config = Config::parse_mem(&format!(
        "\
workspace \"{ws_name}\" {{
    {hidden}
}}

window-rule {{
    open-on-workspace \"{ws_name}\"
}}"
    ))
    .unwrap();

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    // Sanity check: the window really did open on the workspace under test.
    let niri = f.niri();
    let ws = niri
        .layout
        .workspaces()
        .find(|(_mon, _idx, ws)| ws.windows().next().is_some())
        .expect("the window must have opened somewhere")
        .2;
    assert_eq!(ws.name().map(String::as_str), Some(ws_name));

    !WindowMru::new(f.niri()).is_empty()
}

#[test]
fn mru_skips_windows_on_hidden_workspaces() {
    // A hidden workspace is out of normal navigation, so the switcher must not offer a way in.
    assert!(!mru_offers_window_on("scratch", true));

    // The same setup on a visible workspace does list the window, so the assert above is
    // testing the hidden flag and not a fixture that failed to open a window.
    assert!(mru_offers_window_on("scratch", false));
}
