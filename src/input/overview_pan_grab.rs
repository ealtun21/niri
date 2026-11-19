use std::time::Duration;

use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorImageStatus, GestureHoldBeginEvent, GestureHoldEndEvent,
    GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
    GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData,
    MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
};
use smithay::input::SeatHandler;
use smithay::output::Output;
use smithay::utils::{Logical, Point};

use crate::niri::State;

/// Pans the overview camera while a pointer button is held.
pub struct OverviewPanGrab {
    start_data: PointerGrabStartData<State>,
    last_location: Point<f64, Logical>,
    output: Output,
    /// Cleared once the overview closes mid-drag. The grab still lasts until the button is
    /// released, so clients never see a release for a press they didn't get.
    panning: bool,

    // Accumulated and applied in frame().
    new_location: Point<f64, Logical>,
    event_timestamp: Option<Duration>,
    relative_delta: Option<Point<f64, Logical>>,
}

impl OverviewPanGrab {
    pub fn new(start_data: PointerGrabStartData<State>, output: Output) -> Self {
        let location = start_data.location;
        Self {
            last_location: location,
            start_data,
            output,
            panning: true,
            new_location: location,
            event_timestamp: None,
            relative_delta: None,
        }
    }

    fn on_frame(&mut self, data: &mut State) {
        if self.event_timestamp.take().is_none() || !self.panning {
            return;
        }

        // Relative motion keeps panning when the pointer is stuck at a screen edge.
        let delta = self
            .relative_delta
            .take()
            .unwrap_or(self.new_location - self.last_location);
        self.last_location = self.new_location;

        if data.niri.layout.pan_overview(&self.output, delta) {
            data.niri.queue_redraw(&self.output);
        } else {
            // The overview closed; stop panning but hold the grab until release.
            self.panning = false;
            self.on_ungrab(data);
        }
    }

    fn on_ungrab(&mut self, state: &mut State) {
        state
            .niri
            .cursor_manager
            .set_cursor_image(CursorImageStatus::default_named());
        // Show the new cursor right away; the pointer may now be on another output.
        state.niri.queue_redraw_all();
    }
}

impl PointerGrab<State> for OverviewPanGrab {
    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.motion(data, None, event);

        self.new_location = event.location;

        // Relative motion takes precedence over normal motion.
        if self.relative_delta.is_none() {
            self.event_timestamp = Some(Duration::from_micros(event.time.micros()));
        }
    }

    fn relative_motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.relative_motion(data, None, event);

        *self.relative_delta.get_or_insert_default() += event.delta;
        self.event_timestamp = Some(Duration::from_micros(event.time.micros()));
    }

    fn button(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        if handle.current_pressed().is_empty() {
            // No more buttons are pressed, release the grab.
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data);
        self.on_frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<State> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut State) {
        self.on_ungrab(data);
    }
}
