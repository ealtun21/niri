//! The `capture-output` request.

use std::thread;
use std::time::Instant;

use anyhow::Context as _;
use async_channel::Sender;
use calloop::timer::{TimeoutAction, Timer};
use niri_ipc::{
    AgentButton, AgentError, AgentOp, CapturePopup, OutputCapture, ScreenHit, ScreenLayer,
    ScreenWindow,
};
use smithay::backend::allocator::Fourcc;
use smithay::desktop::layer_map_for_output;
use smithay::output::Output;
use smithay::reexports::wayland_server::Resource as _;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::shell::wlr_layer::Layer;

use super::capture::{capture_popups, settle_time};
use super::cursor::move_cursor;
use super::keyboard::{
    chord_strokes, enter_keyboards_on, finish_burst, start, text_strokes, ChunkedOp,
};
use super::pointer::{deliver_window, deliver_with_mods, start_point};
use super::{boost_frames, capture_scale, check_target, OpKind, Target, DEFAULT_MAX_EDGE};
use crate::input::move_grab::overview_activate;
use crate::niri::{KeyboardFocus, Niri, PointContents, State};
use crate::render_helpers::{render_to_vec, RenderCtx, RenderTarget};
use crate::utils::{get_credentials_for_surface, write_png_rgba8};
use crate::window::mapped::MappedId;
use crate::window::Mapped;

/// Most wheel notches an agent scroll runs in the overview: more than the whole zoom range takes.
const MAX_WHEEL_NOTCHES: f64 = 30.;

/// What an output capture looked like: the visible windows' rects, to map its pixels back to them.
#[derive(Debug, Clone)]
pub struct ScreenRecord {
    /// Physical pixels per logical pixel in the image.
    pub scale: f64,
    /// Image size.
    pub size: Size<i32, Physical>,
    /// Scale of the output at capture time.
    pub output_scale: f64,
    /// Output size in logical pixels.
    pub output_size: Size<i32, Logical>,
    pub overview_open: bool,
    /// Visible windows, topmost first, with their tile rects local to the output.
    pub windows: Vec<(MappedId, Rectangle<f64, Logical>)>,
    pub at: Instant,
}

/// Checks, in order, that captures aren't paused and the output exists, and returns the output:
/// the named one, else the agent cursor's, else the active one.
pub fn output_precheck(niri: &Niri, name: Option<&str>) -> Result<Output, AgentError> {
    if niri.agent.paused {
        return Err(AgentError::Paused);
    }
    let output = match name {
        Some(name) => niri.output_by_name_match(name),
        None => niri
            .agent
            .cursor
            .as_ref()
            .map(|cursor| &cursor.output)
            .or_else(|| niri.layout.active_output()),
    };
    output
        .cloned()
        .ok_or_else(|| AgentError::BadOutput(name.unwrap_or_default().to_owned()))
}

/// Handles `Request::CaptureOutput`.
///
/// Boosts the visible windows' frame callbacks for the settle time, then renders the output
/// without any cursor, writes the PNG off the main thread and replies once the file exists.
pub fn capture_output(
    state: &mut State,
    output: Option<String>,
    path: String,
    max_edge: Option<u32>,
    settle_ms: Option<u32>,
    reply: Sender<Result<OutputCapture, String>>,
) {
    let output = match output_precheck(&state.niri, output.as_deref()) {
        Ok(output) => output,
        Err(err) => {
            let _ = reply.send_blocking(Err(err.to_string()));
            return;
        }
    };

    let max_edge = max_edge.unwrap_or(DEFAULT_MAX_EDGE).max(1);
    let settle = settle_time(settle_ms);
    let until = Instant::now() + settle;
    let ids: Vec<MappedId> = visible_tiles(&state.niri, &output)
        .iter()
        .map(|tile| tile.mapped.id())
        .collect();
    for id in ids {
        boost_frames(&mut state.niri, id, until);
    }

    let name = output.name();
    state
        .niri
        .event_loop
        .insert_source(Timer::from_duration(settle), move |_, _, state| {
            let reply = reply.clone();
            match render(state, &name, path.clone(), max_edge) {
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

/// A window drawn on an output.
struct VisibleTile<'a> {
    mapped: &'a Mapped,
    /// Tile rect local to the output.
    rect: Rectangle<f64, Logical>,
    /// Where the main surface origin is, local to the output.
    origin: Point<f64, Logical>,
    zoom: f64,
}

/// The drawn tiles that intersect `output`, scaled by the overview zoom, the same set as
/// `is_on_screen`. Topmost first: floating ones come first, as they render above the tiled ones.
fn visible_tiles<'a>(niri: &'a Niri, output: &Output) -> Vec<VisibleTile<'a>> {
    let mut tiles = Vec::new();
    for mon in niri.layout.monitors().filter(|mon| mon.output() == output) {
        let on_output = Rectangle::from_size(mon.view_size());
        let zoom = mon.overview_zoom();
        for (ws, geo) in mon.workspaces_with_render_geo() {
            for (tile, pos, drawn) in ws.tiles_with_render_positions() {
                let rect =
                    Rectangle::new(geo.loc + pos.upscale(zoom), tile.tile_size().upscale(zoom));
                if drawn && rect.intersection(on_output).is_some() {
                    tiles.push(VisibleTile {
                        mapped: tile.window(),
                        rect,
                        origin: geo.loc + (pos + tile.buf_loc()).upscale(zoom),
                        zoom,
                    });
                }
            }
        }
    }
    tiles
}

/// The record and the IPC lists of what is on `output` at the capture scale for `max_edge`.
/// Windows are the `visible_tiles`.
pub fn describe(
    niri: &Niri,
    output: &Output,
    max_edge: u32,
) -> (ScreenRecord, Vec<ScreenWindow>, Vec<ScreenLayer>) {
    let output_scale = output.current_scale().fractional_scale();
    let output_size = niri
        .global_space
        .output_geometry(output)
        .map_or_else(Size::default, |geo| geo.size);
    let scale = capture_scale(output_scale, output_size.to_f64(), max_edge);
    let size: Size<i32, Physical> = output_size.to_f64().to_physical_precise_round(scale);
    let size = Size::from((size.w.max(1), size.h.max(1)));
    let px = |p: Point<f64, Logical>| (p.x * scale, p.y * scale);
    let px_rect = |r: Rectangle<f64, Logical>| {
        (
            r.loc.x * scale,
            r.loc.y * scale,
            r.size.w * scale,
            r.size.h * scale,
        )
    };

    let mut rects = Vec::new();
    let mut windows = Vec::new();
    for tile in visible_tiles(niri, output) {
        let mapped = tile.mapped;
        let geometry = mapped.window.geometry();
        let popup_scale = scale * tile.zoom;
        rects.push((mapped.id(), tile.rect));
        windows.push(ScreenWindow {
            id: mapped.id().get(),
            rect: px_rect(tile.rect),
            surface_origin: px(tile.origin),
            geometry_loc: (f64::from(geometry.loc.x), f64::from(geometry.loc.y)),
            window_size: (geometry.size.w, geometry.size.h),
            popups: capture_popups(mapped)
                .into_iter()
                .map(|p| CapturePopup {
                    x: p.x * popup_scale,
                    y: p.y * popup_scale,
                    width: p.width * popup_scale,
                    height: p.height * popup_scale,
                })
                .collect(),
        });
    }

    let map = layer_map_for_output(output);
    let layers = [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background]
        .into_iter()
        .flat_map(|layer| map.layers_on(layer).rev())
        .filter(|surface| niri.mapped_layer_surfaces.contains_key(*surface))
        .filter_map(|surface| {
            let geo = map.layer_geometry(surface)?.to_f64();
            Some(ScreenLayer {
                namespace: surface.namespace().to_owned(),
                pid: get_credentials_for_surface(surface.wl_surface()).map(|c| c.pid),
                rect: px_rect(geo),
                surface_origin: px(geo.loc),
            })
        })
        .collect();

    let record = ScreenRecord {
        scale,
        size,
        output_scale,
        output_size,
        overview_open: niri.layout.is_overview_open(),
        windows: rects,
        at: Instant::now(),
    };
    (record, windows, layers)
}

/// Renders the output, stores its `ScreenRecord` and returns the reply and the pixels.
fn render(
    state: &mut State,
    name: &str,
    path: String,
    max_edge: u32,
) -> anyhow::Result<(OutputCapture, Vec<u8>)> {
    // Things may have changed while settling.
    let output = output_precheck(&state.niri, Some(name))?;

    state.niri.update_render_elements(Some(&output));
    let (record, windows, layers) = describe(&state.niri, &output, max_edge);

    let niri = &state.niri;
    let pixels = state
        .backend
        .with_primary_renderer(|renderer| {
            let ctx = RenderCtx {
                renderer,
                target: RenderTarget::ScreenCapture,
                xray: None,
            };
            // No pointer: neither the owner's nor the agent's cursor is in the image.
            let elements = niri.render_to_vec(ctx, &output, false);
            render_to_vec(
                renderer,
                record.size,
                Scale::from(record.scale),
                Transform::Normal,
                Fourcc::Abgr8888,
                elements.iter().rev(),
            )
        })
        .context("no renderer")?
        .context("error rendering output")?;

    let capture = OutputCapture {
        path,
        width: record.size.w as u32,
        height: record.size.h as u32,
        scale: record.scale,
        output: output.name(),
        overview_open: record.overview_open,
        windows,
        layers,
    };
    state.niri.agent.screens.insert(output.name(), record);
    Ok((capture, pixels))
}

/// Handles `Request::AgentScreen` pointer ops, in pixels of the output's last capture: hit-tests
/// the point the way the owner's pointer is, then acts on what is there.
pub fn screen_pointer(
    state: &mut State,
    output: Option<String>,
    op: &AgentOp,
    reply: Sender<Result<ScreenHit, AgentError>>,
) {
    let result = screen_op(state, output.as_deref(), op);
    let _ = reply.send_blocking(result);
}

fn screen_op(state: &mut State, name: Option<&str>, op: &AgentOp) -> Result<ScreenHit, AgentError> {
    let niri = &mut state.niri;
    if !niri.agent.handed_over {
        return Err(AgentError::NotHandedOver);
    }
    let output = output_precheck(niri, name)?;
    let output_geo = niri
        .global_space
        .output_geometry(&output)
        .ok_or_else(|| AgentError::BadOutput(output.name()))?;
    let record = niri
        .agent
        .screens
        .get(&output.name())
        .ok_or(AgentError::NoCapture)?;
    if record.output_scale != output.current_scale().fractional_scale()
        || record.output_size != output_geo.size
        || record.overview_open != niri.layout.is_overview_open()
    {
        return Err(AgentError::StaleCapture);
    }
    let Some((x, y)) = start_point(op) else {
        return Ok(ScreenHit::Nothing);
    };

    let scale = record.scale;
    let to_output = |x: f64, y: f64| Point::<f64, Logical>::from((x / scale, y / scale));
    let local = to_output(x, y);
    // Off the image is another output or nothing; either way not what the agent looked at.
    if !Rectangle::from_size(output_geo.size.to_f64()).contains(local) {
        return Ok(ScreenHit::Nothing);
    }
    let looked = window_at(record.windows.iter().copied(), local);
    let now = visible_tiles(niri, &output)
        .into_iter()
        .map(|tile| (tile.mapped.id(), tile.rect));
    if looked != window_at(now, local) {
        return Err(AgentError::StaleCapture);
    }

    move_cursor(niri, &output, local);
    let origin = output_geo.loc.to_f64();
    let to_global = |x, y| to_output(x, y) + origin;
    let pos = to_global(x, y);
    let under = niri.contents_under(pos);

    // A button press focuses like the owner's does.
    let press = matches!(op, AgentOp::Click { .. } | AgentOp::Drag { .. });
    let hit = match (&under.surface, &under.window, &under.layer) {
        (Some((surface, surface_pos)), _, Some(layer)) => {
            let blocked_out = state
                .niri
                .mapped_layer_surfaces
                .get(layer)
                .is_some_and(|mapped| mapped.rules().block_out_from.is_some());
            if blocked_out {
                return Err(AgentError::BlockedOut);
            }
            if press {
                let layer = layer.clone();
                focus_without_warp(state, |state| {
                    state.niri.focus_layer_surface_if_on_demand(Some(layer));
                });
            }
            let target = Target {
                id: None,
                surface: layer.wl_surface().clone(),
                client: surface.client().ok_or(AgentError::WindowGone)?,
            };
            let to_local = |x, y| to_global(x, y) - *surface_pos;
            let main = target.surface.clone();
            let enter = |niri: &mut Niri| enter_keyboards_on(niri, &target.client, main);
            let (result, sent_mods) =
                deliver_with_mods(state, &target.client, surface, &to_local, op, enter);
            if result.is_ok() || sent_mods {
                finish_burst(&mut state.niri, &target);
            }
            result?;
            ScreenHit::Layer {
                namespace: layer.namespace().to_owned(),
            }
        }
        _ if under.hot_corner => ScreenHit::Nothing,
        _ if under.overview => {
            overview_op(state, &under, pos, op);
            ScreenHit::Overview
        }
        (surface, Some((window, _)), None) => {
            let id = state
                .niri
                .layout
                .windows()
                .find(|(_, mapped)| mapped.window == *window)
                .map(|(_, mapped)| mapped.id())
                .ok_or(AgentError::WindowGone)?;
            let target = check_target(&state.niri, id, OpKind::Screen)?;
            // Activating the window would close the agent's own menu before the click lands.
            let in_agent_popup = surface.as_ref().is_some_and(|(surface, _)| {
                let niri = &state.niri;
                let root = niri.root_surface.get(surface).unwrap_or(surface);
                niri.agent.is_agent_popup(id, root)
            });
            if press && !in_agent_popup {
                focus_without_warp(state, |state| {
                    state.niri.layout.activate_window(window);
                    state.niri.focus_layer_surface_if_on_demand(None);
                });
            }
            // On the border there is no surface: the press only activates.
            if let Some((surface, surface_pos)) = surface {
                let to_local = |x, y| to_global(x, y) - *surface_pos;
                deliver_window(state, &target, surface, &to_local, op)?;
            }
            ScreenHit::Window { id: id.get() }
        }
        _ => {
            if press {
                focus_without_warp(state, |state| {
                    state.click_empty_spot(pos);
                    state.niri.focus_layer_surface_if_on_demand(None);
                });
            }
            ScreenHit::Nothing
        }
    };

    // A drag stays on the surface it started on; the cursor shows where it ended.
    if let (AgentOp::Drag { x1, y1, .. }, ScreenHit::Window { .. } | ScreenHit::Layer { .. }) =
        (op, &hit)
    {
        move_cursor(&mut state.niri, &output, to_output(*x1, *y1));
    }
    Ok(hit)
}

/// Handles `Request::AgentScreen` keyboard ops: the keys go where the seat's keyboard focus is, as
/// the owner's would, but straight to the client, so they never trigger niri binds. That is a
/// window, through the hand-over guards, or a layer surface; nothing for niri's own UI.
pub fn screen_keys(state: &mut State, op: &AgentOp, reply: Sender<Result<ScreenHit, AgentError>>) {
    if !state.niri.agent.handed_over {
        let _ = reply.send_blocking(Err(AgentError::NotHandedOver));
        return;
    }
    // A focus change earlier in this loop pass hasn't reached keyboard_focus yet.
    state.update_keyboard_focus();
    let Some((target, hit)) = focus_target(&state.niri) else {
        let _ = reply.send_blocking(Ok(ScreenHit::Nothing));
        return;
    };
    let strokes = match op {
        AgentOp::Key { chord, repeat } => chord_strokes(state, chord, *repeat, true),
        AgentOp::Type { text } => text_strokes(state, text),
        // Pointer ops go to `screen_pointer`.
        _ => Ok(Vec::new()),
    };
    let op = ChunkedOp {
        check: Box::new(move |state| check_focus_target(&state.niri, &target)),
        done: Box::new(move |_| hit),
        reply,
    };
    start(state, strokes, op);
}

/// What holds the seat's keyboard focus, as a key target, and the hit to report.
fn focus_target(niri: &Niri) -> Option<(Target, ScreenHit)> {
    let (id, surface, hit) = match &niri.keyboard_focus {
        KeyboardFocus::Layout {
            surface: Some(surface),
        } => {
            let (mapped, _) = niri.layout.find_window_and_output(surface)?;
            let id = mapped.id();
            (Some(id), surface, ScreenHit::Window { id: id.get() })
        }
        KeyboardFocus::LayerShell { surface } => {
            let layer = niri
                .mapped_layer_surfaces
                .keys()
                .find(|layer| layer.wl_surface() == surface)?;
            let namespace = layer.namespace().to_owned();
            (None, surface, ScreenHit::Layer { namespace })
        }
        _ => return None,
    };
    let target = Target {
        id,
        surface: surface.clone(),
        client: surface.client()?,
    };
    Some((target, hit))
}

/// The checks a screen `key` or `type` runs before each chunk: the hand-over goes on, and the
/// window passes its guards, or the layer surface is still mapped and not blocked out.
fn check_focus_target(niri: &Niri, target: &Target) -> Result<Target, AgentError> {
    if !niri.agent.handed_over {
        return Err(AgentError::NotHandedOver);
    }
    if let Some(id) = target.id {
        return check_target(niri, id, OpKind::Screen);
    }
    let mapped = niri
        .mapped_layer_surfaces
        .iter()
        .find(|(layer, _)| *layer.wl_surface() == target.surface)
        .map(|(_, mapped)| mapped)
        .ok_or(AgentError::WindowGone)?;
    if mapped.rules().block_out_from.is_some() {
        return Err(AgentError::BlockedOut);
    }
    Ok(target.clone())
}

/// The topmost of the windows, listed topmost first, whose rect holds `at`.
fn window_at(
    windows: impl IntoIterator<Item = (MappedId, Rectangle<f64, Logical>)>,
    at: Point<f64, Logical>,
) -> Option<MappedId> {
    windows
        .into_iter()
        .find(|(_, rect)| rect.contains(at))
        .map(|(id, _)| id)
}

/// Runs `f`, then moves the keyboard focus where it says, never warping the owner's pointer.
fn focus_without_warp(state: &mut State, f: impl FnOnce(&mut State)) {
    state.niri.agent.suppress_warp = true;
    f(state);
    state.update_keyboard_focus();
    state.niri.agent.suppress_warp = false;
}

/// Does what the owner's click or wheel at `pos` does in the overview.
fn overview_op(state: &mut State, under: &PointContents, pos: Point<f64, Logical>, op: &AgentOp) {
    match op {
        AgentOp::Click {
            button: AgentButton::Left,
            ..
        } => focus_without_warp(state, |state| {
            match &under.window {
                Some((window, _)) => {
                    overview_activate(&mut state.niri.layout, window);
                    state.niri.queue_redraw_all();
                }
                None => state.click_empty_spot(pos),
            }
            state.niri.focus_layer_surface_if_on_demand(None);
        }),
        AgentOp::Scroll { dy, .. } => {
            state.overview_wheel(dy.clamp(-MAX_WHEEL_NOTCHES, MAX_WHEEL_NOTCHES));
        }
        _ => (),
    }
}

fn write_png(capture: &OutputCapture, pixels: &[u8]) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    write_png_rgba8(&mut buf, capture.width, capture.height, pixels)
        .context("error encoding PNG")?;
    std::fs::write(&capture.path, buf).context("error writing PNG")
}
