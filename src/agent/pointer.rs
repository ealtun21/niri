//! Shadow pointer delivery: `click`, `move`, `scroll` and `drag`.
//!
//! Events go straight to the target client's `wl_pointer` resources, so the seat's pointer focus,
//! location and grabs never change. Each op is one burst: `enter` ... `leave`.

use async_channel::Sender;
use niri_ipc::{AgentButton, AgentDone, AgentError, AgentMod, AgentOp};
use smithay::desktop::WindowSurfaceType;
use smithay::input::keyboard::xkb::{self, Keymap};
use smithay::reexports::wayland_server::protocol::wl_pointer::{
    Axis, AxisSource, ButtonState, WlPointer,
};
use smithay::reexports::wayland_server::Resource as _;
use smithay::utils::{Logical, Point};

use super::keyboard::{enter_keyboards, finish_burst, with_keymap};
use super::{agent_done, check_target, CaptureRecord, OpKind, Target, MAX_DRAG_STEPS};
use crate::niri::State;
use crate::utils::get_monotonic_time;
use crate::window::mapped::MappedId;

/// `frame` exists since wl_pointer v5.
const FRAME_SINCE: u32 = 5;
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

/// Sends the pointer op to the target. The caller has run `check_target` for a pointer op.
pub fn deliver_pointer(
    state: &mut State,
    target: &Target,
    op: &AgentOp,
    rec: &CaptureRecord,
) -> Result<(), AgentError> {
    let (x, y, mods) = match op {
        AgentOp::Click { x, y, mods, .. } => (*x, *y, mods.as_slice()),
        AgentOp::Move { x, y } | AgentOp::Scroll { x, y, .. } => (*x, *y, &[][..]),
        AgentOp::Drag { x0, y0, .. } => (*x0, *y0, &[][..]),
        AgentOp::Key { .. } | AgentOp::Type { .. } => return Ok(()),
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
    let (surface, local) = match window.surface_under(buf, WindowSurfaceType::ALL) {
        Some((surface, offset)) => (surface, buf - offset.to_f64()),
        None => (target.toplevel.clone(), buf),
    };

    let pointers: Vec<WlPointer> = niri
        .seat
        .get_pointer()
        .unwrap()
        .client_pointers(&target.client)
        .collect();
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

    let time = get_monotonic_time().as_millis() as u32;
    let agent = &mut niri.agent;
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

    send(&|p, serial| p.enter(serial, &surface, local.x, local.y));
    frame();
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
        AgentOp::Drag {
            x0,
            y0,
            x1,
            y1,
            steps,
            ..
        } => {
            let start = capture_to_buffer(*x0, *y0, rec);
            let end = capture_to_buffer(*x1, *y1, rec);
            let code = button_code(AgentButton::Left);
            button(&mut send, code, ButtonState::Pressed);
            // Bounds what one dispatch writes to the client.
            let steps = (*steps).clamp(1, MAX_DRAG_STEPS);
            for i in 1..=steps {
                let t = f64::from(i) / f64::from(steps);
                let delta = end - start;
                motion((local.x + delta.x * t, local.y + delta.y * t).into());
            }
            button(&mut send, code, ButtonState::Released);
        }
        AgentOp::Move { .. } | AgentOp::Key { .. } | AgentOp::Type { .. } => (),
    }

    send(&|p, serial| p.leave(serial, &surface));
    frame();

    for keyboard in &keyboards {
        let serial = niri.agent.next_serial().into();
        keyboard.modifiers(serial, 0, 0, 0, group);
    }
    finish_burst(niri, target);
    Ok(())
}
