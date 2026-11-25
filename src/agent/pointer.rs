//! Shadow pointer delivery: `click`, `move`, `scroll` and `drag`.
//!
//! Events go straight to the target client's `wl_pointer` resources, so the seat's location and
//! grabs never change. The agent pointer stays entered on the surface it last used, so hover
//! state persists; see `AgentState::pointer_focus`. A hand-over takes the seat pointer focus off
//! the client first, so an app never has both pointers entered.

use async_channel::Sender;
use niri_ipc::{AgentButton, AgentDone, AgentError, AgentMod, AgentOp};
use smithay::backend::input::InputTime;
use smithay::desktop::WindowSurfaceType;
use smithay::input::keyboard::xkb::{self, Keymap};
use smithay::input::pointer::MotionEvent;
use smithay::reexports::wayland_server::protocol::wl_pointer::{
    Axis, AxisSource, ButtonState, WlPointer,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource as _};
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use super::cursor::place_on_window;
use super::keyboard::{enter_keyboards, finish_burst, with_keymap};
use super::{
    agent_done, check_target, CaptureRecord, OpKind, PointerFocus, Target, MAX_DRAG_STEPS,
};
use crate::niri::State;
use crate::utils::get_monotonic_time;
use crate::window::mapped::MappedId;

/// `frame` exists since wl_pointer v5.
pub const FRAME_SINCE: u32 = 5;
/// `axis_value120` exists since wl_pointer v8.
const VALUE120_SINCE: u32 = 8;
/// Axis value (surface-local px) of one wheel notch.
const PX_PER_NOTCH: f64 = 15.;

/// Sends one event, with a fresh agent serial, to every pointer of the target client.
type Emit<'a> = dyn FnMut(&dyn Fn(&WlPointer, u32)) + 'a;

/// Maps a point of the last capture image to the main surface's logical coordinates.
pub fn capture_to_buffer(px: f64, py: f64, rec: &CaptureRecord) -> Point<f64, Logical> {
    (
        (px + f64::from(rec.origin.x)) / rec.scale,
        (py + f64::from(rec.origin.y)) / rec.scale,
    )
        .into()
}

/// Handles the pointer ops.
pub fn pointer(
    state: &mut State,
    id: MappedId,
    op: &AgentOp,
    reply: Sender<Result<AgentDone, AgentError>>,
) {
    // A focus bind earlier in this loop pass hasn't reached keyboard_focus yet.
    state.update_keyboard_focus();
    let result = check_target(&state.niri, id, OpKind::Pointer).and_then(|target| {
        // Middle-click pastes the primary selection, which can hold the owner's text; a hand-over
        // allows it.
        if !state.niri.agent.handed_over
            && matches!(
                op,
                AgentOp::Click {
                    button: AgentButton::Middle,
                    ..
                }
            )
        {
            return Err(AgentError::PasteUnsupported);
        }
        let rec = *state
            .niri
            .agent
            .captures
            .get(&id)
            .ok_or(AgentError::NoCapture)?;
        deliver_pointer(state, &target, op, &rec)
    });
    let _ = reply.send_blocking(result.map(|()| agent_done(&state.niri, id)));
}

fn button_code(button: AgentButton) -> u32 {
    match button {
        AgentButton::Left => 0x110,
        AgentButton::Right => 0x111,
        AgentButton::Middle => 0x112,
    }
}

fn mod_mask(keymap: &Keymap, mods: &[AgentMod]) -> u32 {
    mods.iter()
        .filter_map(|m| {
            let name = match m {
                AgentMod::Ctrl => xkb::MOD_NAME_CTRL,
                AgentMod::Shift => xkb::MOD_NAME_SHIFT,
                AgentMod::Alt => xkb::MOD_NAME_ALT,
                AgentMod::Super => xkb::MOD_NAME_LOGO,
            };
            1u32.checked_shl(keymap.mod_get_index(name))
        })
        .fold(0, |a, b| a | b)
}

/// Where the op starts, in its own coordinates; `None` for keyboard ops.
fn start_point(op: &AgentOp) -> Option<(f64, f64)> {
    match op {
        AgentOp::Click { x, y, .. } | AgentOp::Move { x, y } | AgentOp::Scroll { x, y, .. } => {
            Some((*x, *y))
        }
        AgentOp::Drag { x0, y0, .. } => Some((*x0, *y0)),
        AgentOp::Key { .. } | AgentOp::Type { .. } => None,
    }
}

/// Sends the pointer op to the target. The caller has run `check_target` for a pointer op.
pub fn deliver_pointer(
    state: &mut State,
    target: &Target,
    op: &AgentOp,
    rec: &CaptureRecord,
) -> Result<(), AgentError> {
    let Some((x, y)) = start_point(op) else {
        return Ok(());
    };
    let mods = match op {
        AgentOp::Click { mods, .. } => mods.as_slice(),
        _ => &[],
    };
    let (mask, group) = if mods.is_empty() {
        (0, 0)
    } else {
        // Read-only: the group the owner's seat would send to this client.
        let group = {
            let keyboard = state.niri.seat.get_keyboard().unwrap();
            keyboard.with_xkb_state(state, |ctx| ctx.xkb().lock().unwrap().active_layout().0)
        };
        (with_keymap(state, |keymap| mod_mask(keymap, mods)), group)
    };

    let niri = &mut state.niri;
    // Look the window up again: it may have closed or been replaced since the checks.
    let window = niri
        .find_mapped_output_by_id(target.id)
        .map(|(mapped, _)| mapped.window.clone())
        .ok_or(AgentError::WindowGone)?;
    let buf = capture_to_buffer(x, y, rec);
    let (surface, offset) = match window.surface_under(buf, WindowSurfaceType::ALL) {
        Some((surface, offset)) => (surface, offset.to_f64()),
        None => (target.toplevel.clone(), Point::default()),
    };

    let keyboards = if mods.is_empty() {
        Vec::new()
    } else {
        // Clients drop modifiers sent to a keyboard that hasn't had `enter`.
        let keyboards = enter_keyboards(niri, target);
        let serial = niri.agent.next_serial().into();
        for keyboard in &keyboards {
            keyboard.modifiers(serial, mask, 0, 0, group);
        }
        keyboards
    };

    let to_local = |x, y| capture_to_buffer(x, y, rec) - offset;
    let result = deliver_to_surface(state, &target.client, &surface, &to_local, op);

    let niri = &mut state.niri;
    for keyboard in &keyboards {
        let serial = niri.agent.next_serial().into();
        keyboard.modifiers(serial, 0, 0, 0, group);
    }
    if result.is_ok() {
        place_on_window(niri, target.id, buf);
        if let AgentOp::Drag { x1, y1, .. } = op {
            place_on_window(niri, target.id, capture_to_buffer(*x1, *y1, rec));
        }
    }
    // The modifiers went out even if the pointer part failed: that is still a burst.
    if result.is_ok() || !keyboards.is_empty() {
        finish_burst(niri, target);
    }
    result
}

/// While handed over, takes the seat pointer off the client so the agent pointer can enter it: an
/// app never has both entered. The owner's next motion, or the end of the hand-over, brings it
/// back. Fails with `SameClient` when a grab (a held button, a drag) keeps the seat pointer there.
fn park_owner_pointer(state: &mut State, client: &Client) -> Result<(), AgentError> {
    let pointer = state.niri.seat.get_pointer().unwrap();
    let on_client = || {
        pointer
            .current_focus()
            .and_then(|s| s.client())
            .is_some_and(|c| c.id() == client.id())
    };
    if !state.niri.agent.handed_over || !on_client() {
        return Ok(());
    }
    let event = MotionEvent {
        location: pointer.current_location(),
        serial: SERIAL_COUNTER.next_serial(),
        time: InputTime::now(),
    };
    pointer.motion(state, None, &event);
    pointer.frame(state);
    if on_client() {
        return Err(AgentError::SameClient);
    }
    state.niri.agent.owner_pointer_parked = true;
    Ok(())
}

/// Sends the pointer op to `surface` of `client`; `to_local` maps op coordinates to the surface.
/// Enters the surface unless the agent pointer is already on it, leaving any other surface first,
/// and stays entered. Window callers run `finish_burst` after it.
pub fn deliver_to_surface(
    state: &mut State,
    client: &Client,
    surface: &WlSurface,
    to_local: &dyn Fn(f64, f64) -> Point<f64, Logical>,
    op: &AgentOp,
) -> Result<(), AgentError> {
    let Some((x, y)) = start_point(op) else {
        return Ok(());
    };
    let local = to_local(x, y);
    park_owner_pointer(state, client)?;

    let niri = &mut state.niri;
    let pointers: Vec<WlPointer> = niri
        .seat
        .get_pointer()
        .unwrap()
        .client_pointers(client)
        .collect();

    let time = get_monotonic_time().as_millis() as u32;
    let agent = &mut niri.agent;
    if agent
        .pointer_focus
        .as_ref()
        .is_some_and(|focus| focus.surface != *surface)
    {
        agent.release_pointer();
    }
    let entered = agent
        .pointer_focus
        .take()
        .map_or_else(Vec::new, |focus| focus.pointers);
    // Also catches pointers the client made after the first enter.
    let new: Vec<&WlPointer> = pointers.iter().filter(|p| !entered.contains(p)).collect();
    if !new.is_empty() {
        let serial = agent.next_serial().into();
        for pointer in new {
            pointer.enter(serial, surface, local.x, local.y);
            if pointer.version() >= FRAME_SINCE {
                pointer.frame();
            }
        }
    }
    agent.pointer_focus = Some(PointerFocus {
        surface: surface.clone(),
        pointers: pointers.clone(),
    });

    // Only `enter`, `leave` and `button` carry a serial; the others don't use up the serial ring.
    let mut send = |event: &dyn Fn(&WlPointer, u32)| {
        let serial = agent.next_serial().into();
        for pointer in &pointers {
            event(pointer, serial);
        }
    };
    let emit = |event: &dyn Fn(&WlPointer)| {
        for pointer in &pointers {
            event(pointer);
        }
    };
    let frame = || {
        emit(&|p| {
            if p.version() >= FRAME_SINCE {
                p.frame();
            }
        });
    };
    let motion = |at: Point<f64, Logical>| {
        emit(&|p| p.motion(time, at.x, at.y));
        frame();
    };

    motion(local);

    let button = |send: &mut Emit, code: u32, state| {
        send(&|p, serial| p.button(serial, time, code, state));
        frame();
    };
    match op {
        AgentOp::Click {
            button: b, count, ..
        } => {
            for _ in 0..(*count).max(1) {
                button(&mut send, button_code(*b), ButtonState::Pressed);
                button(&mut send, button_code(*b), ButtonState::Released);
            }
        }
        AgentOp::Scroll { dx, dy, .. } => {
            for (axis, d) in [(Axis::HorizontalScroll, *dx), (Axis::VerticalScroll, *dy)] {
                if d == 0. {
                    continue;
                }
                emit(&|p| {
                    if p.version() >= FRAME_SINCE {
                        p.axis_source(AxisSource::Wheel);
                    }
                    if p.version() >= VALUE120_SINCE {
                        p.axis_value120(axis, (120. * d) as i32);
                    }
                    p.axis(time, axis, PX_PER_NOTCH * d);
                });
                frame();
            }
        }
        AgentOp::Drag { x1, y1, steps, .. } => {
            let end = to_local(*x1, *y1);
            let code = button_code(AgentButton::Left);
            button(&mut send, code, ButtonState::Pressed);
            // Bounds what one dispatch writes to the client.
            let steps = (*steps).clamp(1, MAX_DRAG_STEPS);
            let delta = end - local;
            for i in 1..=steps {
                let t = f64::from(i) / f64::from(steps);
                motion((local.x + delta.x * t, local.y + delta.y * t).into());
            }
            button(&mut send, code, ButtonState::Released);
        }
        AgentOp::Move { .. } | AgentOp::Key { .. } | AgentOp::Type { .. } => (),
    }
    Ok(())
}
