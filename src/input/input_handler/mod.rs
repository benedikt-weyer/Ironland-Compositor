use std::{convert::TryInto, process::Command, sync::atomic::Ordering};

use crate::config::{Gesture, SwipeDirection};
use crate::{
    AnvilState, focus::PointerFocusTarget, shell::FullscreenSurface,
    shortcuts::ShortcutsHandler,
};

#[cfg(feature = "udev")]
use crate::udev::UdevData;
#[cfg(feature = "udev")]
use smithay::{backend::renderer::DebugFlags, input::tablet};

use smithay::{
    backend::input::{
        self, Axis, AxisSource, Device, DeviceCapability, Event, InputBackend, InputEvent,
        InputTime, KeyState, KeyboardKeyEvent, MouseButton, PointerAxisEvent, PointerButtonEvent,
        TouchEvent,
    },
    desktop::{WindowSurfaceType, layer_map_for_output},
    input::{
        keyboard::{FilterResult, Keysym, ModifiersState, keysyms as xkb, xkb as xkb_utf8},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
        tablet::{TabletDescriptor, TabletSeatTrait},
        touch::{DownEvent, UpEvent},
    },
    output::Scale,
    reexports::{
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1,
        wayland_server::protocol::wl_pointer,
    },
    utils::{Logical, Point, SERIAL_COUNTER as SCOUNTER, Serial, Transform},
    wayland::{
        input_method::InputMethodSeat,
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat,
        shell::wlr_layer::{KeyboardInteractivity, Layer as WlrLayer},
    },
};
// Already brought in by the `udev`-gated `smithay::wayland::seat::WaylandFocus`
// import below when that feature is on.
#[cfg(not(feature = "udev"))]
use smithay::wayland::seat::WaylandFocus;

use smithay::backend::input::AbsolutePositionEvent;

#[cfg(any(feature = "winit", feature = "x11"))]
use smithay::output::Output;
use tracing::{debug, error, info};

use crate::state::Backend;
#[cfg(feature = "udev")]
use smithay::{
    backend::{
        input::{
            GestureBeginEvent, GestureEndEvent, GesturePinchUpdateEvent as _,
            GestureSwipeUpdateEvent as _, PointerMotionEvent, ProximityState,
            TabletToolButtonEvent, TabletToolEvent, TabletToolProximityEvent, TabletToolTipEvent,
            TabletToolTipState,
        },
        session::Session,
    },
    input::pointer::{
        GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
        GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
        GestureSwipeUpdateEvent, RelativeMotionEvent,
    },
    reexports::wayland_server::DisplayHandle,
    wayland::{
        pointer_constraints::{PointerConstraint, with_pointer_constraint},
        seat::WaylandFocus,
    },
};

mod actions;
mod gestures;
mod keyboard;
mod pointer;
#[cfg(feature = "udev")]
mod tablet_tool;
mod touch;
#[cfg(any(feature = "winit", feature = "x11"))]
mod windowed;

pub(crate) use actions::*;
pub(crate) use gestures::{GestureTracker, compile_gesture_bindings};

impl<BackendData: Backend> AnvilState<BackendData> {
    fn on_device_added<B: InputBackend>(&mut self, device: B::Device) {
        let dh = &self.display_handle;
        if device.has_capability(DeviceCapability::TabletTool) {
            self.seat
                .tablet_seat()
                .add_wp_tablet(dh, &TabletDescriptor::from(&device));
        }
        if device.has_capability(DeviceCapability::Touch) && self.seat.get_touch().is_none() {
            self.seat.add_touch();
        }
    }

    /// Locks the screen when the laptop lid closes, if `lock.lock_on_lid_close`.
    fn on_switch_toggle<B: InputBackend>(&mut self, evt: B::SwitchToggleEvent) {
        use smithay::backend::input::{Switch, SwitchState, SwitchToggleEvent};
        if evt.switch() == Some(Switch::Lid)
            && evt.state() == SwitchState::On
            && self.config.lock.lock_on_lid_close
        {
            self.lock_screen();
        }
    }

    fn on_device_removed<B: InputBackend>(&mut self, device: B::Device) {
        if device.has_capability(DeviceCapability::TabletTool) {
            let tablet_seat = self.seat.tablet_seat();

            tablet_seat.remove_tablet(&TabletDescriptor::from(&device));

            // If there are no tablets in seat we can remove all tools
            if tablet_seat.count_tablets() == 0 {
                tablet_seat.clear_tools();
            }
        }
    }
}

#[cfg(feature = "udev")]
impl AnvilState<UdevData> {
    pub fn process_input_event<B: InputBackend>(
        &mut self,
        dh: &DisplayHandle,
        event: InputEvent<B>,
    ) {
        // While the screen is locked only the keyboard (password entry) and
        // pointer movement (so the cursor still follows the mouse) get through;
        // clicks, scrolling, touch, tablets and gestures are dropped.
        if self.lock.is_locked()
            && !matches!(
                event,
                InputEvent::Keyboard { .. }
                    | InputEvent::PointerMotion { .. }
                    | InputEvent::PointerMotionAbsolute { .. }
                    | InputEvent::DeviceAdded { .. }
                    | InputEvent::DeviceRemoved { .. }
            )
        {
            return;
        }

        // The workspace overview only takes the keyboard and the mouse.
        if self.overview.is_visible()
            && !matches!(
                event,
                InputEvent::Keyboard { .. }
                    | InputEvent::PointerMotion { .. }
                    | InputEvent::PointerMotionAbsolute { .. }
                    | InputEvent::PointerButton { .. }
                    | InputEvent::DeviceAdded { .. }
                    | InputEvent::DeviceRemoved { .. }
            )
        {
            return;
        }

        match event {
            InputEvent::Keyboard { event, .. } => match self.keyboard_key_to_action::<B>(event) {
                #[cfg(feature = "udev")]
                KeyAction::VtSwitch(vt) => {
                    info!(to = vt, "Trying to switch vt");
                    if let Err(err) = self.backend_data.session.change_vt(vt) {
                        error!(vt, "Error switching vt: {}", err);
                    }
                }
                KeyAction::Screen(num) => {
                    let geometry = self
                        .space
                        .outputs()
                        .nth(num)
                        .map(|o| self.space.output_geometry(o).unwrap());

                    if let Some(geometry) = geometry {
                        let x = geometry.loc.x as f64 + geometry.size.w as f64 / 2.0;
                        let y = geometry.size.h as f64 / 2.0;
                        let location = (x, y).into();
                        let pointer = self.pointer.clone();
                        let under = self.surface_under(location);
                        pointer.motion(
                            self,
                            under,
                            &MotionEvent {
                                location,
                                serial: SCOUNTER.next_serial(),
                                time: InputTime::now(),
                            },
                        );
                        pointer.frame(self);
                    }
                }
                KeyAction::ScaleUp => {
                    let pos = self.pointer.current_location().to_i32_round();
                    let output = self
                        .space
                        .outputs()
                        .find(|o| self.space.output_geometry(o).unwrap().contains(pos))
                        .cloned();

                    if let Some(output) = output {
                        let (output_location, scale) = (
                            self.space.output_geometry(&output).unwrap().loc,
                            output.current_scale().fractional_scale(),
                        );
                        let new_scale = scale + 0.25;
                        output.change_current_state(
                            None,
                            None,
                            Some(Scale::Fractional(new_scale)),
                            None,
                        );

                        let rescale = scale / new_scale;
                        let output_location = output_location.to_f64();
                        let mut pointer_output_location =
                            self.pointer.current_location() - output_location;
                        pointer_output_location.x *= rescale;
                        pointer_output_location.y *= rescale;
                        let pointer_location = output_location + pointer_output_location;

                        crate::shell::fixup_positions(&mut self.space, pointer_location);
                        crate::shell::tiling::retile_all_outputs(self);
                        let pointer = self.pointer.clone();
                        let under = self.surface_under(pointer_location);
                        pointer.motion(
                            self,
                            under,
                            &MotionEvent {
                                location: pointer_location,
                                serial: SCOUNTER.next_serial(),
                                time: InputTime::now(),
                            },
                        );
                        pointer.frame(self);
                        self.backend_data.reset_buffers(&output);
                    }
                }
                KeyAction::ScaleDown => {
                    let pos = self.pointer.current_location().to_i32_round();
                    let output = self
                        .space
                        .outputs()
                        .find(|o| self.space.output_geometry(o).unwrap().contains(pos))
                        .cloned();

                    if let Some(output) = output {
                        let (output_location, scale) = (
                            self.space.output_geometry(&output).unwrap().loc,
                            output.current_scale().fractional_scale(),
                        );
                        let new_scale = f64::max(1.0, scale - 0.25);
                        output.change_current_state(
                            None,
                            None,
                            Some(Scale::Fractional(new_scale)),
                            None,
                        );

                        let rescale = scale / new_scale;
                        let output_location = output_location.to_f64();
                        let mut pointer_output_location =
                            self.pointer.current_location() - output_location;
                        pointer_output_location.x *= rescale;
                        pointer_output_location.y *= rescale;
                        let pointer_location = output_location + pointer_output_location;

                        crate::shell::fixup_positions(&mut self.space, pointer_location);
                        crate::shell::tiling::retile_all_outputs(self);
                        let pointer = self.pointer.clone();
                        let under = self.surface_under(pointer_location);
                        pointer.motion(
                            self,
                            under,
                            &MotionEvent {
                                location: pointer_location,
                                serial: SCOUNTER.next_serial(),
                                time: InputTime::now(),
                            },
                        );
                        pointer.frame(self);
                        self.backend_data.reset_buffers(&output);
                    }
                }
                KeyAction::RotateOutput => {
                    let pos = self.pointer.current_location().to_i32_round();
                    let output = self
                        .space
                        .outputs()
                        .find(|o| self.space.output_geometry(o).unwrap().contains(pos))
                        .cloned();

                    if let Some(output) = output {
                        let current_transform = output.current_transform();
                        let new_transform = match current_transform {
                            Transform::Normal => Transform::_90,
                            Transform::_90 => Transform::_180,
                            Transform::_180 => Transform::_270,
                            Transform::_270 => Transform::Flipped,
                            Transform::Flipped => Transform::Flipped90,
                            Transform::Flipped90 => Transform::Flipped180,
                            Transform::Flipped180 => Transform::Flipped270,
                            Transform::Flipped270 => Transform::Normal,
                        };
                        output.change_current_state(None, Some(new_transform), None, None);
                        crate::shell::fixup_positions(
                            &mut self.space,
                            self.pointer.current_location(),
                        );
                        crate::shell::tiling::retile_all_outputs(self);
                        self.backend_data.reset_buffers(&output);
                    }
                }
                KeyAction::ToggleTint => {
                    let mut debug_flags = self.backend_data.debug_flags();
                    debug_flags.toggle(DebugFlags::TINT);
                    self.backend_data.set_debug_flags(debug_flags);
                }

                action => match action {
                    KeyAction::None
                    | KeyAction::Quit
                    | KeyAction::Run(_)
                    | KeyAction::TogglePreview
                    | KeyAction::ToggleDecorations
                    | KeyAction::ToggleLauncher
                    | KeyAction::LockScreen
                    | KeyAction::ToggleOverview
                    | KeyAction::ToggleVmOverview
                    | KeyAction::VmOverviewMove(_)
                    | KeyAction::VmOverviewToggleInternet
                    | KeyAction::LauncherType(_)
                    | KeyAction::LauncherBackspace
                    | KeyAction::LauncherUp
                    | KeyAction::LauncherDown
                    | KeyAction::LauncherActivate
                    | KeyAction::LauncherTabLeft
                    | KeyAction::LauncherTabRight
                    | KeyAction::LauncherClose
                    | KeyAction::ToggleFloating
                    | KeyAction::KillWindow
                    | KeyAction::FocusDirection(_)
                    | KeyAction::SwapDirection(_)
                    | KeyAction::ResizeTiled(_)
                    | KeyAction::SwitchWorkspace(_)
                    | KeyAction::MoveWindowWorkspace(_, _)
                    | KeyAction::MoveWindowMonitor(_)
                    | KeyAction::Shortcut(_)
                    | KeyAction::ShortcutReleased(_)
                    | KeyAction::ShortcutTap(_) => self.process_common_key_action(action),

                    _ => unreachable!(),
                },
            },
            InputEvent::PointerMotion { event, .. } => self.on_pointer_move::<B>(dh, event),
            InputEvent::PointerMotionAbsolute { event, .. } => {
                self.on_pointer_move_absolute::<B>(dh, event)
            }
            InputEvent::PointerButton { event, .. } => self.on_pointer_button::<B>(event),
            InputEvent::PointerAxis { event, .. } => self.on_pointer_axis::<B>(event),
            InputEvent::TabletToolAxis { event, .. } => self.on_tablet_tool_axis::<B>(event),
            InputEvent::TabletToolProximity { event, .. } => {
                self.on_tablet_tool_proximity::<B>(dh, event)
            }
            InputEvent::TabletToolTip { event, .. } => self.on_tablet_tool_tip::<B>(event),
            InputEvent::TabletToolButton { event, .. } => self.on_tablet_button::<B>(event),
            InputEvent::GestureSwipeBegin { event, .. } => self.on_gesture_swipe_begin::<B>(event),
            InputEvent::GestureSwipeUpdate { event, .. } => {
                self.on_gesture_swipe_update::<B>(event)
            }
            InputEvent::GestureSwipeEnd { event, .. } => self.on_gesture_swipe_end::<B>(event),
            InputEvent::GesturePinchBegin { event, .. } => self.on_gesture_pinch_begin::<B>(event),
            InputEvent::GesturePinchUpdate { event, .. } => {
                self.on_gesture_pinch_update::<B>(event)
            }
            InputEvent::GesturePinchEnd { event, .. } => self.on_gesture_pinch_end::<B>(event),
            InputEvent::GestureHoldBegin { event, .. } => self.on_gesture_hold_begin::<B>(event),
            InputEvent::GestureHoldEnd { event, .. } => self.on_gesture_hold_end::<B>(event),

            InputEvent::TouchDown { event } => self.on_touch_down::<B>(event),
            InputEvent::TouchUp { event } => self.on_touch_up::<B>(event),
            InputEvent::TouchMotion { event } => self.on_touch_motion::<B>(event),
            InputEvent::TouchFrame { event } => self.on_touch_frame::<B>(event),
            InputEvent::TouchCancel { event } => self.on_touch_cancel::<B>(event),

            InputEvent::SwitchToggle { event } => self.on_switch_toggle::<B>(event),

            InputEvent::DeviceAdded { device } => self.on_device_added::<B>(device),
            InputEvent::DeviceRemoved { device } => self.on_device_removed::<B>(device),
            _ => {
                // other events are not handled in anvil (yet)
            }
        }
    }
}
