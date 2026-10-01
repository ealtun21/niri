use niri_config::{Action, Config, WorkspaceReference};

use super::*;
use crate::layout::LayoutElement as _;
use crate::niri::GRAVE_WORKSPACE_NAME;
use crate::tests::client::ClientId;
use crate::ui::mru::WindowMru;
use crate::window::mapped::MappedId;

fn window_ids(f: &mut Fixture) -> Vec<MappedId> {
    f.niri().layout.windows().map(|(_mon, m)| m.id()).collect()
}

/// Opens and maps one window, returning its id.
fn open_window(f: &mut Fixture, client: ClientId) -> MappedId {
    let before = window_ids(f);

    let window = f.client(client).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(client);

    let window = f.client(client).window(&surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(client);

    let mut new: Vec<_> = window_ids(f)
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect();
    assert_eq!(new.len(), 1, "exactly one window must have mapped");
    new.pop().unwrap()
}

fn is_graved(f: &mut Fixture, id: MappedId) -> bool {
    f.niri()
        .layout
        .windows()
        .find(|(_mon, m)| m.id() == id)
        .unwrap()
        .1
        .is_graved()
}

fn is_floating(f: &mut Fixture, id: MappedId) -> bool {
    f.niri()
        .layout
        .windows()
        .find(|(_mon, m)| m.id() == id)
        .unwrap()
        .1
        .is_floating()
}

/// Whether the window is stashed in the grave workspace (as opposed to summoned out of it).
fn in_grave(f: &mut Fixture, id: MappedId) -> bool {
    f.niri()
        .layout
        .workspaces()
        .find(|(_mon, _idx, ws)| ws.windows().any(|m| m.id() == id))
        .expect("the window must be on some workspace")
        .2
        .name()
        .map(String::as_str)
        == Some(GRAVE_WORKSPACE_NAME)
}

fn opacity(f: &mut Fixture, id: MappedId) -> Option<f32> {
    f.niri()
        .layout
        .windows()
        .find(|(_mon, m)| m.id() == id)
        .unwrap()
        .1
        .rules()
        .opacity
}

fn act(f: &mut Fixture, action: Action, client: ClientId) {
    f.niri_state().do_action(action, false);
    f.double_roundtrip(client);
}

#[test]
fn graved_state_is_sticky_across_summon_and_floating() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    let client = f.add_client();
    let id = open_window(&mut f, client);

    assert!(!is_graved(&mut f, id));

    // Graving hides the window in the grave workspace.
    act(&mut f, Action::GraveWindow, client);
    assert!(is_graved(&mut f, id));
    assert!(in_grave(&mut f, id));

    // Summoning brings it out as floating, and it stays graved -- this is the stickiness that
    // is-floating cannot express.
    act(&mut f, Action::ShowGravedWindows, client);
    assert!(!in_grave(&mut f, id));
    assert!(is_floating(&mut f, id));
    assert!(is_graved(&mut f, id));

    // Summoning again is a no-op rather than an error, so a held-down key is safe.
    act(&mut f, Action::ShowGravedWindows, client);
    assert!(!in_grave(&mut f, id));
    assert!(is_graved(&mut f, id));

    // Un-floating a summoned window by hand does not ungrave it either.
    act(&mut f, Action::MoveWindowToTiling, client);
    assert!(!is_floating(&mut f, id));
    assert!(is_graved(&mut f, id));

    // Hiding puts it back, still graved, and is likewise idempotent.
    act(&mut f, Action::HideGravedWindows, client);
    assert!(in_grave(&mut f, id));
    assert!(is_graved(&mut f, id));
    act(&mut f, Action::HideGravedWindows, client);
    assert!(in_grave(&mut f, id));
    assert!(is_graved(&mut f, id));

    // Summon it once more, so that ungraving has a floating window to un-float.
    act(&mut f, Action::ShowGravedWindows, client);
    assert!(is_floating(&mut f, id));

    // Only ungraving clears it, and it lands in the tiling layout out of the grave.
    act(&mut f, Action::UngraveWindowById(id.get()), client);
    assert!(!is_graved(&mut f, id));
    assert!(!in_grave(&mut f, id));
    assert!(!is_floating(&mut f, id));
}

#[test]
fn toggle_window_graved_round_trips() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    let client = f.add_client();
    let id = open_window(&mut f, client);

    act(&mut f, Action::ToggleWindowGraved, client);
    assert!(is_graved(&mut f, id));
    assert!(in_grave(&mut f, id));

    // The window is hidden, so it isn't focused: toggle it by id.
    act(&mut f, Action::ToggleWindowGravedById(id.get()), client);
    assert!(!is_graved(&mut f, id));
    assert!(!in_grave(&mut f, id));
}

#[test]
fn is_graved_matches_window_rules_and_floating_does_not_stand_in() {
    let config = Config::parse_mem(
        "\
window-rule {
    match is-graved=true
    opacity 0.5
}",
    )
    .unwrap();

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1280, 720));
    let client = f.add_client();
    let graved = open_window(&mut f, client);
    let floated = open_window(&mut f, client);

    assert_eq!(opacity(&mut f, graved), None);

    act(&mut f, Action::GraveWindowById(graved.get()), client);
    assert_eq!(opacity(&mut f, graved), Some(0.5));

    // The rule still holds once the window is summoned and visible.
    act(&mut f, Action::ShowGravedWindows, client);
    assert_eq!(opacity(&mut f, graved), Some(0.5));

    // A window the user floats by hand is not graved and must not match -- this is exactly what
    // an is-floating proxy got wrong.
    act(
        &mut f,
        Action::MoveWindowToFloatingById(floated.get()),
        client,
    );
    assert!(is_floating(&mut f, floated));
    assert_eq!(opacity(&mut f, floated), None);

    // Ungraving drops the rule again.
    act(&mut f, Action::UngraveWindowById(graved.get()), client);
    assert_eq!(opacity(&mut f, graved), None);
}

/// Every workspace, the windows on each, and the focused window: the whole observable state a
/// no-op toggle must leave untouched, including not conjuring a grave workspace.
type Placement = (Vec<(Option<String>, Vec<MappedId>)>, Option<MappedId>);

fn placement(f: &mut Fixture) -> Placement {
    let places = f
        .niri()
        .layout
        .workspaces()
        .map(|(_mon, _idx, ws)| (ws.name().cloned(), ws.windows().map(|m| m.id()).collect()))
        .collect();
    let focus = f.niri().layout.focus().map(|m| m.id());
    (places, focus)
}

#[test]
fn toggle_graved_windows_alternates_by_parity_and_is_a_no_op_when_nothing_is_graved() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    let client = f.add_client();
    let other = open_window(&mut f, client);
    let id = open_window(&mut f, client);

    // Nothing is graved yet: the toggle must change nothing at all. Focus the window that is
    // *not* the most recently opened one, so a toggle that focuses anything would show up.
    act(&mut f, Action::FocusColumnLeft, client);
    assert_eq!(f.niri().layout.focus().map(|m| m.id()), Some(other));
    let before = placement(&mut f);
    for _ in 0..5 {
        act(&mut f, Action::ToggleGravedWindows, client);
    }
    assert_eq!(placement(&mut f), before);

    act(&mut f, Action::GraveWindowById(id.get()), client);
    assert!(in_grave(&mut f, id));

    // From stashed, odd toggles leave it summoned and even ones leave it stashed. Direction is
    // recomputed from where the window is every time, so it never latches.
    for n in 1..=7 {
        act(&mut f, Action::ToggleGravedWindows, client);
        assert_eq!(in_grave(&mut f, id), n % 2 == 0, "after {n} toggle(s)");
        assert!(is_graved(&mut f, id), "after {n} toggle(s)");
    }

    // A burst with no dispatch in between must not interleave into a half-summoned state: three
    // toggles from stashed still land summoned.
    act(&mut f, Action::HideGravedWindows, client);
    assert!(in_grave(&mut f, id));
    for _ in 0..3 {
        f.niri_state().do_action(Action::ToggleGravedWindows, false);
    }
    f.double_roundtrip(client);
    assert!(!in_grave(&mut f, id));
    assert!(is_floating(&mut f, id));
    assert!(is_graved(&mut f, id));

    // The window that was never graved stayed put through all of it.
    assert!(!is_graved(&mut f, other));
    assert!(!in_grave(&mut f, other));
}

#[test]
fn mru_offers_summoned_graved_windows_first_and_hidden_ones_not_at_all() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    let client = f.add_client();
    let first = open_window(&mut f, client);
    let second = open_window(&mut f, client);

    // Baseline: plain MRU order, most recently focused first.
    let plain = WindowMru::new(f.niri()).window_ids();
    assert_eq!(plain, [second, first]);

    // In the grave, a window is not offered at all: it lives on a hidden workspace.
    act(&mut f, Action::GraveWindowById(first.get()), client);
    assert_eq!(WindowMru::new(f.niri()).window_ids(), [second]);

    // Summoned, it is offered first even though `second` was focused more recently.
    act(&mut f, Action::ShowGravedWindows, client);
    let order = WindowMru::new(f.niri()).window_ids();
    assert_eq!(order, [first, second]);

    // Ordering only: ungraving restores the plain order, with the same windows offered.
    act(&mut f, Action::UngraveWindowById(first.get()), client);
    let order = WindowMru::new(f.niri()).window_ids();
    assert_eq!(order.len(), 2);
    assert!(order.contains(&first) && order.contains(&second));
}

fn active_is_grave(f: &mut Fixture) -> bool {
    let ws = f.niri().layout.active_workspace().unwrap();
    ws.name().map(String::as_str) == Some(GRAVE_WORKSPACE_NAME)
}

#[test]
fn focusing_a_window_in_the_grave_brings_it_out_instead_of_entering_the_grave() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    let client = f.add_client();
    let other = open_window(&mut f, client);
    let id = open_window(&mut f, client);
    act(&mut f, Action::GraveWindowById(id.get()), client);

    // A taskbar click is focus-window --id: the window comes here, the grave stays shut.
    act(&mut f, Action::FocusWindow(id.get()), client);
    assert!(!active_is_grave(&mut f));
    assert!(!in_grave(&mut f, id));
    assert!(is_floating(&mut f, id));
    assert!(is_graved(&mut f, id));
    assert_eq!(f.niri().layout.focus().map(|m| m.id()), Some(id));

    // The grave can't be entered by name or by going back to it either.
    for action in [
        Action::FocusWorkspace(WorkspaceReference::Name(GRAVE_WORKSPACE_NAME.into())),
        Action::FocusWorkspacePrevious,
    ] {
        act(&mut f, action, client);
        assert!(!active_is_grave(&mut f));
    }

    // The peek keeps working afterwards: this is what used to get stuck.
    act(&mut f, Action::HideGravedWindows, client);
    assert!(in_grave(&mut f, id));
    act(&mut f, Action::ToggleGravedWindows, client);
    assert!(!in_grave(&mut f, id));

    // Ungraving lands it on a real workspace, never in the grave.
    act(&mut f, Action::HideGravedWindows, client);
    act(&mut f, Action::FocusWindow(id.get()), client);
    act(&mut f, Action::ToggleWindowGraved, client);
    assert!(!is_graved(&mut f, id));
    assert!(!in_grave(&mut f, id));
    assert!(!in_grave(&mut f, other));
}
