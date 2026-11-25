//! Shadow keyboard delivery: `key` and `type`.
//!
//! Events go straight to the target client's `wl_keyboard` resources, so the seat focus, the xkb
//! state, grabs, text input and the selection never change.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_channel::Sender;
use calloop::timer::{TimeoutAction, Timer};
use niri_ipc::{AgentDone, AgentError};
use smithay::input::keyboard::xkb::{self, Keymap, Keysym};
use smithay::input::keyboard::SerializedMods;
use smithay::reexports::wayland_server::protocol::wl_keyboard::{KeyState, WlKeyboard};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource as _};

use super::{
    agent_done, boost_frames, check_target, OpKind, Sticky, Target, ACTIVE_INDICATOR, BURST_BOOST,
    TYPE_CHUNK_CHARS, TYPE_CHUNK_INTERVAL,
};
use crate::niri::{Niri, State};
use crate::utils::get_monotonic_time;
use crate::window::mapped::MappedId;

/// How often to check for an idle shadow keyboard focus.
const IDLE_CHECK: Duration = Duration::from_secs(1);

type Reply<T = AgentDone> = Sender<Result<T, AgentError>>;

/// Runs a `key` or `type` op's checks and returns where the next chunk goes.
pub type Check = Box<dyn Fn(&mut State) -> Result<Target, AgentError>>;

/// What a `key` or `type` op checks before each chunk, and its reply once every stroke went out.
pub struct ChunkedOp<T> {
    pub check: Check,
    pub done: Box<dyn FnOnce(&Niri) -> T>,
    pub reply: Reply<T>,
}

/// A key press with the layout and modifiers it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyStroke {
    /// Evdev keycode (xkb keycode - 8).
    pub keycode: u32,
    /// Xkb layout index (group).
    pub layout: u32,
    /// Xkb modifier mask.
    pub mods: u32,
}

/// Finds the key that types the character, in any layout of the keymap.
pub fn lookup_char(keymap: &Keymap, ch: char) -> Option<KeyStroke> {
    let sym = match ch {
        '\n' => Keysym::Return,
        '\t' => Keysym::Tab,
        _ => xkb::utf32_to_keysym(ch.into()),
    };
    lookup_keysym(keymap, sym)
}

/// Finds the key for the keysym, preferring layout 0, then the lowest level.
fn lookup_keysym(keymap: &Keymap, sym: Keysym) -> Option<KeyStroke> {
    if sym == Keysym::NoSymbol {
        return None;
    }

    let mut best = None;
    keymap.key_for_each(|keymap, code| {
        for layout in 0..keymap.num_layouts_for_key(code) {
            for level in 0..keymap.num_levels_for_key(code, layout) {
                // Keycodes come in increasing order, so ties keep the lowest one.
                let better = best.is_none_or(|(l, v, _)| (layout, level) < (l, v));
                if better
                    && keymap
                        .key_get_syms_by_level(code, layout, level)
                        .contains(&sym)
                {
                    best = Some((layout, level, code));
                }
            }
        }
    });
    let (layout, level, code) = best?;

    let mut masks = [0; 8];
    let count = keymap.key_get_mods_for_level(code, layout, level, &mut masks);
    let mods = masks.get(..count)?.iter().copied().min()?;
    Some(KeyStroke {
        keycode: code.raw().checked_sub(8)?,
        layout,
        mods,
    })
}

/// Parses a chord such as `ctrl+shift+Return`: `ctrl`, `shift`, `alt` and `super` modifiers,
/// then a keysym name. Paste chords are refused unless `allow_paste`.
pub fn parse_chord(
    keymap: &Keymap,
    chord: &str,
    allow_paste: bool,
) -> Result<KeyStroke, AgentError> {
    let bad = || AgentError::BadChord(chord.to_owned());
    let mask = |name: &str| 1u32.checked_shl(keymap.mod_get_index(name)).ok_or_else(bad);

    let mut parts: Vec<&str> = chord.split('+').collect();
    // keysym_from_name() panics on NUL.
    let name = parts
        .pop()
        .filter(|name| !name.is_empty() && !name.contains('\0'))
        .ok_or_else(bad)?;

    let mut mods = 0;
    for part in parts {
        mods |= match part.to_ascii_lowercase().as_str() {
            "ctrl" => mask(xkb::MOD_NAME_CTRL)?,
            "shift" => mask(xkb::MOD_NAME_SHIFT)?,
            "alt" => mask(xkb::MOD_NAME_ALT)?,
            "super" => mask(xkb::MOD_NAME_LOGO)?,
            _ => return Err(bad()),
        };
    }

    let mut sym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
    if sym == Keysym::NoSymbol {
        sym = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
    }
    let mut stroke = lookup_keysym(keymap, sym).ok_or_else(bad)?;
    stroke.mods |= mods;

    let ctrl = mask(xkb::MOD_NAME_CTRL)?;
    let shift = mask(xkb::MOD_NAME_SHIFT)?;
    let held = stroke.mods & (ctrl | shift | mask(xkb::MOD_NAME_ALT)? | mask(xkb::MOD_NAME_LOGO)?);
    let is_v = sym == Keysym::v || sym == Keysym::V;
    let is_paste = (is_v && (held == ctrl || held == ctrl | shift))
        || (sym == Keysym::Insert && held == shift);
    if is_paste && !allow_paste {
        return Err(AgentError::PasteUnsupported);
    }

    Ok(stroke)
}

pub fn with_keymap<T>(state: &mut State, mut f: impl FnMut(&Keymap) -> T) -> T {
    let keyboard = state.niri.seat.get_keyboard().unwrap();
    keyboard.with_xkb_state(state, |context| {
        let xkb = context.xkb().lock().unwrap();
        // SAFETY: we're not changing the ref count.
        f(unsafe { xkb.keymap() })
    })
}

/// Handles the `key` op: presses the chord `repeat` times.
pub fn key(state: &mut State, id: MappedId, chord: &str, repeat: u16, reply: Reply) {
    let allow_paste = state.niri.agent.handed_over;
    let strokes = chord_strokes(state, chord, repeat, allow_paste);
    // Paste chords stop when the hand-over ends.
    let handover = allow_paste.then_some(AgentError::PasteUnsupported);
    start(state, strokes, window_op(id, handover, reply));
}

/// Handles the `type` op.
pub fn type_text(state: &mut State, id: MappedId, text: &str, reply: Reply) {
    let strokes = text_strokes(state, text);
    start(state, strokes, window_op(id, None, reply));
}

/// The strokes that press the chord `repeat` times.
pub fn chord_strokes(
    state: &mut State,
    chord: &str,
    repeat: u16,
    allow_paste: bool,
) -> Result<Vec<KeyStroke>, AgentError> {
    let stroke = with_keymap(state, |keymap| parse_chord(keymap, chord, allow_paste))?;
    Ok(vec![stroke; usize::from(repeat.max(1))])
}

/// The strokes that type the text. Fails if a character has no key.
pub fn text_strokes(state: &mut State, text: &str) -> Result<Vec<KeyStroke>, AgentError> {
    with_keymap(state, |keymap| {
        // Each lookup scans the whole keymap; text repeats few characters many times.
        let mut found = HashMap::new();
        text.chars()
            .map(|ch| {
                let stroke = *found.entry(ch).or_insert_with(|| lookup_char(keymap, ch));
                stroke.ok_or(AgentError::UnmappableChar(ch))
            })
            .collect::<Result<Vec<_>, _>>()
    })
}

/// A window `key` or `type`: the window's guards before each chunk, then, when `handover` is
/// set, that the hand-over goes on, failing with `handover` once it ended.
fn window_op(id: MappedId, handover: Option<AgentError>, reply: Reply) -> ChunkedOp<AgentDone> {
    ChunkedOp {
        check: Box::new(move |state| {
            let target = check_target(&state.niri, id, OpKind::Keyboard)?;
            match &handover {
                Some(error) if !state.niri.agent.handed_over => Err(error.clone()),
                _ => Ok(target),
            }
        }),
        done: Box::new(move |niri| agent_done(niri, id)),
        reply,
    }
}

/// Delivers the strokes of a `key` or `type` op, or replies with the error that made none.
pub fn start<T: 'static>(
    state: &mut State,
    strokes: Result<Vec<KeyStroke>, AgentError>,
    op: ChunkedOp<T>,
) {
    match strokes {
        Ok(strokes) => deliver_chunked(state, strokes, 0, op),
        Err(err) => {
            let _ = op.reply.send_blocking(Err(err));
        }
    }
}

/// Delivers `strokes[sent..]`, `TYPE_CHUNK_CHARS` at a time with `TYPE_CHUNK_INTERVAL` in
/// between, running the op's checks before each chunk.
fn deliver_chunked<T: 'static>(
    state: &mut State,
    strokes: Vec<KeyStroke>,
    sent: usize,
    op: ChunkedOp<T>,
) {
    // A focus bind earlier in this loop pass hasn't reached keyboard_focus yet; refresh does that
    // only after this callback.
    state.update_keyboard_focus();
    let target = match (op.check)(state) {
        Ok(target) => target,
        Err(error) => {
            let error = if sent == 0 {
                error
            } else {
                AgentError::Interrupted {
                    error: Box::new(error),
                    typed: sent,
                }
            };
            let _ = op.reply.send_blocking(Err(error));
            return;
        }
    };

    let end = strokes.len().min(sent + TYPE_CHUNK_CHARS);
    // An empty `type` sends nothing, not even `enter`, and isn't a burst.
    if end > sent {
        deliver_keys(state, &target, &strokes[sent..end]);
    }

    if end < strokes.len() {
        // ponytail: pacing at 5k strokes/s gives a slow client time to read, but one that stops
        // reading for ~3 s still overflows its buffer (1 MiB in libwayland plus ~200 KB in the
        // kernel, about 16k strokes at 76 bytes each) and is disconnected. That can be any
        // granted window, including an owner window granted by keybind. Upgrade: wait for the
        // target's commit between chunks.
        let mut next = Some((strokes, op));
        let timer = Timer::from_duration(TYPE_CHUNK_INTERVAL);
        state
            .niri
            .event_loop
            .insert_source(timer, move |_, _, state| {
                if let Some((strokes, op)) = next.take() {
                    deliver_chunked(state, strokes, end, op);
                }
                TimeoutAction::Drop
            })
            .unwrap();
    } else {
        let ChunkedOp { done, reply, .. } = op;
        let _ = reply.send_blocking(Ok(done(&state.niri)));
    }
}

/// Gives the target's keyboards the shadow focus, sending `enter` to those that lack it, and
/// returns them. The focus goes to the window's topmost agent popup, as its grab would send it,
/// or else to the target's surface. Leaves any other surface's shadow focus first.
pub fn enter_keyboards(niri: &mut Niri, target: &Target) -> Vec<WlKeyboard> {
    let surface = target
        .id
        .and_then(|id| niri.agent.topmost_popup(id))
        .unwrap_or_else(|| target.surface.clone());
    enter_keyboards_on(niri, &target.client, surface)
}

/// Gives `surface` the shadow focus of its client's keyboards, sending `enter` to those that lack
/// it, and returns them. Leaves any other surface's shadow focus first. When the seat focuses
/// `surface`, its keyboards are already entered: they get no `enter` and no shadow focus, so the
/// agent never sends the `leave` that is the seat's to send. When the seat focuses another
/// surface of the client, that one gets `leave` first, as when a popup grab takes the keyboard,
/// so the client never has two surfaces entered; the release gives it back.
pub fn enter_keyboards_on(niri: &mut Niri, client: &Client, surface: WlSurface) -> Vec<WlKeyboard> {
    let seat_keyboard = niri.seat.get_keyboard().unwrap();
    let keyboards: Vec<WlKeyboard> = seat_keyboard.client_keyboards(client).collect();
    let seat_focus = seat_keyboard.current_focus();
    let agent = &mut niri.agent;

    if agent
        .sticky
        .as_ref()
        .is_some_and(|sticky| sticky.surface != surface)
    {
        agent.release_sticky();
    }
    if seat_focus.as_ref() == Some(&surface) {
        agent.sticky = None;
        return keyboards;
    }
    let (entered, restore) = match agent.sticky.take() {
        Some(sticky) => (sticky.keyboards, sticky.restore),
        None => {
            let restore = seat_focus.filter(|s| s.client().is_some_and(|c| c.id() == client.id()));
            if let Some(seat_surface) = &restore {
                let serial = agent.next_serial().into();
                for keyboard in &keyboards {
                    keyboard.leave(serial, seat_surface);
                }
            }
            (Vec::new(), restore.map(|s| (s, seat_keyboard)))
        }
    };
    let serial = agent.next_serial().into();
    // Also catches keyboards the client made after the first enter.
    for keyboard in keyboards.iter().filter(|k| !entered.contains(k)) {
        keyboard.enter(serial, &surface, Vec::new());
    }
    agent.sticky = Some(Sticky {
        surface,
        keyboards: keyboards.clone(),
        restore,
    });
    keyboards
}

/// Ends an agent burst into the client's keyboards. While the seat focuses the client, they get
/// the seat's modifiers back, since the seat sends its own only when they change; otherwise none,
/// in `layout`, until the seat's `enter` sends the real ones.
pub fn rest_modifiers(niri: &mut Niri, client: &Client, keyboards: &[WlKeyboard], layout: u32) {
    if keyboards.is_empty() {
        return;
    }
    let seat = niri.seat.get_keyboard().unwrap();
    let seat_on_client = seat
        .current_focus()
        .and_then(|s| s.client())
        .is_some_and(|c| c.id() == client.id());
    let mods = if seat_on_client {
        seat.modifier_state().serialized
    } else {
        SerializedMods {
            layout_effective: layout,
            ..SerializedMods::default()
        }
    };
    let serial = niri.agent.next_serial().into();
    for keyboard in keyboards {
        keyboard.modifiers(
            serial,
            mods.depressed,
            mods.latched,
            mods.locked,
            mods.layout_effective,
        );
    }
}

/// Sends the strokes to the target's keyboards, entering it first unless it already has the
/// shadow focus. Every pressed key is released, and the modifiers rest at the end.
pub fn deliver_keys(state: &mut State, target: &Target, strokes: &[KeyStroke]) {
    let niri = &mut state.niri;
    let keyboards = enter_keyboards(niri, target);
    let agent = &mut niri.agent;

    let time = get_monotonic_time().as_millis() as u32;
    let mut send = |event: &dyn Fn(&WlKeyboard, u32)| {
        let serial = agent.next_serial().into();
        for keyboard in &keyboards {
            event(keyboard, serial);
        }
    };
    for stroke in strokes {
        send(&|k, serial| k.modifiers(serial, stroke.mods, 0, 0, stroke.layout));
        send(&|k, serial| k.key(serial, time, stroke.keycode, KeyState::Pressed));
        send(&|k, serial| k.key(serial, time, stroke.keycode, KeyState::Released));
    }
    let layout = strokes.last().map_or(0, |stroke| stroke.layout);
    rest_modifiers(niri, &target.client, &keyboards, layout);

    finish_burst(niri, target);
}

/// Bookkeeping after an agent burst into the target: `last_agent_burst` and the idle release, and
/// for a window the activity indicator, the other `last_*` and a frame boost.
pub fn finish_burst(niri: &mut Niri, target: &Target) {
    let agent = &mut niri.agent;
    let now = Instant::now();
    agent.last_agent_burst = Some(now);
    if let Some(id) = target.id {
        agent.active_until.insert(id, now + ACTIVE_INDICATOR);
        agent.last_burst.insert(target.client.id(), now);
        agent.last_target.insert(target.client.id(), id);
        boost_frames(niri, id, now + BURST_BOOST);
    }
    start_idle_timer(niri);
}

/// Checks every `IDLE_CHECK` whether to release the shadow focus, the agent pointer and agent
/// popups, while there are any.
pub(super) fn start_idle_timer(niri: &mut Niri) {
    if niri.agent.idle_timer {
        return;
    }
    niri.agent.idle_timer = true;
    niri.event_loop
        .insert_source(Timer::from_duration(IDLE_CHECK), |_, _, state| {
            let agent = &mut state.niri.agent;
            agent.release_if_idle(Instant::now());
            super::cursor::hide_if_idle(&mut state.niri);
            let agent = &mut state.niri.agent;
            if agent.cursor.is_some()
                || agent.sticky.is_some()
                || agent.pointer_focus.is_some()
                || !agent.popups.is_empty()
                || !agent.layer_popups.is_empty()
                || !agent.active_until.is_empty()
            {
                TimeoutAction::ToDuration(IDLE_CHECK)
            } else {
                agent.idle_timer = false;
                TimeoutAction::Drop
            }
        })
        .unwrap();
}
