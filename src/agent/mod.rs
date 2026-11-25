//! Agent desktop control: window capture and input for AI agents.

pub mod capture;
pub mod keyboard;
pub mod pointer;

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::time::{Duration, Instant};

use calloop::timer::{TimeoutAction, Timer};
use niri_ipc::{AgentDone, AgentError, AgentStatus, AgentWindow};
use smithay::backend::input::{
    ButtonState, Event as _, InputBackend, InputEvent, InputTime, KeyState, KeyboardKeyEvent as _,
    PointerButtonEvent as _, TabletToolTipEvent as _, TabletToolTipState,
};
use smithay::desktop::{PopupKind, PopupManager};
use smithay::output::Output;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_keyboard::WlKeyboard;
use smithay::reexports::wayland_server::protocol::wl_pointer::WlPointer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource as _};
use smithay::utils::{
    IsAlive as _, Logical, Physical, Point, Rectangle, Serial, Size, SERIAL_COUNTER,
};
use smithay::wayland::virtual_keyboard::VirtualKeyboardDevice;
use smithay::wayland::virtual_pointer::VirtualPointerDevice;
use smithay::wayland::xdg_activation::XdgActivationTokenData;

use self::capture::capture_precheck;
use crate::layout::workspace::WorkspaceId;
use crate::layout::{ActivateWindow, Layout};
use crate::niri::{KeyboardFocus, Niri, State};
use crate::utils::{get_credentials_for_surface, get_monotonic_time, with_toplevel_role};
use crate::window::mapped::{Mapped, MappedId};

pub const AGENT_APP_ID_PREFIX: &str = "agent-";
pub const DEFAULT_MAX_EDGE: u32 = 1568;
pub const DEFAULT_SETTLE: Duration = Duration::from_millis(150);
/// Longest settle time a capture waits.
pub const MAX_SETTLE: Duration = Duration::from_secs(5);
pub const BURST_BOOST: Duration = Duration::from_millis(500);
pub const ACTIVE_INDICATOR: Duration = Duration::from_secs(2);
pub const IDLE_RELEASE: Duration = Duration::from_secs(10);
pub const NEW_WINDOW_GUARD: Duration = Duration::from_secs(5);
pub const TYPE_CHUNK_CHARS: usize = 1000;
/// Pause before each later `type` chunk, so the client can read the previous one.
pub const TYPE_CHUNK_INTERVAL: Duration = Duration::from_millis(200);
pub const SERIAL_RING: usize = 4096;
/// Most motion steps a `drag` sends.
pub const MAX_DRAG_STEPS: u16 = 1000;

/// Interval between boosted frame callbacks.
const BOOST_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug, Default)]
pub struct AgentState {
    /// Kill switch: agent input and captures return `paused`.
    pub paused: bool,
    /// Whether the owner handed the controls over to the agent.
    pub handed_over: bool,
    /// Set while an agent action runs: the cursor warps that follow focus changes are skipped.
    pub suppress_warp: bool,
    /// Last capture of each window.
    // ponytail: records of closed windows are never pruned (ids are never reused); prune on
    // window close if agents ever capture enough windows for this to matter.
    pub captures: HashMap<MappedId, CaptureRecord>,
    /// Windows the owner granted (true) or revoked (false) by keybind, overriding the app id
    /// prefix.
    pub granted: HashMap<MappedId, bool>,
    /// Windows with a running frame boost, and when the boost ends.
    boosts: HashMap<MappedId, Instant>,
    /// Serials of recent agent events, oldest first.
    serials: VecDeque<Serial>,
    /// The agent's shadow keyboard focus.
    sticky: Option<Sticky>,
    /// The surface the agent pointer is entered on.
    pub pointer_focus: Option<PointerFocus>,
    /// Whether a hand-over took the seat pointer focus off a client for the agent pointer; the
    /// owner's next motion gives it back.
    pub owner_pointer_parked: bool,
    /// Whether the idle release timer is running.
    idle_timer: bool,
    // ponytail: the per-client and per-window maps below are never pruned (a few bytes per
    // client or window ever typed into); prune on client disconnect if that ever matters.
    /// Last agent burst into each client.
    pub last_burst: HashMap<ClientId, Instant>,
    /// Window of the last agent burst into each client.
    pub last_target: HashMap<ClientId, MappedId>,
    /// Last agent burst into any client.
    pub last_agent_burst: Option<Instant>,
    /// Last key, button, scroll or touch-down from a physical device.
    last_human_input: Option<Instant>,
    /// Until when each window shows as agent-active.
    pub active_until: HashMap<MappedId, Instant>,
    /// Popups that agent clicks opened, kept without a grab, topmost last; with whether their
    /// window's workspace was visible at the last check.
    popups: HashMap<MappedId, Vec<(PopupKind, bool)>>,
}

/// Runs a niri action for the agent during a hand-over, without warping the owner's pointer.
pub fn run_action(state: &mut State, action: niri_ipc::Action) -> Result<(), AgentError> {
    if !state.niri.agent.handed_over {
        return Err(AgentError::NotHandedOver);
    }
    if matches!(action, niri_ipc::Action::Quit { .. }) {
        return Err(AgentError::ActionRefused(String::from("quit")));
    }

    state.niri.agent.suppress_warp = true;
    state.do_action(action.into(), false, InputTime::now());
    state.update_keyboard_focus();
    state.niri.agent.suppress_warp = false;
    Ok(())
}

/// Marks an xdg-activation token made while an agent acted last: it only sets urgency, whatever
/// the window rules say.
pub struct AgentGuardMarker;

/// A surface holding the agent's shadow keyboard focus, and the keyboards that got its `enter`.
#[derive(Debug)]
pub struct Sticky {
    pub surface: WlSurface,
    pub keyboards: Vec<WlKeyboard>,
}

/// A surface the agent pointer is entered on, and the pointers that got its `enter`.
#[derive(Debug)]
pub struct PointerFocus {
    pub surface: WlSurface,
    pub pointers: Vec<WlPointer>,
}

/// What a capture looked like, to map its pixels back to the window and to detect staleness.
#[derive(Debug, Clone, Copy)]
pub struct CaptureRecord {
    /// Top-left corner of the image in physical pixels at `scale`, relative to the main surface's
    /// buffer origin. Image pixel `p` is at surface-local logical `(p + origin) / scale`.
    pub origin: Point<i32, Physical>,
    /// Physical pixels per logical pixel in the image.
    pub scale: f64,
    /// Image size.
    pub size: Size<i32, Physical>,
    /// Window geometry size at capture time.
    pub window_size: Size<i32, Logical>,
    /// Scale of the window's output at capture time.
    pub output_scale: f64,
    /// Number of popups of the window at capture time.
    pub popup_count: usize,
    pub at: Instant,
}

/// Window size, output scale and popup count: what a capture records and `StaleCapture` compares.
pub fn window_view(mapped: &Mapped, output: Option<&Output>) -> (Size<i32, Logical>, f64, usize) {
    (
        mapped.window.geometry().size,
        output.map_or(1., |o| o.current_scale().fractional_scale()),
        PopupManager::popups_for_surface(mapped.toplevel().wl_surface()).count(),
    )
}

/// A window an agent may act on.
#[derive(Debug, Clone)]
pub struct Target {
    pub id: MappedId,
    pub toplevel: WlSurface,
    pub client: Client,
}

/// Kind of input op; pointer ops also need a fresh capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Keyboard,
    Pointer,
}

impl AgentState {
    /// Pauses agent input and ends any hand-over. One-way: only the owner's keybind unpauses.
    pub fn pause(&mut self) {
        self.paused = true;
        self.handed_over = false;
        self.release_sticky();
        self.release_pointer();
        self.dismiss_all();
    }

    /// Hands the controls over to the agent, unpausing it.
    pub fn hand_over(&mut self) {
        self.handed_over = true;
        self.paused = false;
    }

    /// Takes the controls back from the agent; it stays unpaused.
    pub fn end_handover(&mut self) {
        self.handed_over = false;
    }

    /// Takes the next serial and remembers it as an agent serial.
    pub fn next_serial(&mut self) -> Serial {
        let serial = SERIAL_COUNTER.next_serial();
        if self.serials.len() == SERIAL_RING {
            self.serials.pop_front();
        }
        self.serials.push_back(serial);
        serial
    }

    /// Whether the serial is one of the last `SERIAL_RING` agent serials.
    pub fn is_agent_serial(&self, serial: Serial) -> bool {
        self.serials.contains(&serial)
    }

    /// Records the time of the owner's physical input (see `is_human_input`).
    pub fn note_human_input(&mut self, now: Instant) {
        self.last_human_input = Some(now);
    }

    /// Whether the last agent burst is newer than the owner's last physical input.
    pub fn agent_acted_last(&self) -> bool {
        // `None` sorts before any `Some`: no human input yet counts as older.
        self.last_agent_burst > self.last_human_input
    }

    /// Whether the surface belongs to the client holding the agent's shadow keyboard focus.
    pub fn is_sticky_client(&self, surface: &WlSurface) -> bool {
        self.sticky
            .as_ref()
            .is_some_and(|sticky| sticky.surface.id().same_client_as(&surface.id()))
    }

    /// Sends `leave` for the shadow keyboard focus and drops it.
    pub fn release_sticky(&mut self) {
        let Some(sticky) = self.sticky.take() else {
            return;
        };
        if !sticky.surface.is_alive() {
            return;
        }
        let serial = self.next_serial().into();
        for keyboard in &sticky.keyboards {
            keyboard.leave(serial, &sticky.surface);
        }
    }

    /// Sends `leave` for the agent pointer and drops it.
    pub fn release_pointer(&mut self) {
        let Some(focus) = self.pointer_focus.take() else {
            return;
        };
        if !focus.surface.is_alive() {
            return;
        }
        let serial = self.next_serial().into();
        for pointer in &focus.pointers {
            pointer.leave(serial, &focus.surface);
            if pointer.version() >= pointer::FRAME_SINCE {
                pointer.frame();
            }
        }
    }

    /// Runs before the seat pointer enters `surface`: the agent pointer leaves that client first,
    /// so an app never has both entered, and a parked owner pointer is back.
    pub fn yield_pointer_to(&mut self, surface: Option<&WlSurface>) {
        self.owner_pointer_parked = false;
        let same_client = |focus: &PointerFocus| {
            surface.is_some_and(|s| focus.surface.id().same_client_as(&s.id()))
        };
        if self.pointer_focus.as_ref().is_some_and(same_client) {
            self.release_pointer();
        }
    }

    /// Releases the shadow keyboard focus and the agent pointer once agents have been idle for
    /// `IDLE_RELEASE`.
    pub fn release_if_idle(&mut self, now: Instant) {
        if self
            .last_agent_burst
            .is_some_and(|burst| now.saturating_duration_since(burst) >= IDLE_RELEASE)
        {
            self.release_sticky();
            self.release_pointer();
            self.dismiss_all();
        }
    }

    /// Drops the popups that clients already destroyed.
    fn prune_popups(&mut self) {
        self.popups.retain(|_, popups| {
            popups.retain(|(popup, _)| popup.alive());
            !popups.is_empty()
        });
    }

    /// The surface of the window's topmost agent popup.
    fn topmost_popup(&mut self, id: MappedId) -> Option<WlSurface> {
        self.prune_popups();
        let (popup, _) = self.popups.get(&id)?.last()?;
        Some(popup.wl_surface().clone())
    }

    /// Sends `popup_done` to the window's agent popups, topmost first, and forgets them.
    pub fn dismiss_popups(&mut self, id: MappedId) {
        for (popup, _) in self.popups.remove(&id).into_iter().flatten().rev() {
            if let PopupKind::Xdg(popup) = popup {
                if popup.alive() {
                    popup.send_popup_done();
                }
            }
        }
    }

    /// Dismisses every agent popup.
    pub fn dismiss_all(&mut self) {
        let ids: Vec<MappedId> = self.popups.keys().copied().collect();
        for id in ids {
            self.dismiss_popups(id);
        }
    }

    /// Whether agents may act on the window: as the owner set it by keybind, or else whether its
    /// app id has the agent prefix.
    pub fn is_granted(&self, mapped: &Mapped) -> bool {
        self.granted
            .get(&mapped.id())
            .copied()
            .unwrap_or_else(|| has_agent_app_id(mapped))
    }
}

/// The agent status as IPC reports it.
pub fn status(niri: &Niri) -> AgentStatus {
    let agent = &niri.agent;
    let now = Instant::now();
    let mut granted: Vec<_> = niri
        .layout
        .windows()
        .filter(|(_, mapped)| !agent.paused && agent.is_granted(mapped))
        .map(|(_, mapped)| {
            with_toplevel_role(mapped.toplevel(), |role| AgentWindow {
                id: mapped.id().get(),
                app_id: role.app_id.clone(),
                title: role.title.clone(),
            })
        })
        .collect();
    granted.sort_by_key(|w| w.id);
    let mut acting: Vec<_> = agent
        .active_until
        .iter()
        .filter(|(_, until)| **until > now)
        .map(|(id, _)| id.get())
        .collect();
    acting.sort_unstable();
    AgentStatus {
        paused: agent.paused,
        handed_over: agent.handed_over,
        granted,
        acting,
    }
}

/// Whether the window's app id has the agent prefix. Such windows never take focus by themselves.
pub fn has_agent_app_id(mapped: &Mapped) -> bool {
    with_toplevel_role(mapped.toplevel(), |role| {
        role.app_id
            .as_deref()
            .is_some_and(|id| id.starts_with(AGENT_APP_ID_PREFIX))
    })
}

/// Keeps a popup that an agent click opened, without a grab, so the owner's focus stays put.
/// Returns false if its root isn't a layout window.
pub fn add_popup(niri: &mut Niri, root: &WlSurface, popup: PopupKind) -> bool {
    let Some((mapped, _)) = niri.layout.find_window_and_output(root) else {
        return false;
    };
    let id = mapped.id();
    let visible = is_visible(&niri.layout, id);
    niri.agent
        .popups
        .entry(id)
        .or_default()
        .push((popup, visible));
    // The idle timer may have stopped between the click and this grab request.
    keyboard::start_idle_timer(niri);
    true
}

/// Whether the window's workspace is on screen, so the owner could see its popups.
fn is_visible(layout: &Layout<Mapped>, id: MappedId) -> bool {
    layout.monitors().any(|mon| {
        mon.workspaces_with_render_geo()
            .any(|(ws, _)| ws.windows().any(|m| m.id() == id))
    })
}

/// Dismisses the agent popups the owner could see. Runs on the owner's button presses.
pub fn dismiss_visible_popups(niri: &mut Niri) {
    let layout = &niri.layout;
    let agent = &mut niri.agent;
    let visible: Vec<MappedId> = agent
        .popups
        .keys()
        .copied()
        .filter(|id| is_visible(layout, *id))
        .collect();
    for id in visible {
        agent.dismiss_popups(id);
    }
}

/// Dismisses the agent popups of windows whose workspace came into view. Runs every loop pass.
pub fn refresh_popups(niri: &mut Niri) {
    let agent = &mut niri.agent;
    if agent.popups.is_empty() {
        return;
    }
    agent.prune_popups();
    let mut shown = Vec::new();
    for (id, popups) in &mut agent.popups {
        let visible = is_visible(&niri.layout, *id);
        if visible && popups.iter().any(|(_, was_visible)| !was_visible) {
            shown.push(*id);
        }
        for (_, was_visible) in popups {
            *was_visible = visible;
        }
    }
    for id in shown {
        agent.dismiss_popups(id);
    }
}

/// Whether the input device is a virtual keyboard or virtual pointer, that is, a client's.
pub fn is_virtual_device<D: smithay::backend::input::Device + 'static>(device: &D) -> bool {
    let device: &dyn Any = device;
    device.is::<VirtualKeyboardDevice>() || device.is::<VirtualPointerDevice>()
}

/// Pushes the `is-agent-granted` and `is-agent-active` window rule flags to the windows, and
/// forgets the expired activity. Runs on every refresh, which covers grant changes, the kill
/// switch, app id changes, new windows and bursts; the idle timer keeps ticking while a window is
/// active so that the flag turns off. Nothing is granted while paused, so a border rule shows the
/// pause.
pub fn refresh_flags(niri: &mut Niri) {
    let now = Instant::now();
    niri.agent.active_until.retain(|_, until| *until > now);
    let agent = &niri.agent;
    niri.layout.with_windows_mut(|mapped, _| {
        mapped.set_is_agent_granted(!agent.paused && agent.is_granted(mapped));
        mapped.set_is_agent_active(agent.active_until.contains_key(&mapped.id()));
    });
}

/// Whether the event is the owner's: a key press, button press, scroll or touch-down from a
/// physical device. Motion and releases don't count: a release can end a press from before an
/// agent acted.
pub fn is_human_input<I: InputBackend>(event: &InputEvent<I>) -> bool
where
    I::Device: 'static,
{
    let device = match event {
        InputEvent::Keyboard { event } if event.state() == KeyState::Pressed => event.device(),
        InputEvent::PointerButton { event } if event.state() == ButtonState::Pressed => {
            event.device()
        }
        InputEvent::PointerAxis { event } => event.device(),
        InputEvent::TouchDown { event } => event.device(),
        _ => return false,
    };
    !is_virtual_device(&device)
}

/// Whether the event ends a hand-over: a button press, scroll, touch-down or tablet tip-down from a
/// physical device. Keys are decided in `on_keyboard`, which knows the modifiers and binds.
pub fn ends_handover<I: InputBackend>(event: &InputEvent<I>) -> bool
where
    I::Device: 'static,
{
    let device = match event {
        InputEvent::PointerButton { event } if event.state() == ButtonState::Pressed => {
            event.device()
        }
        InputEvent::PointerAxis { event } => event.device(),
        InputEvent::TouchDown { event } => event.device(),
        InputEvent::TabletToolTip { event } if event.tip_state() == TabletToolTipState::Down => {
            event.device()
        }
        _ => return false,
    };
    !is_virtual_device(&device)
}

/// Applies the agent guards to a new toplevel: returns its activation and workspace.
///
/// It doesn't take focus when its app id has the agent prefix, when an agent acted after the
/// owner, when its activation token carries `AgentGuardMarker` (then it turns urgent, as a mapped
/// window would), or when agents drove its client in the last `NEW_WINDOW_GUARD` and that client
/// doesn't have the owner's focus. In the last case it opens on the workspace of the window the
/// agents drove.
pub fn guard_new_window(
    niri: &Niri,
    mapped: &mut Mapped,
    token: Option<&XdgActivationTokenData>,
    activate: ActivateWindow,
    workspace: Option<WorkspaceId>,
) -> (ActivateWindow, Option<WorkspaceId>) {
    let guarded = token.is_some_and(|t| t.user_data.get::<AgentGuardMarker>().is_some());
    if guarded {
        mapped.set_urgent(true);
    }

    let now = Instant::now();
    let agent = &niri.agent;
    let owner_client = niri
        .keyboard_focus
        .surface()
        .and_then(|s| s.client())
        .map(|c| c.id());
    let driven = mapped
        .toplevel()
        .wl_surface()
        .client()
        .map(|client| client.id())
        .filter(|id| owner_client.as_ref() != Some(id))
        .filter(|id| {
            agent
                .last_burst
                .get(id)
                .is_some_and(|burst| now.saturating_duration_since(*burst) < NEW_WINDOW_GUARD)
        })
        .and_then(|id| agent.last_target.get(&id).copied());

    // While handed over, the agent acts like the owner's own clicks.
    let acted = !agent.handed_over && agent.agent_acted_last();
    let driven = driven.filter(|_| !agent.handed_over);
    let activate = if guarded || driven.is_some() || acted || has_agent_app_id(mapped) {
        ActivateWindow::No
    } else {
        activate
    };
    let driven_workspace = driven.and_then(|id| {
        niri.layout
            .workspaces()
            .find(|(_, _, ws)| ws.windows().any(|m| m.id() == id))
            .map(|(_, _, ws)| ws.id())
    });
    (activate, driven_workspace.or(workspace))
}

/// Whether the window's client is xwayland-satellite.
///
/// Niri doesn't keep the satellite's pid (the child lives in a spawner thread), so this compares
/// the client's process name in `/proc` with the configured satellite binary.
// ponytail: a process renamed to the same comm passes; track the satellite pid if that matters.
fn is_x11(niri: &Niri, mapped: &Mapped) -> bool {
    if niri.satellite.is_none() {
        return false;
    }
    let Some(pid) = get_credentials_for_surface(mapped.toplevel().wl_surface()).map(|c| c.pid)
    else {
        return false;
    };
    let config = niri.config.borrow();
    let Some(name) = Path::new(&config.xwayland_satellite.path).file_name() else {
        return false;
    };
    let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else {
        return false;
    };
    // The kernel truncates comm to 15 bytes.
    let name = name.to_string_lossy();
    let comm = comm.trim_end();
    !comm.is_empty() && name.starts_with(comm)
}

/// Whether the owner can see the window: its tile is drawn and intersects its output on a shown
/// workspace, scaled by the overview zoom (1 when the overview is closed).
pub fn is_on_screen(niri: &Niri, id: MappedId) -> bool {
    niri.layout.monitors().any(|mon| {
        let output = Rectangle::from_size(mon.view_size());
        let zoom = mon.overview_zoom();
        mon.workspaces_with_render_geo().any(|(ws, ws_geo)| {
            ws.tiles_with_render_positions()
                .filter(|(tile, ..)| tile.window().id() == id)
                .any(|(tile, pos, drawn)| {
                    drawn
                        && Rectangle::new(
                            ws_geo.loc + pos.upscale(zoom),
                            tile.tile_size().upscale(zoom),
                        )
                        .intersection(output)
                        .is_some()
                })
        })
    })
}

/// Runs the checks every input op passes, in order; the first failure wins.
pub fn check_target(niri: &Niri, id: MappedId, kind: OpKind) -> Result<Target, AgentError> {
    if niri.agent.paused {
        return Err(AgentError::Paused);
    }
    let (mapped, output) = niri
        .find_mapped_output_by_id(id)
        .ok_or(AgentError::WindowGone)?;
    let handed_over = niri.agent.handed_over;
    if !niri.agent.is_granted(mapped) {
        // A hand-over drives what the owner can see; side by side keeps to grants.
        if !handed_over {
            return Err(AgentError::NotGranted);
        }
        if !is_on_screen(niri, id) {
            return Err(AgentError::NotVisible);
        }
    }
    if is_x11(niri, mapped) {
        return Err(AgentError::X11Unsupported);
    }
    if handed_over {
        // The owner gave the controls away, so their focus and hover don't count.
        capture_precheck(niri, id)?;
    }

    let focused = match &niri.keyboard_focus {
        KeyboardFocus::Layout {
            surface: Some(surface),
        } => niri
            .layout
            .find_window_and_output(surface)
            .is_some_and(|(m, _)| m.id() == id),
        _ => false,
    };
    // In the overview the pointer only picks a tile to zoom to; it isn't the owner using the
    // window.
    let overview = niri.layout.is_overview_open();
    let hovered = !overview
        && niri
            .pointer_contents
            .window
            .as_ref()
            .is_some_and(|(window, _)| *window == mapped.window);
    if !handed_over && (focused || hovered) {
        return Err(AgentError::HumanFocused);
    }

    let toplevel = mapped.toplevel().wl_surface().clone();
    let client = toplevel.client().ok_or(AgentError::WindowGone)?;
    let human_clients = [
        niri.seat.get_keyboard().and_then(|k| k.current_focus()),
        niri.seat
            .get_pointer()
            .filter(|_| !overview)
            .and_then(|p| p.current_focus()),
    ];
    if !handed_over
        && human_clients
            .iter()
            .flatten()
            .any(|s| s.client().is_some_and(|c| c.id() == client.id()))
    {
        return Err(AgentError::SameClient);
    }

    if kind == OpKind::Pointer {
        let record = niri.agent.captures.get(&id).ok_or(AgentError::NoCapture)?;
        let (window_size, output_scale, popup_count) = window_view(mapped, output);
        if window_size != record.window_size
            || output_scale != record.output_scale
            || popup_count != record.popup_count
        {
            return Err(AgentError::StaleCapture);
        }
    }

    Ok(Target {
        id,
        toplevel,
        client,
    })
}

/// The reply to a finished agent op on the window.
pub fn agent_done(niri: &Niri, id: MappedId) -> AgentDone {
    let (app_id, title) = niri
        .find_mapped_output_by_id(id)
        .map(|(mapped, _)| {
            with_toplevel_role(mapped.toplevel(), |role| {
                (role.app_id.clone(), role.title.clone())
            })
        })
        .unwrap_or_default();
    let capture_age_ms = niri
        .agent
        .captures
        .get(&id)
        .map(|record| u64::try_from(record.at.elapsed().as_millis()).unwrap_or(u64::MAX));
    AgentDone {
        id: id.get(),
        app_id,
        title,
        capture_age_ms,
    }
}

/// Scale for a capture: the output scale, reduced so the longest edge fits in `max_edge` pixels.
pub fn capture_scale(output_scale: f64, logical: Size<f64, Logical>, max_edge: u32) -> f64 {
    output_scale.min(f64::from(max_edge) / logical.w.max(logical.h))
}

/// Sends the window frame callbacks every 16 ms until `until`, even when it is hidden or the
/// monitors are off. Boosting an already boosted window extends its boost.
pub fn boost_frames(niri: &mut Niri, id: MappedId, until: Instant) {
    if let Some(end) = niri.agent.boosts.get_mut(&id) {
        *end = (*end).max(until);
        return;
    }

    niri.agent.boosts.insert(id, until);
    niri.event_loop
        .insert_source(Timer::immediate(), move |_, _, state| {
            boost_tick(&mut state.niri, id)
        })
        .unwrap();
}

fn boost_tick(niri: &mut Niri, id: MappedId) -> TimeoutAction {
    let running = niri
        .agent
        .boosts
        .get(&id)
        .is_some_and(|end| Instant::now() < *end);
    let mut found = false;
    if running {
        niri.layout.with_windows_mut(|mapped, output| {
            if mapped.id() != id {
                return;
            }
            found = true;
            if let Some(output) = output {
                mapped.set_needs_frame_callback();
                // A `None` throttle never sends to hidden surfaces.
                let throttle = Some(Duration::ZERO);
                mapped.send_frame(output, get_monotonic_time(), throttle, |_, _| None);
            }
        });
    }

    if found {
        TimeoutAction::ToDuration(BOOST_INTERVAL)
    } else {
        niri.agent.boosts.remove(&id);
        TimeoutAction::Drop
    }
}
