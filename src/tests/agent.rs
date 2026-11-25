use std::time::{Duration, Instant};

use async_channel::Receiver;
use insta::assert_snapshot;
use niri_config::Config;
use niri_ipc::{AgentButton, AgentDone, AgentError, AgentOp, AgentStatus, AgentWindow};
use smithay::backend::input::{InputEvent, InputTime, KeyState, Keycode};
use smithay::desktop::Window;
use smithay::input::keyboard::Layout;
use smithay::reexports::wayland_protocols::xdg::shell::client::xdg_popup::XdgPopup;
use smithay::utils::{Point, Serial, Size};
use wayland_client::protocol::wl_pointer::{Axis, ButtonState};

use super::client::{ClientId, PointerEvent};
use super::*;
use crate::agent::capture::{capture_popups, capture_precheck, settle_time};
use crate::agent::pointer::{self, capture_to_buffer};
use crate::agent::{
    capture_scale, check_target, keyboard, status, CaptureRecord, OpKind, DEFAULT_SETTLE,
    MAX_DRAG_STEPS, MAX_SETTLE, TYPE_CHUNK_CHARS, TYPE_CHUNK_INTERVAL,
};
use crate::layout::{ActivateWindow, LayoutElement as _};
use crate::niri::LockState;
use crate::tests::test_input_backend::{
    TestInputBackend, TestKeyboardKeyEvent, TestPointerAxisEvent, TestPointerButtonEvent,
    TestPointerMotionAbsoluteEvent,
};
use crate::window::mapped::MappedId;

fn set_up(config: Config) -> (Fixture, MappedId) {
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let mapped_id = f.niri().layout.windows().next().unwrap().1.id();
    (f, mapped_id)
}

fn set_up_blocked_out(target: &str) -> (Fixture, MappedId) {
    let config = format!(
        r#"
window-rule {{
    block-out-from "{target}"
}}
"#
    );
    set_up(Config::parse_mem(&config).unwrap())
}

#[test]
fn capture_scale_caps_long_edge() {
    assert!((capture_scale(2.0, Size::from((1920., 1080.)), 1568) - 1568. / 1920.).abs() < 1e-9);
}

#[test]
fn capture_scale_keeps_small_window() {
    assert_eq!(capture_scale(1.0, Size::from((800., 600.)), 1568), 1.0);
}

#[test]
fn capture_scale_keeps_hidpi_small() {
    assert_eq!(capture_scale(2.0, Size::from((500., 400.)), 1568), 2.0);
}

#[test]
fn capture_plain_window_is_ok() {
    let (mut f, id) = set_up(Config::default());
    assert_eq!(capture_precheck(f.niri(), id), Ok(()));
}

#[test]
fn capture_unknown_window_is_window_gone() {
    let (mut f, _) = set_up(Config::default());
    let unknown = MappedId::specific(u64::MAX);
    assert_eq!(
        capture_precheck(f.niri(), unknown),
        Err(AgentError::WindowGone)
    );
}

#[test]
fn capture_while_paused_is_paused() {
    let (mut f, id) = set_up(Config::default());
    f.niri().agent.paused = true;
    assert_eq!(capture_precheck(f.niri(), id), Err(AgentError::Paused));
}

#[test]
fn capture_blocked_out() {
    let (mut f, id) = set_up_blocked_out("screen-capture");
    assert_eq!(capture_precheck(f.niri(), id), Err(AgentError::BlockedOut));
}

#[test]
fn capture_blocked_out_from_screencast() {
    let (mut f, id) = set_up_blocked_out("screencast");
    assert_eq!(capture_precheck(f.niri(), id), Err(AgentError::BlockedOut));
}

#[test]
fn settle_is_capped() {
    assert_eq!(settle_time(None), DEFAULT_SETTLE);
    assert_eq!(settle_time(Some(300)), Duration::from_millis(300));
    assert_eq!(settle_time(Some(u32::MAX)), MAX_SETTLE);
}

type WlSurface = wayland_client::protocol::wl_surface::WlSurface;

fn map_window(f: &mut Fixture, id: ClientId, app_id: &str) -> WlSurface {
    map_window_with(f, id, app_id, |_, _| ())
}

/// Like `map_window`, running `before_map` between the initial commit and the first buffer.
fn map_window_with(
    f: &mut Fixture,
    id: ClientId,
    app_id: &str,
    before_map: impl FnOnce(&mut Fixture, &WlSurface),
) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id(app_id);
    window.commit();
    f.roundtrip(id);
    before_map(f, &surface);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

fn window_of(f: &mut Fixture, app_id: &str) -> Window {
    f.niri()
        .layout
        .windows()
        .find(|(_, m)| {
            crate::utils::with_toplevel_role(m.toplevel(), |r| r.app_id.as_deref() == Some(app_id))
        })
        .unwrap()
        .1
        .window
        .clone()
}

fn mapped_id(f: &mut Fixture, app_id: &str) -> MappedId {
    let w = window_of(f, app_id);
    f.niri()
        .layout
        .windows()
        .find(|(_, m)| m.window == w)
        .unwrap()
        .1
        .id()
}

/// Human window `a` (focused) and agent window `agent-b` on workspace 2, from another client.
fn two_clients() -> (Fixture, ClientId, WlSurface, ClientId, WlSurface) {
    two_clients_with(Config::default())
}

fn two_clients_with(config: Config) -> (Fixture, ClientId, WlSurface, ClientId, WlSurface) {
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));

    let human = f.add_client();
    let a = map_window(&mut f, human, "a");
    let agent = f.add_client();
    let b = map_window(&mut f, agent, "agent-b");

    let b_id = window_of(&mut f, "agent-b");
    f.niri()
        .layout
        .move_to_workspace(Some(&b_id), 1, ActivateWindow::No);
    let a_id = window_of(&mut f, "a");
    f.niri().layout.activate_window(&a_id);
    f.niri_state().update_keyboard_focus();
    f.double_roundtrip(human);
    f.double_roundtrip(agent);
    (f, human, a, agent, b)
}

#[test]
fn prefix_window_is_granted() {
    let (mut f, ..) = two_clients();
    let id = mapped_id(&mut f, "agent-b");
    assert!(check_target(f.niri(), id, OpKind::Keyboard).is_ok());
}

#[test]
fn plain_window_is_not_granted() {
    let (mut f, human, ..) = two_clients();
    map_window(&mut f, human, "b");
    let id = mapped_id(&mut f, "b");
    assert_eq!(
        check_target(f.niri(), id, OpKind::Keyboard).err(),
        Some(AgentError::NotGranted)
    );
}

#[test]
fn manual_grant_allows() {
    let (mut f, _, _, agent, _) = two_clients();
    map_window(&mut f, agent, "b");
    let id = mapped_id(&mut f, "b");
    let w = window_of(&mut f, "b");
    f.niri().agent.granted.insert(id, true);
    f.niri()
        .layout
        .move_to_workspace(Some(&w), 1, ActivateWindow::No);
    let a = window_of(&mut f, "a");
    f.niri().layout.activate_window(&a);
    f.niri_state().update_keyboard_focus();
    f.double_roundtrip(agent);
    assert!(check_target(f.niri(), id, OpKind::Keyboard).is_ok());
}

#[test]
fn paused_refuses() {
    let (mut f, ..) = two_clients();
    let id = mapped_id(&mut f, "agent-b");
    f.niri().agent.paused = true;
    assert_eq!(
        check_target(f.niri(), id, OpKind::Keyboard).err(),
        Some(AgentError::Paused)
    );
}

#[test]
fn focused_window_is_human_focused() {
    let (mut f, ..) = two_clients();
    let id = mapped_id(&mut f, "a");
    f.niri().agent.granted.insert(id, true);
    assert_eq!(
        check_target(f.niri(), id, OpKind::Keyboard).err(),
        Some(AgentError::HumanFocused)
    );
}

#[test]
fn second_window_of_focused_client_is_same_client() {
    let (mut f, human, ..) = two_clients();
    map_window(&mut f, human, "agent-a2");
    let a_id = window_of(&mut f, "a");
    f.niri().layout.activate_window(&a_id);
    f.niri_state().update_keyboard_focus();
    f.double_roundtrip(human);
    let id = mapped_id(&mut f, "agent-a2");
    assert_eq!(
        check_target(f.niri(), id, OpKind::Keyboard).err(),
        Some(AgentError::SameClient)
    );
}

#[test]
fn pointer_without_capture_is_no_capture() {
    let (mut f, ..) = two_clients();
    let id = mapped_id(&mut f, "agent-b");
    assert_eq!(
        check_target(f.niri(), id, OpKind::Pointer).err(),
        Some(AgentError::NoCapture)
    );
}

fn record_for(f: &mut Fixture, id: MappedId) -> CaptureRecord {
    let (mapped, output) = f.niri().find_mapped_output_by_id(id).unwrap();
    let (window_size, output_scale, popup_count) = crate::agent::window_view(mapped, output);
    CaptureRecord {
        origin: (0, 0).into(),
        scale: 1.,
        size: (100, 100).into(),
        window_size,
        output_scale,
        popup_count,
        at: Instant::now(),
    }
}

#[test]
fn fresh_capture_allows_pointer() {
    let (mut f, ..) = two_clients();
    let id = mapped_id(&mut f, "agent-b");
    let record = record_for(&mut f, id);
    f.niri().agent.captures.insert(id, record);
    assert!(check_target(f.niri(), id, OpKind::Pointer).is_ok());
}

#[test]
fn resize_after_capture_is_stale() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = mapped_id(&mut f, "agent-b");
    let record = record_for(&mut f, id);
    f.niri().agent.captures.insert(id, record);

    let window = f.client(agent).window(&b);
    window.set_size(200, 150);
    window.commit();
    f.double_roundtrip(agent);
    assert_eq!(
        check_target(f.niri(), id, OpKind::Pointer).err(),
        Some(AgentError::StaleCapture)
    );
}

type AgentReply = Result<AgentDone, AgentError>;

fn start_type(f: &mut Fixture, id: MappedId, text: &str) -> Receiver<AgentReply> {
    let (tx, rx) = async_channel::bounded(1);
    keyboard::type_text(f.niri_state(), id, text, tx);
    rx
}

fn type_now(f: &mut Fixture, id: MappedId, text: &str) -> AgentReply {
    start_type(f, id, text).try_recv().unwrap()
}

fn key_now(f: &mut Fixture, id: MappedId, chord: &str, repeat: u16) -> AgentReply {
    let (tx, rx) = async_channel::bounded(1);
    keyboard::key(f.niri_state(), id, chord, repeat, tx);
    rx.try_recv().unwrap()
}

/// Keyboard events `surface` got since the last call, one per line.
fn events(f: &mut Fixture, client: ClientId, surface: &WlSurface) -> String {
    f.double_roundtrip(client);
    f.client(client)
        .state
        .format_recent_keyboard_events(surface)
}

fn owner_layout(f: &mut Fixture) -> u32 {
    let state = f.niri_state();
    let keyboard = state.niri.seat.get_keyboard().unwrap();
    keyboard.with_xkb_state(state, |ctx| ctx.xkb().lock().unwrap().active_layout().0)
}

fn set_owner_layout(f: &mut Fixture, layout: u32) {
    let state = f.niri_state();
    let keyboard = state.niri.seat.get_keyboard().unwrap();
    keyboard.with_xkb_state(state, |mut ctx| ctx.set_layout(Layout(layout)));
}

fn focus_for_owner(f: &mut Fixture, app_id: &str) {
    let w = window_of(f, app_id);
    f.niri().layout.activate_window(&w);
    f.niri_state().update_keyboard_focus();
}

/// Agent B's id, with the keyboard events from setting up drained.
fn agent_target(f: &mut Fixture, agent: ClientId, b: &WlSurface) -> MappedId {
    events(f, agent, b);
    mapped_id(f, "agent-b")
}

#[test]
fn type_reaches_target_only() {
    let (mut f, human, a, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);
    events(&mut f, human, &a);

    type_now(&mut f, id, "hi").unwrap();

    assert_snapshot!(events(&mut f, agent, &b), @"
    enter: []
    modifiers: depressed=0, latched=0, locked=0, group=0
    key pressed: 35
    key released: 35
    modifiers: depressed=0, latched=0, locked=0, group=0
    key pressed: 23
    key released: 23
    modifiers: depressed=0, latched=0, locked=0, group=0
    ");
    assert_eq!(events(&mut f, human, &a), "");
    let a_surface = window_of(&mut f, "a")
        .toplevel()
        .unwrap()
        .wl_surface()
        .clone();
    assert_eq!(f.niri().keyboard_focus.surface(), Some(&a_surface));
    let keyboard = f.niri().seat.get_keyboard().unwrap();
    assert_eq!(keyboard.current_focus(), Some(a_surface));
}

#[test]
fn sticky_across_calls() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);

    type_now(&mut f, id, "a").unwrap();
    type_now(&mut f, id, "b").unwrap();

    let events = events(&mut f, agent, &b);
    assert_eq!(events.matches("enter").count(), 1, "{events}");
    assert_eq!(events.matches("leave").count(), 0, "{events}");
}

#[test]
fn owner_focusing_sticky_client_leaves_first() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);

    type_now(&mut f, id, "a").unwrap();
    focus_for_owner(&mut f, "agent-b");

    let events = events(&mut f, agent, &b);
    let focus_changes: Vec<_> = events
        .lines()
        .filter(|l| l.starts_with("enter") || l.starts_with("leave"))
        .collect();
    assert_eq!(
        focus_changes,
        ["enter: []", "leave", "enter: []"],
        "{events}"
    );
}

fn us_tr() -> Config {
    let config = r#"
input {
    keyboard {
        xkb {
            layout "us,tr"
        }
    }
}
"#;
    Config::parse_mem(config).unwrap()
}

#[test]
fn turkish_char_uses_group_1() {
    let (mut f, _, _, agent, b) = two_clients_with(us_tr());
    let id = agent_target(&mut f, agent, &b);

    type_now(&mut f, id, "ğ").unwrap();

    assert_snapshot!(events(&mut f, agent, &b), @"
    enter: []
    modifiers: depressed=0, latched=0, locked=0, group=1
    key pressed: 26
    key released: 26
    modifiers: depressed=0, latched=0, locked=0, group=1
    ");
    assert_eq!(owner_layout(&mut f), 0);
}

#[test]
fn ascii_while_owner_on_tr_layout() {
    let (mut f, _, _, agent, b) = two_clients_with(us_tr());
    let id = agent_target(&mut f, agent, &b);
    set_owner_layout(&mut f, 1);

    type_now(&mut f, id, "a").unwrap();

    assert_snapshot!(events(&mut f, agent, &b), @"
    enter: []
    modifiers: depressed=0, latched=0, locked=0, group=0
    key pressed: 30
    key released: 30
    modifiers: depressed=0, latched=0, locked=0, group=0
    ");
    assert_eq!(owner_layout(&mut f), 1);
}

#[test]
fn unmappable_char_sends_nothing() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);

    let err = type_now(&mut f, id, "a日").unwrap_err();

    assert_eq!(err, AgentError::UnmappableChar('日'));
    assert_eq!(events(&mut f, agent, &b), "");
}

#[test]
fn empty_type_sends_nothing() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);

    type_now(&mut f, id, "").unwrap();

    assert_eq!(events(&mut f, agent, &b), "");
    // No burst, so the handoff guard stays off.
    assert!(!f.niri().agent.agent_acted_last());
}

#[test]
fn paste_chord_refused() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);

    for chord in ["ctrl+v", "ctrl+shift+v", "shift+Insert", "Ctrl+V"] {
        let err = key_now(&mut f, id, chord, 1).unwrap_err();
        assert_eq!(err, AgentError::PasteUnsupported, "{chord}");
    }
    assert_eq!(events(&mut f, agent, &b), "");
}

#[test]
fn key_chord_with_repeat() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);

    key_now(&mut f, id, "ctrl+l", 2).unwrap();
    assert_snapshot!(events(&mut f, agent, &b), @"
    enter: []
    modifiers: depressed=4, latched=0, locked=0, group=0
    key pressed: 38
    key released: 38
    modifiers: depressed=4, latched=0, locked=0, group=0
    key pressed: 38
    key released: 38
    modifiers: depressed=0, latched=0, locked=0, group=0
    ");

    for chord in ["ctrl+nosuchkey", "hyper+a", "ctrl+", "", "a\0"] {
        let err = key_now(&mut f, id, chord, 1).unwrap_err();
        assert_eq!(err, AgentError::BadChord(chord.to_owned()), "{chord:?}");
    }
}

#[test]
fn idle_release_sends_leave() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);
    type_now(&mut f, id, "a").unwrap();
    events(&mut f, agent, &b);

    f.niri()
        .agent
        .release_if_idle(Instant::now() + Duration::from_secs(9));
    assert_eq!(events(&mut f, agent, &b), "");

    f.niri()
        .agent
        .release_if_idle(Instant::now() + Duration::from_secs(11));
    assert_eq!(events(&mut f, agent, &b), "leave");
}

/// Runs the next `type` chunk: waits out its timer, then dispatches the server once.
///
/// Each chunk arms the next one's timer when it runs, so one call runs exactly one chunk.
fn step_chunk(f: &mut Fixture) {
    std::thread::sleep(TYPE_CHUNK_INTERVAL);
    f.state.server.dispatch();
}

#[test]
fn long_type_paces_chunks() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);

    let rx = start_type(&mut f, id, &"a".repeat(2500));
    // The next chunk waits for its timer, not just for the next dispatch.
    f.state.server.dispatch();
    assert!(rx.is_empty());
    step_chunk(&mut f);
    // Read without dispatching the server, or the test socket fills up.
    f.client(agent).dispatch();
    assert!(rx.is_empty());
    step_chunk(&mut f);
    assert!(rx.try_recv().unwrap().is_ok());

    let events = events(&mut f, agent, &b);
    assert_eq!(events.matches("key pressed: 30").count(), 2500);
}

#[test]
fn next_chunk_waits_for_interval() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);

    let rx = start_type(&mut f, id, &"a".repeat(2 * TYPE_CHUNK_CHARS));
    // A slow client gets 200 ms to read each chunk.
    std::thread::sleep(Duration::from_millis(100));
    f.state.server.dispatch();
    assert!(rx.is_empty());
    f.client(agent).dispatch();
    step_chunk(&mut f);
    assert!(rx.try_recv().unwrap().is_ok());
}

fn assert_interrupted_after_first_chunk(
    f: &mut Fixture,
    rx: &Receiver<AgentReply>,
    agent: ClientId,
    b: &WlSurface,
) {
    let err = rx.try_recv().unwrap().unwrap_err();
    assert_eq!(
        err,
        AgentError::Interrupted {
            error: Box::new(AgentError::HumanFocused),
            typed: 1000
        }
    );
    let events = events(f, agent, b);
    assert_eq!(events.matches("key pressed").count(), 1000);
    let (_, after_leave) = events.split_once("leave").unwrap();
    assert!(!after_leave.contains("key"), "{after_leave}");
}

#[test]
fn owner_focus_between_chunks_interrupts() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);

    let rx = start_type(&mut f, id, &"a".repeat(2500));
    focus_for_owner(&mut f, "agent-b");
    step_chunk(&mut f);

    assert_interrupted_after_first_chunk(&mut f, &rx, agent, &b);
}

#[test]
fn owner_focus_not_yet_refreshed_interrupts() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);

    let rx = start_type(&mut f, id, &"a".repeat(2500));
    // A focus bind moves the layout focus; niri.keyboard_focus catches up only in refresh, which
    // runs after the chunk in the same loop pass.
    let w = window_of(&mut f, "agent-b");
    f.niri().layout.activate_window(&w);
    step_chunk(&mut f);

    assert_interrupted_after_first_chunk(&mut f, &rx, agent, &b);
}

fn rec(origin: (i32, i32), scale: f64) -> CaptureRecord {
    CaptureRecord {
        origin: origin.into(),
        scale,
        size: (100, 100).into(),
        window_size: (100, 100).into(),
        output_scale: scale,
        popup_count: 0,
        at: Instant::now(),
    }
}

#[test]
fn capture_to_buffer_scale_1() {
    let rec = rec((0, 0), 1.0);
    assert_eq!(capture_to_buffer(10., 20., &rec), Point::from((10., 20.)));
}

#[test]
fn capture_to_buffer_fractional_with_csd_origin() {
    let rec = rec((-23, -23), 1.5);
    let p = capture_to_buffer(150., 75., &rec);
    assert!((p.x - 84.666_666).abs() < 0.5 && (p.y - 34.666_666).abs() < 0.5);
}

fn pointer_now(f: &mut Fixture, id: MappedId, op: AgentOp) -> AgentReply {
    let (tx, rx) = async_channel::bounded(1);
    pointer::pointer(f.niri_state(), id, &op, tx);
    rx.try_recv().unwrap()
}

fn pointer_events(f: &mut Fixture, client: ClientId, surface: &WlSurface) -> Vec<PointerEvent> {
    f.double_roundtrip(client);
    let surface = f.client(client).state.surfaces.get_mut(surface).unwrap();
    std::mem::take(&mut surface.pointer_events_received)
}

fn click(x: f64, y: f64) -> AgentOp {
    AgentOp::Click {
        x,
        y,
        button: AgentButton::Left,
        count: 1,
        mods: Vec::new(),
    }
}

fn captured_target(f: &mut Fixture, agent: ClientId, b: &WlSurface) -> MappedId {
    let id = agent_target(f, agent, b);
    let record = record_for(f, id);
    f.niri().agent.captures.insert(id, record);
    pointer_events(f, agent, b);
    id
}

#[test]
fn middle_click_is_refused_and_sends_nothing() {
    // Middle-click pastes the primary selection, which can hold the owner's text.
    let (mut f, _human, _a, agent, b) = two_clients();
    let id = captured_target(&mut f, agent, &b);
    pointer_events(&mut f, agent, &b);

    let op = AgentOp::Click {
        x: 10.,
        y: 10.,
        button: AgentButton::Middle,
        count: 1,
        mods: Vec::new(),
    };
    assert_eq!(
        pointer_now(&mut f, id, op).unwrap_err(),
        AgentError::PasteUnsupported
    );
    assert_eq!(pointer_events(&mut f, agent, &b), []);
}

#[test]
fn click_reaches_target_only() {
    let (mut f, human, a, agent, b) = two_clients();
    let id = captured_target(&mut f, agent, &b);
    pointer_events(&mut f, human, &a);
    let location = f.niri().seat.get_pointer().unwrap().current_location();

    pointer_now(&mut f, id, click(10., 10.)).unwrap();

    use PointerEvent::*;
    let button = |state| Button {
        serial: 0,
        button: 0x110,
        state,
    };
    let mut got = pointer_events(&mut f, agent, &b);
    for event in &mut got {
        if let Button { serial, .. } = event {
            *serial = 0;
        }
    }
    assert_eq!(
        got,
        [
            Enter { x: 10., y: 10. },
            Frame,
            Motion { x: 10., y: 10. },
            Frame,
            button(ButtonState::Pressed),
            Frame,
            button(ButtonState::Released),
            Frame,
        ]
    );
    assert_eq!(pointer_events(&mut f, human, &a), []);
    assert_eq!(
        f.niri().seat.get_pointer().unwrap().current_location(),
        location
    );
}

#[test]
fn scroll_sends_value120() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = captured_target(&mut f, agent, &b);

    let op = AgentOp::Scroll {
        x: 5.,
        y: 5.,
        dx: 0.,
        dy: 3.,
    };
    pointer_now(&mut f, id, op).unwrap();

    let got = pointer_events(&mut f, agent, &b);
    assert_eq!(
        got[4..8],
        [
            PointerEvent::AxisSource(wayland_client::protocol::wl_pointer::AxisSource::Wheel),
            PointerEvent::AxisValue120 {
                axis: Axis::VerticalScroll,
                value120: 360
            },
            PointerEvent::Axis {
                axis: Axis::VerticalScroll,
                value: 45.
            },
            PointerEvent::Frame
        ]
    );
}

#[test]
fn click_after_close_is_window_gone() {
    let (mut f, human, a, agent, b) = two_clients();
    let id = captured_target(&mut f, agent, &b);
    pointer_events(&mut f, human, &a);

    let window = f.client(agent).window(&b);
    window.attach_null();
    window.commit();
    f.double_roundtrip(agent);

    assert_eq!(
        pointer_now(&mut f, id, click(10., 10.)).err(),
        Some(AgentError::WindowGone)
    );
    assert_eq!(pointer_events(&mut f, human, &a), []);
}

#[test]
fn click_with_mods_wraps_clicks_in_modifiers() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = captured_target(&mut f, agent, &b);

    let op = AgentOp::Click {
        x: 10.,
        y: 10.,
        button: AgentButton::Left,
        count: 1,
        mods: vec![niri_ipc::AgentMod::Ctrl],
    };
    pointer_now(&mut f, id, op).unwrap();

    assert_snapshot!(events(&mut f, agent, &b), @"
    enter: []
    modifiers: depressed=4, latched=0, locked=0, group=0
    modifiers: depressed=0, latched=0, locked=0, group=0
    ");
}

#[test]
fn drag_presses_moves_and_releases() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = captured_target(&mut f, agent, &b);

    let op = AgentOp::Drag {
        x0: 10.,
        y0: 10.,
        x1: 30.,
        y1: 10.,
        steps: 2,
    };
    pointer_now(&mut f, id, op).unwrap();

    let motions: Vec<_> = pointer_events(&mut f, agent, &b)
        .into_iter()
        .filter_map(|e| match e {
            PointerEvent::Motion { x, .. } => Some(x),
            _ => None,
        })
        .collect();
    assert_eq!(motions, [10., 20., 30.]);
}

#[test]
fn drag_steps_are_capped() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = captured_target(&mut f, agent, &b);

    let op = AgentOp::Drag {
        x0: 10.,
        y0: 10.,
        x1: 30.,
        y1: 10.,
        steps: MAX_DRAG_STEPS + 500,
    };
    pointer_now(&mut f, id, op).unwrap();

    let motions = pointer_events(&mut f, agent, &b)
        .into_iter()
        .filter(|e| matches!(e, PointerEvent::Motion { .. }))
        .count();
    // The motion to the start point, then one per step.
    assert_eq!(motions, 1 + usize::from(MAX_DRAG_STEPS));
}

#[test]
fn click_serial_survives_drags_and_typing() {
    let (mut f, _, _, agent, b) = two_clients();
    let serial = agent_click_serial(&mut f, agent, &b);
    let id = mapped_id(&mut f, "agent-b");

    // Chromium may ask for its activation token long after the click.
    let drag = AgentOp::Drag {
        x0: 10.,
        y0: 10.,
        x1: 30.,
        y1: 10.,
        steps: MAX_DRAG_STEPS,
    };
    for _ in 0..3 {
        pointer_now(&mut f, id, drag.clone()).unwrap();
        pointer_events(&mut f, agent, &b);
    }
    type_now(&mut f, id, &"a".repeat(TYPE_CHUNK_CHARS)).unwrap();
    events(&mut f, agent, &b);

    assert!(f.niri().agent.is_agent_serial(Serial::from(serial)));
}

#[test]
fn click_with_mods_keeps_owner_layout_group() {
    let (mut f, _, _, agent, b) = two_clients_with(us_tr());
    let id = captured_target(&mut f, agent, &b);
    set_owner_layout(&mut f, 1);

    let op = AgentOp::Click {
        x: 10.,
        y: 10.,
        button: AgentButton::Left,
        count: 1,
        mods: vec![niri_ipc::AgentMod::Ctrl],
    };
    pointer_now(&mut f, id, op).unwrap();

    assert_snapshot!(events(&mut f, agent, &b), @"
    enter: []
    modifiers: depressed=4, latched=0, locked=0, group=1
    modifiers: depressed=0, latched=0, locked=0, group=1
    ");
    assert_eq!(owner_layout(&mut f), 1);
}

/// Like the owner's config: tokens with any serial are honored.
const HONOR_INVALID_SERIAL: &str = "debug { honor-xdg-activation-with-invalid-serial; }";

fn honor_invalid_serial() -> Config {
    Config::parse_mem(HONOR_INVALID_SERIAL).unwrap()
}

/// A serial no event ever had.
const BOGUS_SERIAL: u32 = u32::MAX;

fn activation_token(
    f: &mut Fixture,
    id: ClientId,
    serial: Option<u32>,
    surface: Option<&WlSurface>,
) -> String {
    let token = f.client(id).state.activation_token(serial, surface);
    f.roundtrip(id);
    token.get().unwrap().clone()
}

/// The client asks for a token with the serial and activates its surface with it.
fn self_activate(f: &mut Fixture, id: ClientId, serial: Option<u32>, surface: &WlSurface) {
    let token = activation_token(f, id, serial, Some(surface));
    f.client(id).state.activate(&token, surface);
    f.double_roundtrip(id);
}

fn focused_app_id(f: &mut Fixture) -> Option<String> {
    let mapped = f.niri().layout.focus()?;
    crate::utils::with_toplevel_role(mapped.toplevel(), |r| r.app_id.clone())
}

fn is_urgent(f: &mut Fixture, app_id: &str) -> bool {
    let id = mapped_id(f, app_id);
    f.niri().find_mapped_output_by_id(id).unwrap().0.is_urgent()
}

fn workspace_idx(f: &mut Fixture, app_id: &str) -> usize {
    let w = window_of(f, app_id);
    f.niri()
        .layout
        .workspaces()
        .find(|(_, _, ws)| ws.has_window(&w))
        .unwrap()
        .1
}

/// `two_clients` with the owner's activation flag, plus window `c` of a third client, which
/// the owner already runs.
fn three_clients() -> (Fixture, ClientId, WlSurface, ClientId, WlSurface) {
    three_clients_with(honor_invalid_serial())
}

fn three_clients_with(config: Config) -> (Fixture, ClientId, WlSurface, ClientId, WlSurface) {
    let (mut f, human, _, agent, b) = two_clients_with(config);
    let other = f.add_client();
    let c = map_window(&mut f, other, "c");
    focus_for_owner(&mut f, "a");
    f.double_roundtrip(human);
    f.double_roundtrip(other);
    (f, agent, b, other, c)
}

fn physical_key_event(f: &mut Fixture, state: KeyState) {
    // KEY_A.
    key_event(f, 38, state);
}

/// A key event (evdev code + 8) from the physical keyboard.
fn key_event(f: &mut Fixture, code: u32, state: KeyState) {
    f.niri_state()
        .process_input_event(InputEvent::<TestInputBackend>::Keyboard {
            event: TestKeyboardKeyEvent {
                time: InputTime::from_micros(0),
                code: Keycode::new(code),
                state,
                count: 1,
            },
        });
}

fn physical_key(f: &mut Fixture) {
    physical_key_event(f, KeyState::Pressed);
    physical_key_event(f, KeyState::Released);
}

/// The agent clicks B; returns the serial of the button press.
fn agent_click_serial(f: &mut Fixture, agent: ClientId, b: &WlSurface) -> u32 {
    let id = captured_target(f, agent, b);
    pointer_now(f, id, click(10., 10.)).unwrap();
    pointer_events(f, agent, b)
        .into_iter()
        .find_map(|e| match e {
            PointerEvent::Button { serial, .. } => Some(serial),
            _ => None,
        })
        .unwrap()
}

#[test]
fn agent_serial_token_rejected() {
    let (mut f, _, _, agent, b) = two_clients_with(honor_invalid_serial());
    let serial = agent_click_serial(&mut f, agent, &b);
    self_activate(&mut f, agent, Some(serial), &b);

    assert_eq!(focused_app_id(&mut f).as_deref(), Some("a"));
    assert!(!is_urgent(&mut f, "agent-b"));
}

#[test]
fn handoff_activation_is_urgent_only() {
    let (mut f, agent, b, other, c) = three_clients();
    let id = agent_target(&mut f, agent, &b);
    type_now(&mut f, id, "x").unwrap();

    self_activate(&mut f, other, Some(BOGUS_SERIAL), &c);

    assert!(is_urgent(&mut f, "c"));
    assert_eq!(focused_app_id(&mut f).as_deref(), Some("a"));
}

#[test]
fn handoff_guard_beats_focus_rule() {
    let config = format!(r#"{HONOR_INVALID_SERIAL}; window-rule {{ on-xdg-activate "focus"; }}"#);
    let (mut f, agent, b, other, c) = three_clients_with(Config::parse_mem(&config).unwrap());
    let id = agent_target(&mut f, agent, &b);
    type_now(&mut f, id, "x").unwrap();

    // Without a serial: also covers tokens that skip the serial checks.
    self_activate(&mut f, other, None, &c);

    assert!(is_urgent(&mut f, "c"));
    assert_eq!(focused_app_id(&mut f).as_deref(), Some("a"));
}

#[test]
fn virtual_input_keeps_handoff_guard() {
    let (mut f, agent, b, other, c) = three_clients();
    let id = agent_target(&mut f, agent, &b);
    type_now(&mut f, id, "x").unwrap();

    let pointer = f.client(other).state.virtual_pointer();
    pointer.axis(0, Axis::VerticalScroll, 10.);
    pointer.frame();
    f.roundtrip(other);
    self_activate(&mut f, other, Some(BOGUS_SERIAL), &c);

    assert_eq!(focused_app_id(&mut f).as_deref(), Some("a"));
}

#[test]
fn owner_input_restores_activation() {
    let (mut f, agent, b, other, c) = three_clients();
    let id = agent_target(&mut f, agent, &b);
    type_now(&mut f, id, "x").unwrap();

    physical_key(&mut f);
    self_activate(&mut f, other, Some(BOGUS_SERIAL), &c);

    assert_eq!(focused_app_id(&mut f).as_deref(), Some("c"));
}

#[test]
fn held_key_release_keeps_handoff_guard() {
    let (mut f, agent, b, other, c) = three_clients();
    physical_key_event(&mut f, KeyState::Pressed);
    let id = agent_target(&mut f, agent, &b);
    type_now(&mut f, id, "x").unwrap();

    // Releasing a key held since before the burst isn't the owner acting after the agent.
    physical_key_event(&mut f, KeyState::Released);
    self_activate(&mut f, other, Some(BOGUS_SERIAL), &c);

    assert!(is_urgent(&mut f, "c"));
    assert_eq!(focused_app_id(&mut f).as_deref(), Some("a"));
}

#[test]
fn spawn_token_after_agent_burst_is_urgent_only() {
    let (mut f, agent, b, other, c) = three_clients();
    let id = agent_target(&mut f, agent, &b);
    type_now(&mut f, id, "x").unwrap();

    // `spawn` hands the child a token niri makes itself, which never passes `token_created`.
    // kitty activates its new window with it after mapping (harness, 2026-11-20).
    let token = f
        .niri()
        .activation_state
        .create_external_token(None)
        .0
        .clone();
    f.client(other).state.activate(token.as_str(), &c);
    f.double_roundtrip(other);

    assert!(is_urgent(&mut f, "c"));
    assert_eq!(focused_app_id(&mut f).as_deref(), Some("a"));
}

#[test]
fn agent_window_spawn_token_is_urgent_only() {
    let (mut f, _, _, agent, b) = two_clients();
    // The owner acted last, so the handoff guard is off.
    physical_key(&mut f);

    // `niri msg action spawn` hands the agent app a token niri makes itself.
    let token = f
        .niri()
        .activation_state
        .create_external_token(None)
        .0
        .clone();
    f.client(agent).state.activate(token.as_str(), &b);
    f.double_roundtrip(agent);

    assert!(is_urgent(&mut f, "agent-b"));
    assert_eq!(focused_app_id(&mut f).as_deref(), Some("a"));

    // The owner can still focus it by keybind.
    let id = mapped_id(&mut f, "agent-b");
    f.niri_state().do_action(
        niri_config::Action::FocusWindow(id.get()),
        false,
        InputTime::from_micros(0),
    );
    assert_eq!(focused_app_id(&mut f).as_deref(), Some("agent-b"));
}

#[test]
fn agent_window_mapping_with_spawn_token_not_focused() {
    let (mut f, ..) = two_clients();
    physical_key(&mut f);
    let spawned = f.add_client();
    let token = f
        .niri()
        .activation_state
        .create_external_token(None)
        .0
        .clone();

    map_window_with(&mut f, spawned, "agent-c", |f, surface| {
        f.client(spawned).state.activate(token.as_str(), surface);
        f.roundtrip(spawned);
    });

    assert_eq!(focused_app_id(&mut f).as_deref(), Some("a"));
}

#[test]
fn guarded_token_on_unmapped_window_stays_urgent_only() {
    let (mut f, agent, b, other, _) = three_clients();
    let id = agent_target(&mut f, agent, &b);
    type_now(&mut f, id, "x").unwrap();

    map_window_with(&mut f, other, "c2", |f, surface| {
        let token = activation_token(f, other, Some(BOGUS_SERIAL), Some(surface));
        f.client(other).state.activate(&token, surface);
        f.roundtrip(other);
        // The owner acting after the token was made doesn't lift its guard.
        physical_key(f);
    });

    assert_eq!(focused_app_id(&mut f).as_deref(), Some("a"));
    assert!(is_urgent(&mut f, "c2"));
}

#[test]
fn new_window_from_agent_client_stays_put() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);
    type_now(&mut f, id, "x").unwrap();
    // Holds even when the owner acted after the agent, and for a window with a valid token.
    physical_key(&mut f);
    let token = activation_token(&mut f, agent, None, None);

    map_window_with(&mut f, agent, "b2", |f, surface| {
        f.client(agent).state.activate(&token, surface);
    });

    assert_eq!(focused_app_id(&mut f).as_deref(), Some("a"));
    assert_eq!(workspace_idx(&mut f, "b2"), 1);
}

#[test]
fn new_window_from_owner_focused_agent_client_focuses() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);
    type_now(&mut f, id, "x").unwrap();
    focus_for_owner(&mut f, "agent-b");
    physical_key(&mut f);

    map_window(&mut f, agent, "b2");

    assert_eq!(focused_app_id(&mut f).as_deref(), Some("b2"));
}

#[test]
fn handoff_new_window_not_focused() {
    let (mut f, agent, b, other, _) = three_clients();
    let id = agent_target(&mut f, agent, &b);
    type_now(&mut f, id, "x").unwrap();

    map_window(&mut f, other, "c2");

    assert_eq!(focused_app_id(&mut f).as_deref(), Some("a"));
}

#[test]
fn owner_launch_still_focuses() {
    let (mut f, _, _, other, _) = three_clients();
    map_window(&mut f, other, "c2");
    assert_eq!(focused_app_id(&mut f).as_deref(), Some("c2"));
}

/// B opens a popup that grabs with the serial.
fn open_popup(
    f: &mut Fixture,
    agent: ClientId,
    b: &WlSurface,
    serial: Option<u32>,
) -> (WlSurface, XdgPopup) {
    let parent = f.client(agent).window(b).xdg_surface.clone();
    let popup = f.client(agent).state.create_popup(&parent, serial);
    f.double_roundtrip(agent);
    popup
}

/// The agent clicks B, and B opens a menu that grabs with that click.
fn agent_popup(f: &mut Fixture, agent: ClientId, b: &WlSurface) -> (WlSurface, XdgPopup) {
    let serial = agent_click_serial(f, agent, b);
    open_popup(f, agent, b, Some(serial))
}

fn popup_done(f: &mut Fixture, agent: ClientId, popup: &XdgPopup) -> bool {
    f.double_roundtrip(agent);
    f.client(agent).state.popups_done.contains(popup)
}

fn physical_click(f: &mut Fixture) {
    use smithay::backend::input::ButtonState;
    for state in [ButtonState::Pressed, ButtonState::Released] {
        let event = TestPointerButtonEvent {
            button: 0x110,
            state,
        };
        f.niri_state()
            .process_input_event(InputEvent::<TestInputBackend>::PointerButton { event });
    }
}

fn physical_motion(f: &mut Fixture, x: f64, y: f64) {
    let event = TestPointerMotionAbsoluteEvent { x, y };
    f.niri_state()
        .process_input_event(InputEvent::<TestInputBackend>::PointerMotionAbsolute { event });
}

fn physical_scroll(f: &mut Fixture, dy: f64) {
    let event = TestPointerAxisEvent { dy };
    f.niri_state()
        .process_input_event(InputEvent::<TestInputBackend>::PointerAxis { event });
}

/// The compositor side of the window's surface.
fn server_surface(
    f: &mut Fixture,
    app_id: &str,
) -> smithay::reexports::wayland_server::protocol::wl_surface::WlSurface {
    window_of(f, app_id)
        .toplevel()
        .unwrap()
        .wl_surface()
        .clone()
}

#[test]
fn agent_popup_survives() {
    let (mut f, _, _, agent, b) = two_clients();
    let (_, popup) = agent_popup(&mut f, agent, &b);

    assert!(!popup_done(&mut f, agent, &popup));
    assert!(f.niri().popup_grab.is_none());
    let a = server_surface(&mut f, "a");
    assert_eq!(f.niri().keyboard_focus.surface(), Some(&a));
    let keyboard = f.niri().seat.get_keyboard().unwrap();
    assert_eq!(keyboard.current_focus(), Some(a));
}

#[test]
fn agent_keys_go_to_popup() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);
    type_now(&mut f, id, "a").unwrap();
    events(&mut f, agent, &b);
    let (menu, _) = agent_popup(&mut f, agent, &b);

    type_now(&mut f, id, "x").unwrap();

    assert_eq!(events(&mut f, agent, &b), "leave");
    assert_snapshot!(events(&mut f, agent, &menu), @"
    enter: []
    modifiers: depressed=0, latched=0, locked=0, group=0
    key pressed: 45
    key released: 45
    modifiers: depressed=0, latched=0, locked=0, group=0
    ");
}

#[test]
fn keys_return_to_window_after_popup_closes() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);
    let (menu, popup) = agent_popup(&mut f, agent, &b);
    type_now(&mut f, id, "x").unwrap();
    events(&mut f, agent, &menu);

    popup.destroy();
    f.double_roundtrip(agent);
    type_now(&mut f, id, "y").unwrap();

    assert_eq!(events(&mut f, agent, &menu), "leave");
    assert_snapshot!(events(&mut f, agent, &b), @"
    enter: []
    modifiers: depressed=0, latched=0, locked=0, group=0
    key pressed: 21
    key released: 21
    modifiers: depressed=0, latched=0, locked=0, group=0
    ");
}

/// Moves B next to A, on the owner's workspace, keeping A focused.
fn b_on_owner_workspace(f: &mut Fixture, human: ClientId) {
    let w = window_of(f, "agent-b");
    f.niri()
        .layout
        .move_to_workspace(Some(&w), 0, ActivateWindow::No);
    focus_for_owner(f, "a");
    f.double_roundtrip(human);
}

#[test]
fn owner_click_dismisses_visible_popup() {
    let (mut f, human, _, agent, b) = two_clients();
    b_on_owner_workspace(&mut f, human);
    let (_, popup) = agent_popup(&mut f, agent, &b);
    assert!(!popup_done(&mut f, agent, &popup));

    physical_click(&mut f);

    assert!(popup_done(&mut f, agent, &popup));
    let a = server_surface(&mut f, "a");
    assert_eq!(f.niri().keyboard_focus.surface(), Some(&a));
}

#[test]
fn owner_focusing_window_dismisses() {
    let (mut f, human, _, agent, b) = two_clients();
    b_on_owner_workspace(&mut f, human);
    let (_, popup) = agent_popup(&mut f, agent, &b);
    assert!(!popup_done(&mut f, agent, &popup));

    focus_for_owner(&mut f, "agent-b");

    assert!(popup_done(&mut f, agent, &popup));
}

#[test]
fn owner_click_keeps_hidden_popup() {
    let (mut f, _, _, agent, b) = two_clients();
    let (_, popup) = agent_popup(&mut f, agent, &b);

    physical_click(&mut f);

    assert!(!popup_done(&mut f, agent, &popup));
}

#[test]
fn owner_switch_to_workspace_dismisses() {
    let (mut f, _, _, agent, b) = two_clients();
    let (_, popup) = agent_popup(&mut f, agent, &b);
    assert!(!popup_done(&mut f, agent, &popup));

    f.niri().layout.switch_workspace(1);

    assert!(popup_done(&mut f, agent, &popup));
}

#[test]
fn overview_showing_popup_dismisses() {
    let (mut f, _, _, agent, b) = two_clients();
    let (_, popup) = agent_popup(&mut f, agent, &b);
    assert!(!popup_done(&mut f, agent, &popup));

    // The overview shows workspace 2 without focusing B.
    f.niri().layout.toggle_overview();
    f.niri_complete_animations();

    assert!(popup_done(&mut f, agent, &popup));
    let b = server_surface(&mut f, "agent-b");
    assert_ne!(f.niri().keyboard_focus.surface(), Some(&b));
}

#[test]
fn idle_release_dismisses_popups() {
    let (mut f, _, _, agent, b) = two_clients();
    let (_, popup) = agent_popup(&mut f, agent, &b);
    assert!(!popup_done(&mut f, agent, &popup));

    f.niri()
        .agent
        .release_if_idle(Instant::now() + Duration::from_secs(11));

    assert!(popup_done(&mut f, agent, &popup));
}

#[test]
fn non_agent_grab_still_dismissed_when_unfocused() {
    let (mut f, _, _, agent, b) = two_clients();
    let (_, popup) = open_popup(&mut f, agent, &b, Some(BOGUS_SERIAL));
    assert!(popup_done(&mut f, agent, &popup));
}

const OWNER_BINDS: &str = r#"
binds {
    Mod+Alt+A { toggle-agent-input; }
    Mod+Alt+G { toggle-window-agent-input; }
}
"#;

/// Presses and releases Mod+Alt+`key` (evdev code + 8) on the physical keyboard.
fn owner_bind(f: &mut Fixture, key: u32) {
    // Super_L and Alt_L.
    press_keys(f, &[133, 64, key]);
}

/// Presses the keys (evdev code + 8) in order on the physical keyboard, then releases them in
/// reverse.
fn press_keys(f: &mut Fixture, keys: &[u32]) {
    for (keys, state) in [
        (keys.to_vec(), KeyState::Pressed),
        (keys.iter().rev().copied().collect(), KeyState::Released),
    ] {
        for code in keys {
            key_event(f, code, state);
        }
    }
}

const KEY_A: u32 = 38;
const KEY_G: u32 = 42;

#[test]
fn owner_bind_config_parses() {
    let config = Config::parse_mem(&format!(
        "{OWNER_BINDS}\nwindow-rule {{ match is-agent-granted=true is-agent-active=false; }}"
    ))
    .unwrap();
    assert_eq!(config.binds.0.len(), 2);
    let rule = &config.window_rules[0];
    assert_eq!(rule.matches[0].is_agent_granted, Some(true));
    assert_eq!(rule.matches[0].is_agent_active, Some(false));
}

#[test]
fn agent_bind_allowed_rejects_virtual() {
    use niri_config::Action;

    use crate::input::agent_bind_allowed;
    assert!(!agent_bind_allowed(&Action::ToggleAgentInput, true));
    assert!(!agent_bind_allowed(&Action::ToggleWindowAgentInput, true));
    assert!(!agent_bind_allowed(&Action::ToggleAgentHandover, true));
    assert!(agent_bind_allowed(&Action::ToggleAgentHandover, false));
    assert!(agent_bind_allowed(&Action::ToggleAgentInput, false));
    assert!(agent_bind_allowed(&Action::Spawn(vec![]), true));
}

#[test]
fn physical_bind_pauses() {
    let (mut f, ..) = two_clients_with(Config::parse_mem(OWNER_BINDS).unwrap());
    owner_bind(&mut f, KEY_A);
    assert!(f.niri().agent.paused);
    owner_bind(&mut f, KEY_A);
    assert!(!f.niri().agent.paused);
}

#[test]
fn pause_mid_type_stops_and_releases() {
    let (mut f, _, _, agent, b) = two_clients_with(Config::parse_mem(OWNER_BINDS).unwrap());
    let (menu, popup) = agent_popup(&mut f, agent, &b);
    let id = agent_target(&mut f, agent, &b);

    // The chunks go to the open menu, which holds the shadow focus.
    let rx = start_type(&mut f, id, &"a".repeat(2500));
    owner_bind(&mut f, KEY_A);
    step_chunk(&mut f);

    assert_eq!(
        rx.try_recv().unwrap().unwrap_err(),
        AgentError::Interrupted {
            error: Box::new(AgentError::Paused),
            typed: 1000
        }
    );
    let events = events(&mut f, agent, &menu);
    assert_eq!(events.matches("key pressed").count(), 1000);
    let (_, after_leave) = events.split_once("leave").unwrap();
    assert!(!after_leave.contains("key"), "{after_leave}");
    assert!(popup_done(&mut f, agent, &popup));
}

#[test]
fn grant_bind_grants_focused() {
    let (mut f, human, ..) = two_clients_with(Config::parse_mem(OWNER_BINDS).unwrap());
    map_window(&mut f, human, "d");
    let id = mapped_id(&mut f, "d");
    focus_for_owner(&mut f, "d");
    assert_eq!(
        check_target(f.niri(), id, OpKind::Keyboard).err(),
        Some(AgentError::NotGranted)
    );

    owner_bind(&mut f, KEY_G);

    assert_eq!(
        check_target(f.niri(), id, OpKind::Keyboard).err(),
        Some(AgentError::HumanFocused)
    );
    owner_bind(&mut f, KEY_G);
    assert_eq!(
        check_target(f.niri(), id, OpKind::Keyboard).err(),
        Some(AgentError::NotGranted)
    );
}

#[test]
fn grant_bind_revokes_agent_window() {
    let (mut f, ..) = two_clients_with(Config::parse_mem(OWNER_BINDS).unwrap());
    let id = mapped_id(&mut f, "agent-b");

    focus_for_owner(&mut f, "agent-b");
    owner_bind(&mut f, KEY_G);
    focus_for_owner(&mut f, "a");
    assert_eq!(
        check_target(f.niri(), id, OpKind::Keyboard).err(),
        Some(AgentError::NotGranted)
    );

    focus_for_owner(&mut f, "agent-b");
    owner_bind(&mut f, KEY_G);
    focus_for_owner(&mut f, "a");
    assert!(check_target(f.niri(), id, OpKind::Keyboard).is_ok());
}

const OPACITY_RULE: &str = r#"
window-rule {
    match is-agent-granted=true
    opacity 0.5
}
"#;

fn opacity_of(f: &mut Fixture, app_id: &str) -> Option<f32> {
    let id = mapped_id(f, app_id);
    f.niri()
        .layout
        .windows()
        .find(|(_, m)| m.id() == id)
        .unwrap()
        .1
        .rules()
        .opacity
}

#[test]
fn matcher_is_agent_granted() {
    let (mut f, human, _, agent, _) = two_clients_with(Config::parse_mem(OPACITY_RULE).unwrap());
    assert_eq!(opacity_of(&mut f, "agent-b"), Some(0.5));
    assert_eq!(opacity_of(&mut f, "a"), None);

    // The keybind source follows too, and an app id change updates the flag.
    map_window(&mut f, human, "c");
    assert_eq!(opacity_of(&mut f, "c"), None);
    let c = mapped_id(&mut f, "c");
    f.niri().agent.granted.insert(c, true);
    f.niri_state().refresh_and_flush_clients();
    assert_eq!(opacity_of(&mut f, "c"), Some(0.5));
    f.niri().agent.granted.remove(&c);

    let surface = map_window(&mut f, agent, "plain");
    assert_eq!(opacity_of(&mut f, "plain"), None);
    f.client(agent).window(&surface).set_app_id("agent-plain");
    f.client(agent).window(&surface).commit();
    f.double_roundtrip(agent);
    assert_eq!(opacity_of(&mut f, "agent-plain"), Some(0.5));
}

#[test]
fn pause_clears_is_agent_granted() {
    let config = format!("{OWNER_BINDS}{OPACITY_RULE}");
    let (mut f, ..) = two_clients_with(Config::parse_mem(&config).unwrap());
    assert_eq!(opacity_of(&mut f, "agent-b"), Some(0.5));

    owner_bind(&mut f, KEY_A);
    f.niri_state().refresh_and_flush_clients();
    assert_eq!(opacity_of(&mut f, "agent-b"), None);

    owner_bind(&mut f, KEY_A);
    f.niri_state().refresh_and_flush_clients();
    assert_eq!(opacity_of(&mut f, "agent-b"), Some(0.5));
}

#[test]
fn matcher_is_agent_active_expires() {
    let config = "window-rule {\n match is-agent-active=true\n opacity 0.5\n}";
    let (mut f, _, _, agent, b) = two_clients_with(Config::parse_mem(config).unwrap());
    let id = agent_target(&mut f, agent, &b);
    assert_eq!(opacity_of(&mut f, "agent-b"), None);

    type_now(&mut f, id, "x").unwrap();
    f.niri_state().refresh_and_flush_clients();
    assert_eq!(opacity_of(&mut f, "agent-b"), Some(0.5));

    f.niri().agent.active_until.insert(id, Instant::now());
    f.niri_state().refresh_and_flush_clients();
    assert_eq!(opacity_of(&mut f, "agent-b"), None);
}

#[test]
fn capture_popups_reports_popup_offset() {
    let (mut f, _, _, agent, b) = two_clients();
    let parent = f.client(agent).window(&b).xdg_surface.clone();
    let (surface, _popup) = f
        .client(agent)
        .state
        .create_popup_at(&parent, None, (20, 30));
    f.double_roundtrip(agent);
    // Commit the popup's position and size.
    let (xdg_surface, serial) = f.client(agent).state.popup_configures.pop().unwrap();
    xdg_surface.ack_configure(serial);
    xdg_surface.set_window_geometry(0, 0, 10, 10);
    // Smithay only applies the acked popup state once the popup has a buffer.
    let state = &f.client(agent).state;
    let buffer = state
        .spbm
        .as_ref()
        .unwrap()
        .create_u32_rgba_buffer(0, 0, 0, 0, &state.qh, ());
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    f.double_roundtrip(agent);

    let id = mapped_id(&mut f, "agent-b");
    let (mapped, _) = f.niri().find_mapped_output_by_id(id).unwrap();
    let geo = mapped.window.geometry();
    let popups = capture_popups(mapped);
    assert_eq!(popups.len(), 1);
    // Anchor point (20, 30) minus half the 10x10 popup, relative to the window geometry.
    assert_eq!(popups[0].x, f64::from(geo.loc.x + 15));
    assert_eq!(popups[0].y, f64::from(geo.loc.y + 25));
    assert_eq!((popups[0].width, popups[0].height), (10., 10.));
}

/// Opens the overview and moves the owner's pointer over `id`'s tile, found by scanning the output.
/// A point of the 1920 × 1080 output where the window is under the pointer.
fn point_over(f: &mut Fixture, window: &Window) -> (f64, f64) {
    (0..1080 / 10)
        .flat_map(|j| (0..1920 / 10).map(move |i| (f64::from(i * 10 + 5), f64::from(j * 10 + 5))))
        .find(|&(x, y)| {
            f.niri()
                .contents_under((x, y).into())
                .window
                .is_some_and(|(w, _)| w == *window)
        })
        .expect("window is drawn")
}

fn hover_overview_tile(f: &mut Fixture, id: MappedId) {
    f.niri().layout.toggle_overview();
    f.niri_complete_animations();
    let window = f
        .niri()
        .find_mapped_output_by_id(id)
        .unwrap()
        .0
        .window
        .clone();
    let (x, y) = point_over(f, &window);
    physical_motion(f, x, y);
    assert!(f
        .niri()
        .pointer_contents
        .window
        .as_ref()
        .is_some_and(|(w, _)| *w == window));
}

#[test]
fn overview_hover_does_not_block_agent() {
    let (mut f, _human, _a, agent, b) = two_clients();
    let id = captured_target(&mut f, agent, &b);
    hover_overview_tile(&mut f, id);
    pointer_now(&mut f, id, click(10., 10.)).unwrap();
}

#[test]
fn overview_keyboard_focus_still_blocks() {
    let (mut f, ..) = two_clients();
    let id = mapped_id(&mut f, "agent-b");
    f.niri().agent.granted.insert(id, true);
    let window = window_of(&mut f, "agent-b");
    f.niri().layout.activate_window(&window);
    f.niri_state().update_keyboard_focus();
    f.niri().layout.toggle_overview();
    f.niri_complete_animations();
    assert_eq!(
        check_target(f.niri(), id, OpKind::Keyboard).err(),
        Some(AgentError::HumanFocused)
    );
}

#[test]
fn status_lists_granted_and_acting() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);
    let s = status(f.niri());
    assert_eq!(s.granted.len(), 1);
    assert_eq!(s.granted[0].id, id.get());
    assert_eq!(s.granted[0].app_id.as_deref(), Some("agent-b"));
    assert!(s.acting.is_empty());

    type_now(&mut f, id, "a").unwrap();
    assert_eq!(status(f.niri()).acting, [id.get()]);
}

#[test]
fn status_while_paused_grants_nothing() {
    let (mut f, ..) = two_clients();
    f.niri().agent.pause();
    let s = status(f.niri());
    assert!(s.paused);
    assert!(s.granted.is_empty());
}

#[test]
fn ipc_pause_pauses_and_releases_sticky() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = agent_target(&mut f, agent, &b);
    type_now(&mut f, id, "a").unwrap();
    events(&mut f, agent, &b);

    f.niri().agent.pause();

    assert!(f.niri().agent.paused);
    assert!(events(&mut f, agent, &b).contains("leave"));
}

#[test]
fn ipc_pause_never_unpauses() {
    let (mut f, ..) = two_clients();
    f.niri().agent.pause();
    f.niri().agent.pause();
    assert!(f.niri().agent.paused);
}

#[test]
fn agent_stream_sends_initial_then_changes() {
    let (mut f, ..) = two_clients();
    let (tx, rx) = async_channel::bounded::<AgentStatus>(64);
    let (disconnect, _keep) = async_channel::bounded(1);
    f.niri_state().ipc_add_agent_stream(tx, disconnect);
    assert_eq!(rx.try_recv().unwrap(), status(f.niri()));
    assert!(rx.try_recv().is_err());

    let id = mapped_id(&mut f, "a");
    f.niri().agent.granted.insert(id, true);
    f.niri_state().ipc_refresh_agent();
    let line = rx.try_recv().unwrap();
    assert!(line.granted.iter().any(|w: &AgentWindow| w.id == id.get()));
    assert!(rx.try_recv().is_err());

    f.niri_state().ipc_refresh_agent();
    assert!(rx.try_recv().is_err());
}

const HANDOVER_BINDS: &str = r#"
binds {
    Mod+Alt+D { toggle-agent-handover; }
    Mod+1 { focus-workspace 1; }
}
"#;

fn handover_config() -> Config {
    Config::parse_mem(HANDOVER_BINDS).unwrap()
}

const KEY_D: u32 = 40;

/// Asks to lock the session from a new client; the lock then waits for lock surfaces.
fn start_lock(f: &mut Fixture) {
    let id = f.add_client();
    f.client(id).lock_session();
    f.roundtrip(id);
    assert!(matches!(
        f.niri().lock_state,
        LockState::WaitingForSurfaces { .. }
    ));
}

/// Removes the only output, which lets a waiting lock go through.
fn finish_lock(f: &mut Fixture) {
    let output = f.niri_output(1);
    f.niri().remove_output(&output);
    assert!(f.niri().is_locked());
}

#[test]
fn handover_bind_toggles() {
    let (mut f, ..) = two_clients_with(handover_config());
    owner_bind(&mut f, KEY_D);
    assert!(f.niri().agent.handed_over);
    owner_bind(&mut f, KEY_D);
    assert!(!f.niri().agent.handed_over);
}

#[test]
fn handover_unpauses() {
    let (mut f, ..) = two_clients_with(handover_config());
    f.niri().agent.pause();
    owner_bind(&mut f, KEY_D);
    assert!(f.niri().agent.handed_over);
    assert!(!f.niri().agent.paused);
}

#[test]
fn handover_refused_while_locked() {
    let (mut f, ..) = two_clients_with(handover_config());
    start_lock(&mut f);
    finish_lock(&mut f);

    owner_bind(&mut f, KEY_D);
    assert!(!f.niri().agent.handed_over);
    // Even when the action is let through the lock.
    f.niri_state().do_action(
        niri_config::Action::ToggleAgentHandover,
        true,
        InputTime::from_micros(0),
    );
    assert!(!f.niri().agent.handed_over);
}

#[test]
fn lock_ends_handover() {
    let (mut f, ..) = two_clients_with(handover_config());
    owner_bind(&mut f, KEY_D);
    start_lock(&mut f);
    assert!(!f.niri().agent.handed_over);

    // Handed over again while the lock waits for its surfaces: going through ends it.
    owner_bind(&mut f, KEY_D);
    assert!(f.niri().agent.handed_over);
    finish_lock(&mut f);
    assert!(!f.niri().agent.handed_over);
}

#[test]
fn pause_ends_handover() {
    let (mut f, ..) = two_clients_with(handover_config());
    // The kill switch.
    f.niri().agent.hand_over();
    f.niri_state().do_action(
        niri_config::Action::ToggleAgentInput,
        false,
        InputTime::from_micros(0),
    );
    assert!(f.niri().agent.paused);
    assert!(!f.niri().agent.handed_over);

    // The IPC pause.
    f.niri().agent.hand_over();
    f.niri().agent.pause();
    assert!(!f.niri().agent.handed_over);
}

#[test]
fn owner_key_ends_handover_and_is_swallowed() {
    let (mut f, human, a, ..) = two_clients_with(handover_config());
    owner_bind(&mut f, KEY_D);
    events(&mut f, human, &a);

    physical_key(&mut f);

    assert!(!f.niri().agent.handed_over);
    let events = events(&mut f, human, &a);
    assert!(!events.contains("key"), "{events}");
    assert!(f.niri().suppressed_keys.is_empty());
}

#[test]
fn owner_key_into_exit_dialog_ends_handover() {
    let (mut f, ..) = two_clients_with(handover_config());
    f.niri().agent.hand_over();
    assert!(f.niri().exit_confirm_dialog.show());
    physical_key(&mut f);
    assert!(!f.niri().agent.handed_over);
}

#[test]
fn modifier_alone_keeps_handover() {
    let (mut f, ..) = two_clients_with(handover_config());
    owner_bind(&mut f, KEY_D);
    // Shift_L.
    press_keys(&mut f, &[50]);
    assert!(f.niri().agent.handed_over);
}

#[test]
fn owner_bind_ends_handover_and_runs() {
    let (mut f, ..) = two_clients_with(handover_config());
    focus_for_owner(&mut f, "agent-b");
    owner_bind(&mut f, KEY_D);

    // Mod+1.
    press_keys(&mut f, &[133, 10]);

    assert!(!f.niri().agent.handed_over);
    assert_eq!(focused_app_id(&mut f).as_deref(), Some("a"));
}

#[test]
fn handover_bind_turns_it_off_once() {
    let (mut f, ..) = two_clients_with(handover_config());
    f.niri().agent.hand_over();
    owner_bind(&mut f, KEY_D);
    assert!(!f.niri().agent.handed_over);
    assert!(f.niri().suppressed_keys.is_empty());
}

#[test]
fn release_handover_bind_turns_it_off_once() {
    let config = "binds { Mod+Alt+D { release { toggle-agent-handover; }; }; }";
    let (mut f, ..) = two_clients_with(Config::parse_mem(config).unwrap());
    f.niri().agent.hand_over();
    owner_bind(&mut f, KEY_D);
    assert!(!f.niri().agent.handed_over);
    assert!(f.niri().suppressed_keys.is_empty());
}

#[test]
fn release_handover_bind_interrupted_still_ends_it() {
    let config = "binds { Mod+Alt+D { release { toggle-agent-handover; }; }; }";
    let (mut f, _, _, agent, _) = two_clients_with(Config::parse_mem(config).unwrap());
    f.niri().agent.hand_over();

    // Super_L, Alt_L, D; an agent scroll cancels the pending release bind; then the releases.
    for code in [133, 64, KEY_D] {
        key_event(&mut f, code, KeyState::Pressed);
    }
    let pointer = f.client(agent).state.virtual_pointer();
    pointer.axis(0, Axis::VerticalScroll, 10.);
    pointer.frame();
    f.roundtrip(agent);
    for code in [KEY_D, 64, 133] {
        key_event(&mut f, code, KeyState::Released);
    }

    assert!(!f.niri().agent.handed_over);
    assert!(f.niri().suppressed_keys.is_empty());
}

#[test]
fn egl_handover_bind_with_screenshot_ui_open_ends_it() {
    let (mut f, ..) = two_clients_with(handover_config());
    f.niri_state().backend.headless().add_renderer().unwrap();
    f.niri_state().do_action(
        niri_config::Action::Screenshot(true, None),
        false,
        InputTime::from_micros(0),
    );
    assert!(f.niri().screenshot_ui.is_open());
    f.niri().agent.hand_over();

    owner_bind(&mut f, KEY_D);

    assert!(!f.niri().agent.handed_over);
}

#[test]
fn virtual_keys_keep_handover() {
    let (mut f, _, _, agent, _) = two_clients_with(handover_config());
    let keyboard = f.client(agent).state.virtual_keyboard();
    f.roundtrip(agent);
    f.niri().agent.hand_over();

    // KEY_A, then Mod+Alt+D.
    keyboard.key(0, 30, 1);
    keyboard.key(0, 30, 0);
    for key in [125, 56, 32] {
        keyboard.key(0, key, 1);
    }
    for key in [32, 56, 125] {
        keyboard.key(0, key, 0);
    }
    f.roundtrip(agent);

    assert!(f.niri().agent.handed_over);
}

#[test]
fn virtual_click_and_scroll_keep_handover() {
    let (mut f, _, _, agent, _) = two_clients_with(handover_config());
    f.niri().agent.hand_over();

    let pointer = f.client(agent).state.virtual_pointer();
    pointer.button(0, 0x110, ButtonState::Pressed);
    pointer.button(0, 0x110, ButtonState::Released);
    pointer.frame();
    pointer.axis(0, Axis::VerticalScroll, 10.);
    pointer.frame();
    f.roundtrip(agent);

    assert!(f.niri().agent.handed_over);
}

#[test]
fn owner_click_ends_handover() {
    let (mut f, ..) = two_clients_with(handover_config());
    f.niri().agent.hand_over();
    physical_click(&mut f);
    assert!(!f.niri().agent.handed_over);
}

#[test]
fn owner_scroll_ends_handover() {
    let (mut f, ..) = two_clients_with(handover_config());
    f.niri().agent.hand_over();
    physical_scroll(&mut f, 10.);
    assert!(!f.niri().agent.handed_over);
}

#[test]
fn owner_motion_keeps_handover() {
    let (mut f, ..) = two_clients_with(handover_config());
    f.niri().agent.hand_over();
    physical_motion(&mut f, 500., 500.);
    physical_motion(&mut f, 600., 400.);
    assert!(f.niri().agent.handed_over);
}

#[test]
fn ffm_suspended_while_handed_over() {
    let config = format!("input {{ focus-follows-mouse; }}\n{HANDOVER_BINDS}");
    let mut f = Fixture::with_config(Config::parse_mem(&config).unwrap());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a");
    map_window(&mut f, id, "c");
    focus_for_owner(&mut f, "a");
    f.niri_complete_animations();
    let (a, c) = (window_of(&mut f, "a"), window_of(&mut f, "c"));
    let over_a = point_over(&mut f, &a);
    let over_c = point_over(&mut f, &c);
    physical_motion(&mut f, over_a.0, over_a.1);

    f.niri().agent.hand_over();
    physical_motion(&mut f, over_c.0, over_c.1);
    assert_eq!(focused_app_id(&mut f).as_deref(), Some("a"));

    f.niri().agent.end_handover();
    physical_motion(&mut f, over_a.0, over_a.1);
    physical_motion(&mut f, over_c.0, over_c.1);
    assert_eq!(focused_app_id(&mut f).as_deref(), Some("c"));
}

/// Maps plain window "b" from the agent's client on the owner's workspace; "a" keeps focus.
fn plain_b(f: &mut Fixture, human: ClientId, agent: ClientId) -> (WlSurface, MappedId) {
    let b = map_window(f, agent, "b");
    focus_for_owner(f, "a");
    f.double_roundtrip(human);
    f.double_roundtrip(agent);
    let id = mapped_id(f, "b");
    (b, id)
}

#[test]
fn handover_drives_ungranted_visible_window() {
    let (mut f, human, _, agent, _) = two_clients_with(handover_config());
    let (_, id) = plain_b(&mut f, human, agent);
    assert_eq!(
        check_target(f.niri(), id, OpKind::Keyboard).err(),
        Some(AgentError::NotGranted)
    );
    f.niri().agent.hand_over();
    assert!(check_target(f.niri(), id, OpKind::Keyboard).is_ok());
}

#[test]
fn handover_refuses_offscreen_ungranted_window() {
    let (mut f, human, _, agent, _) = two_clients_with(handover_config());
    let (_, id) = plain_b(&mut f, human, agent);
    let w = window_of(&mut f, "b");
    f.niri()
        .layout
        .move_to_workspace(Some(&w), 1, ActivateWindow::No);
    focus_for_owner(&mut f, "a");
    f.niri().agent.hand_over();
    let err = check_target(f.niri(), id, OpKind::Keyboard).err().unwrap();
    assert_eq!(err, AgentError::NotVisible);
    assert_eq!(err.to_string(), "not_visible");
}

#[test]
fn handover_drives_focused_window() {
    let (mut f, ..) = two_clients_with(handover_config());
    let id = mapped_id(&mut f, "a");
    f.niri().agent.hand_over();
    assert!(check_target(f.niri(), id, OpKind::Keyboard).is_ok());
}

#[test]
fn handover_refuses_blocked_out_input() {
    let (mut f, id) = set_up_blocked_out("screen-capture");
    f.niri().agent.hand_over();
    assert_eq!(
        type_now(&mut f, id, "x").unwrap_err(),
        AgentError::BlockedOut
    );
}

#[test]
fn handover_allows_paste_chord() {
    let (mut f, _, _, agent, b) = two_clients_with(handover_config());
    let id = agent_target(&mut f, agent, &b);
    f.niri().agent.hand_over();
    key_now(&mut f, id, "ctrl+v", 1).unwrap();
    assert!(events(&mut f, agent, &b).contains("key pressed"));
}

#[test]
fn handover_allows_middle_click() {
    let (mut f, _, _, agent, b) = two_clients_with(handover_config());
    let id = captured_target(&mut f, agent, &b);
    f.niri().agent.hand_over();
    let op = AgentOp::Click {
        x: 10.,
        y: 10.,
        button: AgentButton::Middle,
        count: 1,
        mods: Vec::new(),
    };
    pointer_now(&mut f, id, op).unwrap();
    assert!(pointer_events(&mut f, agent, &b)
        .iter()
        .any(|e| matches!(e, PointerEvent::Button { button: 0x112, .. })));
}

#[test]
fn handover_refuses_inactive_tab() {
    let (mut f, human, _, agent, _) = two_clients_with(handover_config());
    let (_, id) = plain_b(&mut f, human, agent);
    let a = window_of(&mut f, "a");
    let b = window_of(&mut f, "b");
    f.niri().layout.consume_or_expel_window_left(Some(&b));
    f.niri().layout.toggle_column_tabbed_display();
    f.niri().layout.activate_window(&a);
    f.niri_complete_animations();
    f.niri().agent.hand_over();
    assert_eq!(
        check_target(f.niri(), id, OpKind::Keyboard).err(),
        Some(AgentError::NotVisible)
    );
}

#[test]
fn handover_refuses_offscreen_column() {
    let (mut f, human, _, agent, _) = two_clients_with(handover_config());
    plain_b(&mut f, human, agent);
    // Windows are 100 px wide, so it takes this many columns to leave the 1920 px output.
    for i in 0..20 {
        map_window(&mut f, agent, &format!("c{i}"));
    }
    focus_for_owner(&mut f, "a");
    f.niri_complete_animations();
    f.niri().agent.hand_over();
    let results: Vec<_> = (0..20)
        .map(|i| {
            let id = mapped_id(&mut f, &format!("c{i}"));
            check_target(f.niri(), id, OpKind::Keyboard).err()
        })
        .collect();
    assert!(results.contains(&Some(AgentError::NotVisible)));
    assert!(results.contains(&None));
}

#[test]
fn paste_stops_when_handover_ends() {
    let (mut f, _, _, agent, b) = two_clients_with(handover_config());
    let id = agent_target(&mut f, agent, &b);
    f.niri().agent.hand_over();
    let (tx, rx) = async_channel::bounded(1);
    keyboard::key(
        f.niri_state(),
        id,
        "ctrl+v",
        2 * TYPE_CHUNK_CHARS as u16,
        tx,
    );
    f.client(agent).dispatch();
    f.niri().agent.end_handover();
    step_chunk(&mut f);
    assert!(matches!(
        rx.try_recv().unwrap(),
        Err(AgentError::Interrupted { error, typed: TYPE_CHUNK_CHARS })
            if *error == AgentError::PasteUnsupported
    ));
    let events = events(&mut f, agent, &b);
    assert_eq!(events.matches("key pressed: 47").count(), TYPE_CHUNK_CHARS);
}

#[test]
fn handover_new_window_from_agent_click_focuses() {
    let (mut f, agent, b, other, _) = three_clients_with(handover_config());
    f.niri().agent.hand_over();
    let id = agent_target(&mut f, agent, &b);
    type_now(&mut f, id, "x").unwrap();

    map_window(&mut f, other, "c2");

    assert_eq!(focused_app_id(&mut f).as_deref(), Some("c2"));
}

#[test]
fn handover_agent_serial_token_activates() {
    let (mut f, human, _, agent, _) = two_clients_with(honor_invalid_serial());
    let (b, id) = plain_b(&mut f, human, agent);
    f.niri().agent.granted.insert(id, true);
    f.niri().agent.hand_over();
    let record = record_for(&mut f, id);
    f.niri().agent.captures.insert(id, record);
    pointer_events(&mut f, agent, &b);
    pointer_now(&mut f, id, click(10., 10.)).unwrap();
    let serial = pointer_events(&mut f, agent, &b)
        .into_iter()
        .find_map(|e| match e {
            PointerEvent::Button { serial, .. } => Some(serial),
            _ => None,
        })
        .unwrap();

    self_activate(&mut f, agent, Some(serial), &b);

    assert_eq!(focused_app_id(&mut f).as_deref(), Some("b"));
}

const WARP_CONFIG: &str = "input { warp-mouse-to-focus; }";

/// Two columns "a" and "b", "b" focused, with the pointer parked at the top-left corner.
fn warp_setup() -> Fixture {
    let mut f = Fixture::with_config(Config::parse_mem(WARP_CONFIG).unwrap());
    f.add_output(1, (1920, 1080));
    for app_id in ["a", "b"] {
        let id = f.add_client();
        map_window(&mut f, id, app_id);
    }
    f.niri_state().update_keyboard_focus();
    f.niri_state().move_cursor(Point::from((0., 0.)));
    f
}

fn pointer_location(f: &mut Fixture) -> Point<f64, smithay::utils::Logical> {
    f.niri().seat.get_pointer().unwrap().current_location()
}

fn focus_left() -> niri_ipc::Action {
    niri_ipc::Action::FocusColumnLeft {}
}

#[test]
fn agent_action_needs_handover() {
    let mut f = warp_setup();
    let r = crate::agent::run_action(f.niri_state(), focus_left());
    assert_eq!(r, Err(AgentError::NotHandedOver));
}

#[test]
fn agent_action_quit_refused() {
    let mut f = warp_setup();
    f.niri().agent.hand_over();
    let r = crate::agent::run_action(
        f.niri_state(),
        niri_ipc::Action::Quit {
            skip_confirmation: true,
        },
    );
    assert_eq!(r, Err(AgentError::ActionRefused("quit".into())));
}

#[test]
fn agent_action_focus_does_not_warp() {
    let mut f = warp_setup();
    f.niri().agent.hand_over();
    let before = pointer_location(&mut f);
    crate::agent::run_action(f.niri_state(), focus_left()).unwrap();
    assert_eq!(focused_app_id(&mut f).as_deref(), Some("a"));
    assert_eq!(pointer_location(&mut f), before);
    assert!(!f.niri().agent.suppress_warp);
}

#[test]
fn owner_action_still_warps() {
    let mut f = warp_setup();
    let before = pointer_location(&mut f);
    f.niri_state().do_action(
        niri_config::Action::FocusColumnLeft,
        false,
        InputTime::from_micros(0),
    );
    assert_ne!(pointer_location(&mut f), before);
}

#[test]
fn agent_action_focus_monitor_does_not_move_pointer() {
    let mut f = warp_setup();
    f.add_output(2, (1920, 1080));
    f.niri().agent.hand_over();
    let before = pointer_location(&mut f);
    crate::agent::run_action(f.niri_state(), niri_ipc::Action::FocusMonitorRight {}).unwrap();
    assert_eq!(pointer_location(&mut f), before);
}

/// Fails on an `Enter` while the pointer is already entered: an app never sees two pointers.
fn assert_one_pointer(events: &[PointerEvent]) {
    let mut entered = false;
    for (i, event) in events.iter().enumerate() {
        match event {
            PointerEvent::Enter { .. } => {
                assert!(!entered, "enter #{i} while entered: {events:?}");
                entered = true;
            }
            PointerEvent::Leave => entered = false,
            _ => (),
        }
    }
}

/// The agent clicks B; returns B's pointer events.
fn agent_click_b(f: &mut Fixture, agent: ClientId, b: &WlSurface) -> Vec<PointerEvent> {
    let id = captured_target(f, agent, b);
    pointer_now(f, id, click(10., 10.)).unwrap();
    pointer_events(f, agent, b)
}

#[test]
fn agent_pointer_stays_entered_after_click() {
    let (mut f, _, _, agent, b) = two_clients();
    let got = agent_click_b(&mut f, agent, &b);
    assert_one_pointer(&got);
    assert!(
        matches!(
            got[..],
            [
                ..,
                PointerEvent::Button {
                    state: ButtonState::Released,
                    ..
                },
                PointerEvent::Frame
            ]
        ),
        "{got:?}"
    );
}

#[test]
fn agent_pointer_second_click_sends_no_second_enter() {
    let (mut f, _, _, agent, b) = two_clients();
    let id = captured_target(&mut f, agent, &b);
    pointer_now(&mut f, id, click(10., 10.)).unwrap();
    pointer_now(&mut f, id, click(20., 20.)).unwrap();

    let got = pointer_events(&mut f, agent, &b);
    assert_one_pointer(&got);
    let enters = got
        .iter()
        .filter(|e| matches!(e, PointerEvent::Enter { .. }))
        .count();
    assert_eq!(enters, 1, "{got:?}");
    assert!(
        got.contains(&PointerEvent::Motion { x: 20., y: 20. }),
        "{got:?}"
    );
}

#[test]
fn agent_pointer_leaves_for_another_surface() {
    let (mut f, _, _, agent, b) = two_clients();
    let c = map_window(&mut f, agent, "agent-c");
    let c_id = mapped_id(&mut f, "agent-c");
    let record = record_for(&mut f, c_id);
    f.niri().agent.captures.insert(c_id, record);
    pointer_events(&mut f, agent, &c);
    let mut on_b = agent_click_b(&mut f, agent, &b);
    assert!(!on_b.contains(&PointerEvent::Leave), "{on_b:?}");

    pointer_now(&mut f, c_id, click(10., 10.)).unwrap();

    let left = pointer_events(&mut f, agent, &b);
    assert_eq!(left, [PointerEvent::Leave, PointerEvent::Frame]);
    on_b.extend(left);
    assert_one_pointer(&on_b);
    let on_c = pointer_events(&mut f, agent, &c);
    assert_one_pointer(&on_c);
    assert!(
        matches!(on_c.first(), Some(PointerEvent::Enter { .. })),
        "{on_c:?}"
    );
}

#[test]
fn agent_pointer_leaves_after_idle() {
    let (mut f, _, _, agent, b) = two_clients();
    let mut got = agent_click_b(&mut f, agent, &b);

    f.niri()
        .agent
        .release_if_idle(Instant::now() + Duration::from_secs(9));
    assert_eq!(pointer_events(&mut f, agent, &b), []);

    f.niri()
        .agent
        .release_if_idle(Instant::now() + Duration::from_secs(11));
    let left = pointer_events(&mut f, agent, &b);
    assert_eq!(left, [PointerEvent::Leave, PointerEvent::Frame]);
    got.extend(left);
    assert_one_pointer(&got);
}

#[test]
fn agent_pointer_leaves_on_pause() {
    let (mut f, _, _, agent, b) = two_clients();
    let mut got = agent_click_b(&mut f, agent, &b);

    f.niri().agent.pause();

    let left = pointer_events(&mut f, agent, &b);
    assert_eq!(left, [PointerEvent::Leave, PointerEvent::Frame]);
    got.extend(left);
    assert_one_pointer(&got);
}

#[test]
fn owner_entering_client_releases_agent_pointer_first() {
    let (mut f, human, _, agent, b) = two_clients();
    b_on_owner_workspace(&mut f, human);
    f.niri_complete_animations();
    let mut got = agent_click_b(&mut f, agent, &b);

    let window = window_of(&mut f, "agent-b");
    let (x, y) = point_over(&mut f, &window);
    physical_motion(&mut f, x, y);

    let after = pointer_events(&mut f, agent, &b);
    let leave = after.iter().position(|e| *e == PointerEvent::Leave);
    let enter = after
        .iter()
        .position(|e| matches!(e, PointerEvent::Enter { .. }));
    assert!(leave.is_some() && leave < enter, "{after:?}");
    got.extend(after);
    assert_one_pointer(&got);
}

/// The owner's pointer rests on "a", the owner hands over, and the agent clicks "a". Returns the
/// events "a" got from the owner's first enter, and where the owner's pointer is.
fn park_over_a(f: &mut Fixture, human: ClientId, a: &WlSurface) -> (Vec<PointerEvent>, (f64, f64)) {
    f.niri_complete_animations();
    let window = window_of(f, "a");
    let (x, y) = point_over(f, &window);
    physical_motion(f, x, y);
    let got = pointer_events(f, human, a);
    assert!(
        matches!(got.first(), Some(PointerEvent::Enter { .. })),
        "{got:?}"
    );

    f.niri().agent.hand_over();
    let id = mapped_id(f, "a");
    let record = record_for(f, id);
    f.niri().agent.captures.insert(id, record);
    pointer_now(f, id, click(10., 10.)).unwrap();
    (got, (x, y))
}

#[test]
fn handover_parks_owner_pointer() {
    let (mut f, human, a, ..) = two_clients_with(handover_config());
    let (mut got, (x, y)) = park_over_a(&mut f, human, &a);

    // The owner's pointer leaves before the agent's enters.
    let parked = pointer_events(&mut f, human, &a);
    assert!(
        matches!(
            parked[..],
            [
                PointerEvent::Leave,
                PointerEvent::Frame,
                PointerEvent::Enter { .. },
                ..
            ]
        ),
        "{parked:?}"
    );
    assert_eq!(f.niri().seat.get_pointer().unwrap().current_focus(), None);
    got.extend(parked);

    // While parked, a refresh doesn't bring the owner's pointer back.
    f.niri().pointer_contents = Default::default();
    f.niri_state().refresh_pointer_contents();
    let refreshed = pointer_events(&mut f, human, &a);
    assert_eq!(refreshed, []);

    // The owner's next motion: the agent's pointer leaves, then the owner's enters.
    physical_motion(&mut f, x + 1., y);
    let back = pointer_events(&mut f, human, &a);
    assert!(
        matches!(
            back[..],
            [
                PointerEvent::Leave,
                PointerEvent::Frame,
                PointerEvent::Enter { .. },
                ..
            ]
        ),
        "{back:?}"
    );
    let a_surface = server_surface(&mut f, "a");
    assert_eq!(
        f.niri().seat.get_pointer().unwrap().current_focus(),
        Some(a_surface)
    );
    got.extend(back);
    assert_one_pointer(&got);
}

#[test]
fn owner_warp_onto_agent_client_releases_agent_pointer_first() {
    let (mut f, human, _, agent, b) = two_clients();
    b_on_owner_workspace(&mut f, human);
    f.niri_complete_animations();
    let mut got = agent_click_b(&mut f, agent, &b);

    let window = window_of(&mut f, "agent-b");
    let (x, y) = point_over(&mut f, &window);
    f.niri_state().move_cursor((x, y).into());

    let after = pointer_events(&mut f, agent, &b);
    assert!(
        matches!(after.first(), Some(PointerEvent::Leave)),
        "{after:?}"
    );
    got.extend(after);
    assert_one_pointer(&got);
}

#[test]
fn owner_pointer_refresh_onto_agent_client_releases_agent_pointer_first() {
    let (mut f, human, a, agent, b) = two_clients();
    f.niri_complete_animations();
    let window = window_of(&mut f, "a");
    let (x, y) = point_over(&mut f, &window);
    physical_motion(&mut f, x, y);
    pointer_events(&mut f, human, &a);
    let mut got = agent_click_b(&mut f, agent, &b);

    // B shows up under the owner's still pointer.
    f.niri().layout.switch_workspace_down();
    f.niri_complete_animations();

    let after = pointer_events(&mut f, agent, &b);
    assert!(
        matches!(after.first(), Some(PointerEvent::Leave)),
        "{after:?}"
    );
    assert!(
        after
            .iter()
            .any(|e| matches!(e, PointerEvent::Enter { .. })),
        "{after:?}"
    );
    got.extend(after);
    assert_one_pointer(&got);
}

/// Positions of the first `Leave`, the first `Enter` and the first event matching `then`.
fn leave_enter_then(
    events: &[PointerEvent],
    then: impl Fn(&PointerEvent) -> bool,
) -> (Option<usize>, Option<usize>, Option<usize>) {
    (
        events.iter().position(|e| *e == PointerEvent::Leave),
        events
            .iter()
            .position(|e| matches!(e, PointerEvent::Enter { .. })),
        events.iter().position(then),
    )
}

#[test]
fn owner_click_while_parked_lands_on_the_window() {
    let (mut f, human, a, ..) = two_clients_with(handover_config());
    let (mut got, _) = park_over_a(&mut f, human, &a);
    got.extend(pointer_events(&mut f, human, &a));

    // No motion: the click that ends the hand-over must still reach "a".
    physical_click(&mut f);

    assert!(!f.niri().agent.handed_over);
    let after = pointer_events(&mut f, human, &a);
    let (leave, enter, button) = leave_enter_then(&after, |e| {
        matches!(
            e,
            PointerEvent::Button {
                state: ButtonState::Pressed,
                ..
            }
        )
    });
    assert!(
        leave.is_some() && leave < enter && enter < button,
        "{after:?}"
    );
    got.extend(after);
    assert_one_pointer(&got);
}

#[test]
fn owner_scroll_while_parked_lands_on_the_window() {
    let (mut f, human, a, ..) = two_clients_with(handover_config());
    let (mut got, _) = park_over_a(&mut f, human, &a);
    got.extend(pointer_events(&mut f, human, &a));

    physical_scroll(&mut f, 10.);

    assert!(!f.niri().agent.handed_over);
    let after = pointer_events(&mut f, human, &a);
    let (leave, enter, axis) = leave_enter_then(&after, |e| matches!(e, PointerEvent::Axis { .. }));
    assert!(
        leave.is_some() && leave < enter && enter < axis,
        "{after:?}"
    );
    got.extend(after);
    assert_one_pointer(&got);
}

#[test]
fn pause_while_parked_gives_the_owner_pointer_back() {
    let (mut f, human, a, ..) = two_clients_with(handover_config());
    let (mut got, _) = park_over_a(&mut f, human, &a);
    got.extend(pointer_events(&mut f, human, &a));

    // No motion: the next refresh enters the owner's pointer again.
    f.niri().agent.pause();

    let after = pointer_events(&mut f, human, &a);
    let (leave, enter, _) = leave_enter_then(&after, |_| false);
    assert!(leave.is_some() && leave < enter, "{after:?}");
    let a_surface = server_surface(&mut f, "a");
    assert_eq!(
        f.niri().seat.get_pointer().unwrap().current_focus(),
        Some(a_surface)
    );
    assert!(!f.niri().agent.owner_pointer_parked);
    got.extend(after);
    assert_one_pointer(&got);
}

#[test]
fn park_refused_while_a_grab_holds_the_owner_pointer() {
    let (mut f, human, a, agent, _) = two_clients_with(handover_config());
    events(&mut f, human, &a);
    f.niri_complete_animations();
    let window = window_of(&mut f, "a");
    let (x, y) = point_over(&mut f, &window);
    physical_motion(&mut f, x, y);
    let mut got = pointer_events(&mut f, human, &a);

    f.niri().agent.hand_over();
    // A held button (virtual, so the hand-over stays) grabs the seat pointer on "a".
    let pointer = f.client(agent).state.virtual_pointer();
    pointer.button(0, 0x110, ButtonState::Pressed);
    pointer.frame();
    f.roundtrip(agent);
    assert!(f.niri().agent.handed_over);

    let id = mapped_id(&mut f, "a");
    let record = record_for(&mut f, id);
    f.niri().agent.captures.insert(id, record);
    let ctrl_click = AgentOp::Click {
        x: 10.,
        y: 10.,
        button: AgentButton::Left,
        count: 1,
        mods: vec![niri_ipc::AgentMod::Ctrl],
    };
    assert_eq!(
        pointer_now(&mut f, id, ctrl_click).unwrap_err(),
        AgentError::SameClient
    );

    // The modifiers sent before the refusal are cleared.
    let keys = events(&mut f, human, &a);
    assert_eq!(
        keys.lines().rfind(|l| l.starts_with("modifiers")),
        Some("modifiers: depressed=0, latched=0, locked=0, group=0"),
        "{keys}"
    );
    let after = pointer_events(&mut f, human, &a);
    assert!(!after.contains(&PointerEvent::Leave), "{after:?}");
    assert!(!f.niri().agent.owner_pointer_parked);
    got.extend(after);
    assert_one_pointer(&got);
}

/// Pointer events every surface of the client got since the last call, in order.
fn client_pointer_events(f: &mut Fixture, client: ClientId) -> Vec<PointerEvent> {
    f.double_roundtrip(client);
    std::mem::take(&mut f.client(client).state.pointer_events_received)
}

#[test]
fn owner_entering_other_surface_of_agent_client_releases_agent_pointer_first() {
    let config = Config::parse_mem(
        r#"
layout {
    default-column-width { proportion 0.25; }
}
"#,
    )
    .unwrap();
    let (mut f, human, _, agent, b) = two_clients_with(config);
    b_on_owner_workspace(&mut f, human);
    map_window(&mut f, agent, "agent-c");
    focus_for_owner(&mut f, "a");
    f.niri_complete_animations();
    client_pointer_events(&mut f, agent);
    agent_click_b(&mut f, agent, &b);

    let window = window_of(&mut f, "agent-c");
    let (x, y) = point_over(&mut f, &window);
    physical_motion(&mut f, x, y);

    // Client-wide: B's Leave comes before C's Enter.
    let got = client_pointer_events(&mut f, agent);
    assert_one_pointer(&got);
    assert!(
        got.contains(&PointerEvent::Leave)
            && got
                .iter()
                .filter(|e| matches!(e, PointerEvent::Enter { .. }))
                .count()
                == 2,
        "{got:?}"
    );
    let b_left = pointer_events(&mut f, agent, &b);
    assert_eq!(
        b_left.iter().filter(|e| **e == PointerEvent::Leave).count(),
        1,
        "{b_left:?}"
    );
}
