//! The agent's own drawn cursor: where it was last placed, and its glide there.

use std::time::{Duration, Instant};

use smithay::output::Output;
use smithay::utils::{Logical, Point};

use super::keyboard::start_idle_timer;
use super::{is_on_screen, IDLE_RELEASE};
use crate::animation::{Animation, Curve};
use crate::niri::Niri;
use crate::window::mapped::MappedId;

/// How long the cursor takes to glide to a new point.
pub const CURSOR_GLIDE: Duration = Duration::from_millis(150);

/// Where the agent cursor is drawn, in logical coordinates local to `output`.
#[derive(Debug)]
pub struct AgentCursor {
    pub output: Output,
    pub from: Point<f64, Logical>,
    pub to: Point<f64, Logical>,
    /// The window the cursor points at, for a window op: the cursor hides once it is off screen.
    /// `None` for a screen op.
    pub window: Option<MappedId>,
    /// Eased 0 to 1 from `from` to `to`.
    pub glide: Animation,
    /// When the agent last placed the cursor.
    pub last_op: Instant,
}

impl AgentCursor {
    /// Rests the cursor on its target once the glide is over.
    pub fn settle(&mut self) {
        if self.glide.is_done() {
            self.from = self.to;
        }
    }
}

/// Glides the cursor to `to` on `output`, or shows it there if it was hidden or elsewhere. `window`
/// is the window a window op points at.
pub fn move_cursor(
    niri: &mut Niri,
    output: &Output,
    to: Point<f64, Logical>,
    window: Option<MappedId>,
) {
    let from = cursor_location(niri, output).unwrap_or(to);
    let old = niri.agent.cursor.take();
    if let Some(old) = old.filter(|old| old.output != *output) {
        redraw(niri, &old.output);
    }
    niri.agent.cursor = Some(AgentCursor {
        output: output.clone(),
        from,
        to,
        window,
        glide: Animation::ease(
            niri.clock.clone(),
            0.,
            1.,
            0.,
            CURSOR_GLIDE.as_millis() as u64,
            Curve::EaseOutCubic,
        ),
        last_op: Instant::now(),
    });
    niri.queue_redraw(output);
    start_idle_timer(niri);
}

fn redraw(niri: &mut Niri, output: &Output) {
    // The output may be gone by now.
    if niri.output_state.contains_key(output) {
        niri.queue_redraw(output);
    }
}

/// Hides the cursor and redraws the output it was on.
pub fn hide_cursor(niri: &mut Niri) {
    if let Some(cursor) = niri.agent.cursor.take() {
        redraw(niri, &cursor.output);
    }
}

/// Hides the cursor once the agent has been idle for `IDLE_RELEASE`.
pub fn hide_if_idle(niri: &mut Niri) {
    if niri
        .agent
        .cursor
        .as_ref()
        .is_some_and(|c| c.last_op.elapsed() > IDLE_RELEASE)
    {
        hide_cursor(niri);
    }
}

/// Where to draw the agent cursor on `output`, if it shows there.
pub fn cursor_location(niri: &Niri, output: &Output) -> Option<Point<f64, Logical>> {
    let cursor = niri.agent.cursor.as_ref()?;
    if niri.agent.paused
        || cursor.output != *output
        || cursor.last_op.elapsed() > IDLE_RELEASE
        // Windows are scaled in the overview, so the point would be wrong.
        || (!niri.agent.handed_over && niri.layout.is_overview_open())
        || cursor.window.is_some_and(|id| !is_on_screen(niri, id))
    {
        return None;
    }
    Some(cursor.from + (cursor.to - cursor.from).upscale(cursor.glide.value()))
}

/// Puts the cursor at `local`, a surface-local point of window `id`, or hides it when the window
/// isn't on screen.
pub fn place_on_window(niri: &mut Niri, id: MappedId, local: Point<f64, Logical>) {
    match window_point(niri, id, local) {
        Some((output, at)) => move_cursor(niri, &output, at, Some(id)),
        None => hide_cursor(niri),
    }
}

/// The output and output-local point where surface-local `local` of window `id` is on screen.
fn window_point(
    niri: &Niri,
    id: MappedId,
    local: Point<f64, Logical>,
) -> Option<(Output, Point<f64, Logical>)> {
    if !is_on_screen(niri, id) {
        return None;
    }
    niri.layout.monitors().find_map(|mon| {
        let zoom = mon.overview_zoom();
        mon.workspaces_with_render_geo().find_map(|(ws, geo)| {
            ws.tiles_with_render_positions()
                .find(|(tile, ..)| tile.window().id() == id)
                .map(|(tile, pos, _)| {
                    let at = geo.loc + (pos + tile.buf_loc() + local).upscale(zoom);
                    (mon.output().clone(), at)
                })
        })
    })
}
