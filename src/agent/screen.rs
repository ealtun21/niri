//! The `capture-output` request.

use std::thread;
use std::time::Instant;

use anyhow::Context as _;
use async_channel::Sender;
use calloop::timer::{TimeoutAction, Timer};
use niri_ipc::{AgentError, CapturePopup, OutputCapture, ScreenLayer, ScreenWindow};
use smithay::backend::allocator::Fourcc;
use smithay::desktop::layer_map_for_output;
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::shell::wlr_layer::Layer;

use super::capture::{capture_popups, settle_time};
use super::{boost_frames, capture_scale, DEFAULT_MAX_EDGE};
use crate::niri::{Niri, State};
use crate::render_helpers::{render_to_vec, RenderCtx, RenderTarget};
use crate::utils::{get_credentials_for_surface, write_png_rgba8};
use crate::window::mapped::MappedId;

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
    for (id, _) in describe(&state.niri, &output, max_edge).0.windows {
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

/// The record and the IPC lists of what is on `output` at the capture scale for `max_edge`.
///
/// Windows are the drawn tiles that intersect the output, scaled by the overview zoom, the same
/// set as `is_on_screen`; floating ones come first, as they render above the tiled ones.
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
    let mons = niri.layout.monitors().filter(|mon| mon.output() == output);
    for mon in mons {
        let on_output = Rectangle::from_size(mon.view_size());
        let zoom = mon.overview_zoom();
        for (ws, geo) in mon.workspaces_with_render_geo() {
            for (tile, pos, drawn) in ws.tiles_with_render_positions() {
                let rect =
                    Rectangle::new(geo.loc + pos.upscale(zoom), tile.tile_size().upscale(zoom));
                if !drawn || rect.intersection(on_output).is_none() {
                    continue;
                }
                let mapped = tile.window();
                let geometry = mapped.window.geometry();
                let origin = geo.loc + (pos + tile.buf_loc()).upscale(zoom);
                let popup_scale = scale * zoom;
                rects.push((mapped.id(), rect));
                windows.push(ScreenWindow {
                    id: mapped.id().get(),
                    rect: px_rect(rect),
                    surface_origin: px(origin),
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
        }
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

fn write_png(capture: &OutputCapture, pixels: &[u8]) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    write_png_rgba8(&mut buf, capture.width, capture.height, pixels)
        .context("error encoding PNG")?;
    std::fs::write(&capture.path, buf).context("error writing PNG")
}
