//! The `capture-window` request.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_channel::Sender;
use calloop::timer::{TimeoutAction, Timer};
use niri_ipc::{AgentError, CapturePopup, WindowCapture};
use smithay::desktop::PopupManager;
use smithay::utils::Scale;

use super::{
    boost_frames, capture_scale, window_view, CaptureRecord, DEFAULT_MAX_EDGE, DEFAULT_SETTLE,
    MAX_SETTLE,
};
use crate::layout::LayoutElement as _;
use crate::niri::{Niri, State};
use crate::utils::{with_toplevel_role, write_png_rgba8};
use crate::window::mapped::{Mapped, MappedId};

/// Checks, in order, that captures aren't paused, the window exists and it isn't blocked out.
///
/// Captures need no grant. Any `block-out-from` rule blocks them, screencast-only ones included.
pub fn capture_precheck(niri: &Niri, id: MappedId) -> Result<(), AgentError> {
    if niri.agent.paused {
        return Err(AgentError::Paused);
    }
    let (mapped, _) = niri
        .find_mapped_output_by_id(id)
        .ok_or(AgentError::WindowGone)?;
    if mapped.rules().block_out_from.is_some() {
        return Err(AgentError::BlockedOut);
    }
    Ok(())
}

/// How long a capture waits for the window to settle, at most `MAX_SETTLE`.
pub fn settle_time(settle_ms: Option<u32>) -> Duration {
    settle_ms
        .map_or(DEFAULT_SETTLE, |ms| Duration::from_millis(ms.into()))
        .min(MAX_SETTLE)
}

/// Handles `Request::CaptureWindow`.
///
/// Boosts the window's frame callbacks for the settle time, then renders it, writes the PNG off
/// the main thread and replies once the file exists. No clipboard, notification or IPC event.
pub fn capture_window(
    state: &mut State,
    id: u64,
    path: String,
    max_edge: Option<u32>,
    settle_ms: Option<u32>,
    reply: Sender<Result<WindowCapture, String>>,
) {
    let id = MappedId::specific(id);
    if let Err(err) = capture_precheck(&state.niri, id) {
        let _ = reply.send_blocking(Err(err.to_string()));
        return;
    }

    let settle = settle_time(settle_ms);
    boost_frames(&mut state.niri, id, Instant::now() + settle);

    let max_edge = max_edge.unwrap_or(DEFAULT_MAX_EDGE).max(1);
    state
        .niri
        .event_loop
        .insert_source(Timer::from_duration(settle), move |_, _, state| {
            let reply = reply.clone();
            match render(state, id, path.clone(), max_edge) {
                Ok((capture, pixels)) => {
                    thread::spawn(move || {
                        let result = write_png(&capture, &pixels)
                            .map(|()| capture)
                            .map_err(|err| format!("{err:#}"));
                        let _ = reply.send_blocking(result);
                    });
                }
                Err(err) => {
                    let _ = reply.send_blocking(Err(format!("{err:#}")));
                }
            }
            TimeoutAction::Drop
        })
        .unwrap();
}

/// Popup surface origins and geometry sizes, relative to the window's main surface origin.
///
/// `popups_for_surface` yields the popup geometry location relative to the window geometry, so
/// the surface origin is that plus the window geometry location minus the popup geometry
/// location, the same placement `render_popups` uses.
pub fn capture_popups(mapped: &Mapped) -> Vec<CapturePopup> {
    let geo_loc = mapped.window.geometry().loc;
    PopupManager::popups_for_surface(mapped.toplevel().wl_surface())
        .map(|(popup, offset)| {
            let geo = popup.geometry();
            let origin = geo_loc + offset - geo.loc;
            CapturePopup {
                x: f64::from(origin.x),
                y: f64::from(origin.y),
                width: f64::from(geo.size.w),
                height: f64::from(geo.size.h),
            }
        })
        .collect()
}

/// Renders the window, stores its `CaptureRecord` and returns the reply and the pixels.
fn render(
    state: &mut State,
    id: MappedId,
    path: String,
    max_edge: u32,
) -> anyhow::Result<(WindowCapture, Vec<u8>)> {
    // Things may have changed while settling.
    capture_precheck(&state.niri, id)?;

    let niri = &state.niri;
    let (mapped, output) = niri
        .find_mapped_output_by_id(id)
        .ok_or(AgentError::WindowGone)?;

    let (window_size, output_scale, popup_count) = window_view(mapped, output);
    let bbox = mapped.window.bbox_with_popups().size.to_f64();
    let scale = capture_scale(output_scale, bbox, max_edge);
    let (size, pixels, origin) = state
        .backend
        .with_primary_renderer(|renderer| {
            niri.render_window_capture(renderer, mapped, Scale::from(scale))
        })
        .context("no renderer")?
        .context("error rendering window")?;

    let geometry = mapped.window.geometry();
    let (app_id, title) = with_toplevel_role(mapped.toplevel(), |role| {
        (role.app_id.clone(), role.title.clone())
    });
    let workspace_visible = niri.layout.workspaces().any(|(mon, idx, ws)| {
        mon.is_some_and(|mon| mon.active_workspace_idx() == idx) && ws.has_window(&mapped.window)
    });
    let record = CaptureRecord {
        origin,
        scale,
        size,
        window_size,
        output_scale,
        popup_count,
        at: Instant::now(),
    };
    let capture = WindowCapture {
        id: id.get(),
        path,
        width: size.w as u32,
        height: size.h as u32,
        scale,
        origin: (f64::from(origin.x) / scale, f64::from(origin.y) / scale),
        geometry_loc: (f64::from(geometry.loc.x), f64::from(geometry.loc.y)),
        window_size: (geometry.size.w, geometry.size.h),
        workspace_visible,
        app_id,
        title,
        popups: capture_popups(mapped),
    };

    state.niri.agent.captures.insert(id, record);
    Ok((capture, pixels))
}

fn write_png(capture: &WindowCapture, pixels: &[u8]) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    write_png_rgba8(&mut buf, capture.width, capture.height, pixels)
        .context("error encoding PNG")?;
    std::fs::write(&capture.path, buf).context("error writing PNG")
}
