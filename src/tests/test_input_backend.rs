use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisRelativeDirection, AxisSource, ButtonState, Device,
    DeviceCapability, Event, GestureBeginEvent, GestureHoldBeginEvent, GesturePinchBeginEvent,
    GestureSwipeBeginEvent, InputBackend, InputTime, KeyState, KeyboardKeyEvent, Keycode,
    PointerAxisEvent, PointerButtonEvent, PointerMotionAbsoluteEvent, TabletToolButtonEvent,
    TabletToolCapabilities, TabletToolDescriptor, TabletToolEvent, TabletToolType, UnusedEvent,
};
use smithay::output::Output;

use crate::input::backend_ext::NiriInputDevice;

pub struct TestInputBackend;

#[derive(PartialEq, Eq, Hash)]
pub struct TestInputDevice;

pub struct TestKeyboardKeyEvent {
    pub time: InputTime,
    pub code: Keycode,
    pub state: KeyState,
    pub count: u32,
}

pub struct TestPointerButtonEvent {
    pub button: u32,
    pub state: ButtonState,
}

/// Absolute pointer motion; `x` and `y` are logical pixels in the global space.
pub struct TestPointerMotionAbsoluteEvent {
    pub x: f64,
    pub y: f64,
}

/// A vertical wheel scroll of `dy` pixels.
pub struct TestPointerAxisEvent {
    pub dy: f64,
}

/// The start of a touchpad swipe, pinch or hold.
pub struct TestGestureBeginEvent {
    pub fingers: u32,
}

/// A stylus button of a tablet tool that the seat doesn't know.
pub struct TestTabletToolButtonEvent {
    pub state: ButtonState,
}

impl InputBackend for TestInputBackend {
    type Device = TestInputDevice;

    type KeyboardKeyEvent = TestKeyboardKeyEvent;
    type PointerAxisEvent = TestPointerAxisEvent;
    type PointerButtonEvent = TestPointerButtonEvent;
    type PointerMotionEvent = UnusedEvent;
    type PointerMotionAbsoluteEvent = TestPointerMotionAbsoluteEvent;

    type GestureSwipeBeginEvent = TestGestureBeginEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = TestGestureBeginEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = TestGestureBeginEvent;
    type GestureHoldEndEvent = UnusedEvent;

    type TouchDownEvent = UnusedEvent;
    type TouchUpEvent = UnusedEvent;
    type TouchMotionEvent = UnusedEvent;
    type TouchCancelEvent = UnusedEvent;
    type TouchFrameEvent = UnusedEvent;
    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = TestTabletToolButtonEvent;

    type SwitchToggleEvent = UnusedEvent;

    type SpecialEvent = UnusedEvent;
}

impl Device for TestInputDevice {
    fn id(&self) -> String {
        String::from("test")
    }

    fn name(&self) -> String {
        String::from("test input device")
    }

    fn has_capability(&self, capability: DeviceCapability) -> bool {
        matches!(capability, DeviceCapability::Keyboard)
    }

    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }

    fn syspath(&self) -> Option<std::path::PathBuf> {
        None
    }
}

impl NiriInputDevice for TestInputDevice {
    fn output(&self, _state: &crate::niri::State) -> Option<Output> {
        None
    }
}

impl Event<TestInputBackend> for TestKeyboardKeyEvent {
    fn time(&self) -> InputTime {
        self.time
    }

    fn device(&self) -> <TestInputBackend as InputBackend>::Device {
        TestInputDevice
    }
}

impl KeyboardKeyEvent<TestInputBackend> for TestKeyboardKeyEvent {
    fn key_code(&self) -> Keycode {
        self.code
    }

    fn state(&self) -> KeyState {
        self.state
    }

    fn count(&self) -> u32 {
        self.count
    }
}

impl Event<TestInputBackend> for TestPointerButtonEvent {
    fn time(&self) -> InputTime {
        InputTime::from_micros(0)
    }

    fn device(&self) -> <TestInputBackend as InputBackend>::Device {
        TestInputDevice
    }
}

impl PointerButtonEvent<TestInputBackend> for TestPointerButtonEvent {
    fn button_code(&self) -> u32 {
        self.button
    }

    fn state(&self) -> ButtonState {
        self.state
    }
}

impl Event<TestInputBackend> for TestPointerMotionAbsoluteEvent {
    fn time(&self) -> InputTime {
        InputTime::from_micros(0)
    }

    fn device(&self) -> <TestInputBackend as InputBackend>::Device {
        TestInputDevice
    }
}

impl AbsolutePositionEvent<TestInputBackend> for TestPointerMotionAbsoluteEvent {
    fn x(&self) -> f64 {
        self.x
    }

    fn y(&self) -> f64 {
        self.y
    }

    fn x_transformed(&self, _width: i32) -> f64 {
        self.x
    }

    fn y_transformed(&self, _height: i32) -> f64 {
        self.y
    }
}

impl PointerMotionAbsoluteEvent<TestInputBackend> for TestPointerMotionAbsoluteEvent {}

impl Event<TestInputBackend> for TestPointerAxisEvent {
    fn time(&self) -> InputTime {
        InputTime::from_micros(0)
    }

    fn device(&self) -> <TestInputBackend as InputBackend>::Device {
        TestInputDevice
    }
}

impl PointerAxisEvent<TestInputBackend> for TestPointerAxisEvent {
    fn amount(&self, axis: Axis) -> Option<f64> {
        (axis == Axis::Vertical).then_some(self.dy)
    }

    fn amount_v120(&self, axis: Axis) -> Option<f64> {
        (axis == Axis::Vertical).then_some(self.dy / 15. * 120.)
    }

    fn source(&self) -> AxisSource {
        AxisSource::Wheel
    }

    fn relative_direction(&self, _axis: Axis) -> AxisRelativeDirection {
        AxisRelativeDirection::Identical
    }
}

impl Event<TestInputBackend> for TestGestureBeginEvent {
    fn time(&self) -> InputTime {
        InputTime::from_micros(0)
    }

    fn device(&self) -> <TestInputBackend as InputBackend>::Device {
        TestInputDevice
    }
}

impl GestureBeginEvent<TestInputBackend> for TestGestureBeginEvent {
    fn fingers(&self) -> u32 {
        self.fingers
    }
}

impl GestureSwipeBeginEvent<TestInputBackend> for TestGestureBeginEvent {}
impl GesturePinchBeginEvent<TestInputBackend> for TestGestureBeginEvent {}
impl GestureHoldBeginEvent<TestInputBackend> for TestGestureBeginEvent {}

/// Trait methods that return zero or `false`.
macro_rules! zeros {
    ($($name:ident: $ty:ty),* $(,)?) => {
        $(fn $name(&self) -> $ty { Default::default() })*
    };
}

impl Event<TestInputBackend> for TestTabletToolButtonEvent {
    fn time(&self) -> InputTime {
        InputTime::from_micros(0)
    }

    fn device(&self) -> <TestInputBackend as InputBackend>::Device {
        TestInputDevice
    }
}

impl AbsolutePositionEvent<TestInputBackend> for TestTabletToolButtonEvent {
    zeros!(x: f64, y: f64);

    fn x_transformed(&self, _width: i32) -> f64 {
        0.
    }

    fn y_transformed(&self, _height: i32) -> f64 {
        0.
    }
}

impl TabletToolEvent<TestInputBackend> for TestTabletToolButtonEvent {
    fn tool(&self) -> TabletToolDescriptor {
        TabletToolDescriptor {
            tool_type: TabletToolType::Pen,
            hardware_serial: 0,
            hardware_id_wacom: 0,
            capabilities: TabletToolCapabilities::empty(),
        }
    }

    zeros!(
        delta_x: f64,
        delta_y: f64,
        distance: f64,
        distance_has_changed: bool,
        pressure: f64,
        pressure_has_changed: bool,
        slider_position: f64,
        slider_has_changed: bool,
        tilt_x: f64,
        tilt_x_has_changed: bool,
        tilt_y: f64,
        tilt_y_has_changed: bool,
        rotation: f64,
        rotation_has_changed: bool,
        wheel_delta: f64,
        wheel_delta_discrete: i32,
        wheel_has_changed: bool,
    );
}

impl TabletToolButtonEvent<TestInputBackend> for TestTabletToolButtonEvent {
    fn button(&self) -> u32 {
        // BTN_STYLUS.
        0x14b
    }

    fn seat_button_count(&self) -> u32 {
        u32::from(self.state == ButtonState::Pressed)
    }

    fn button_state(&self) -> ButtonState {
        self.state
    }
}
