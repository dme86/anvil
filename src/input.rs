//! Translation from backend input events into Wayland events and window-manager actions.
//!
//! Input has two possible destinations. Ordinary keys, buttons and scrolling are forwarded to the
//! focused client through Smithay's seat. Compositor shortcuts are intercepted first and converted
//! into small `Action` values, then executed after the keyboard callback releases its borrow.

use crate::Anvil;
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{FilterResult, ModifiersState, xkb},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::SERIAL_COUNTER,
};

#[derive(Debug)]
/// A compositor command produced while filtering one keyboard event.
///
/// Returning data from the filter instead of mutating `Anvil` inside it avoids overlapping mutable
/// borrows of the keyboard handle and global compositor state.
enum Action {
    None,
    Quit,
    Terminal,
    Focus(isize),
    SwapMaster,
    ChangeFactor(f64),
    Close,
    SelectTag(usize),
    MoveToTag(usize),
}

impl Anvil {
    /// Handles the backend-independent input event stream exposed by Smithay.
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => self.keyboard_event::<I>(event),
            InputEvent::PointerMotionAbsolute { event, .. } => {
                // Tablet/VM input arrives normalized. Transform it into logical output pixels so
                // hit testing and rendering use the same coordinate system.
                let pos = event
                    .position_transformed((self.screen_area.width, self.screen_area.height).into());
                let pointer = self.seat.get_pointer().unwrap();
                pointer.motion(
                    self,
                    self.surface_under(pos),
                    &MotionEvent {
                        location: pos,
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerMotion { event, .. } => {
                // Real libinput mice report relative deltas. Add them to the seat's current
                // location and clamp to the logical output so hit testing never escapes the KMS
                // framebuffer. Winit commonly supplies absolute events, which is why this path was
                // not needed until Anvil gained a direct backend.
                let pointer = self.seat.get_pointer().unwrap();
                let current = pointer.current_location();
                let next = (
                    (current.x + event.delta().x).clamp(0.0, self.screen_area.width as f64 - 1.0),
                    (current.y + event.delta().y).clamp(0.0, self.screen_area.height as f64 - 1.0),
                )
                    .into();
                pointer.motion(
                    self,
                    self.surface_under(next),
                    &MotionEvent {
                        location: next,
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerButton { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                let serial = SERIAL_COUNTER.next_serial();
                // Focus follows a deliberate click. Do not change it during an active client grab
                // (for example while a popup owns the pointer), because that would break protocol
                // ordering and could send the matching release to another surface.
                if event.state() == ButtonState::Pressed && !pointer.is_grabbed() {
                    if let Some((window, _)) = self
                        .space
                        .element_under(pointer.current_location())
                        .map(|(w, p)| (w.clone(), p))
                    {
                        if let Some(index) = self
                            .visible_indices_for_input()
                            .iter()
                            .position(|&i| self.windows[i].window == window)
                        {
                            self.focus_index(index);
                        }
                    } else {
                        self.seat.get_keyboard().unwrap().set_focus(
                            self,
                            Option::<WlSurface>::None,
                            serial,
                        );
                    }
                }
                // Forward the physical event even when Anvil used it to update focus. Clients need
                // the click itself for widgets and decorations.
                pointer.button(
                    self,
                    &ButtonEvent {
                        button: event.button_code(),
                        state: event.state(),
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();
                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                for axis in [Axis::Horizontal, Axis::Vertical] {
                    // Devices report either continuous pixel-like values or v120 wheel units.
                    // Normalize both while preserving v120 for clients that support precise wheels.
                    let amount = event
                        .amount(axis)
                        .unwrap_or_else(|| event.amount_v120(axis).unwrap_or(0.0) * 15.0 / 120.0);
                    if amount != 0.0 {
                        frame = frame.value(axis, amount);
                    }
                    if let Some(discrete) = event.amount_v120(axis) {
                        frame = frame.v120(axis, discrete as i32);
                    }
                    // Finger scrolling is a gesture with a defined end; sending stop lets clients
                    // finish kinetic scrolling instead of waiting indefinitely for another event.
                    if source == AxisSource::Finger && event.amount(axis) == Some(0.0) {
                        frame = frame.stop(axis);
                    }
                }
                let pointer = self.seat.get_pointer().unwrap();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            // Gestures, touch and tablet tools are intentionally future work. Ignoring them is
            // preferable to advertising incomplete behavior to clients.
            _ => {}
        }
    }

    fn keyboard_event<I: InputBackend>(&mut self, event: I::KeyboardKeyEvent) {
        let state = event.state();
        // Clone the small binding table because the closure runs while the keyboard handle also
        // borrows `self`. This is simpler and safer than introducing interior mutability.
        let keys = self.config.keys.clone();
        let action = self
            .seat
            .get_keyboard()
            .unwrap()
            .input(
                self,
                event.key_code(),
                state,
                SERIAL_COUNTER.next_serial(),
                event.time_msec(),
                |_, modifiers, handle| {
                    // Releases for non-intercepted keys must reach the client. Actions trigger once
                    // on press; executing again on release would spawn or rearrange twice.
                    if state != KeyState::Pressed {
                        return FilterResult::Forward;
                    }
                    // Compare resolved keysyms rather than hardware keycodes so bindings continue
                    // to follow the user's active XKB keyboard layout.
                    let name = xkb::keysym_get_name(handle.modified_sym());
                    match shortcut(&keys, *modifiers, &name) {
                        Action::None => FilterResult::Forward,
                        action => FilterResult::Intercept(action),
                    }
                },
            )
            .unwrap_or(Action::None);
        self.run_action(action);
    }

    fn run_action(&mut self, action: Action) {
        // All window-manager mutation happens after Smithay's keyboard filter returns. Besides
        // satisfying Rust's borrowing rules, this keeps protocol input handling and policy cleanly
        // separated.
        match action {
            Action::None => {}
            Action::Quit => self.loop_signal.stop(),
            Action::Terminal => self.spawn(&self.config.general.terminal),
            Action::Focus(delta) => self.focus_relative(delta),
            Action::SwapMaster => self.swap_master(),
            Action::ChangeFactor(delta) => {
                // Runtime adjustments are clamped to the same safe range as file validation.
                self.config.layout.master_factor =
                    (self.config.layout.master_factor + delta).clamp(0.1, 0.9);
                self.arrange();
            }
            Action::Close => self.close_focused(),
            Action::SelectTag(tag) => self.select_tag(tag),
            Action::MoveToTag(tag) => self.move_focused_to_tag(tag),
        }
    }

    fn visible_indices_for_input(&self) -> Vec<usize> {
        self.windows
            .iter()
            .enumerate()
            .filter_map(|(i, w)| (w.tags & self.selected_tags != 0).then_some(i))
            .collect()
    }
}

fn shortcut(keys: &anvil::config::Keys, modifiers: ModifiersState, name: &str) -> Action {
    // The modifier is configurable, while unknown spellings intentionally fall back to Super: the
    // compositor must always retain a usable command modifier instead of matching every key.
    let modifier = match keys.modifier.to_ascii_lowercase().as_str() {
        "alt" => modifiers.alt,
        "ctrl" | "control" => modifiers.ctrl,
        _ => modifiers.logo,
    };
    if !modifier {
        return Action::None;
    }

    // Numeric tag bindings are structural and shared by view/move actions. Shift selects the latter
    // just as it does in dwm.
    if let Some(tag) = key_tag(name) {
        return if modifiers.shift {
            Action::MoveToTag(tag)
        } else {
            Action::SelectTag(tag)
        };
    }
    // Destructive/session actions require Shift by design, making accidental close/quit less
    // likely while keeping the TOML representation compact.
    if modifiers.shift && name.eq_ignore_ascii_case(&keys.close) {
        return Action::Close;
    }
    if modifiers.shift && name.eq_ignore_ascii_case(&keys.quit) {
        return Action::Quit;
    }
    if modifiers.shift && name.eq_ignore_ascii_case(&keys.swap_master) {
        return Action::SwapMaster;
    }
    if name.eq_ignore_ascii_case(&keys.terminal) {
        Action::Terminal
    } else if name.eq_ignore_ascii_case(&keys.focus_next) {
        Action::Focus(1)
    } else if name.eq_ignore_ascii_case(&keys.focus_previous) {
        Action::Focus(-1)
    } else if name.eq_ignore_ascii_case(&keys.master_grow) {
        Action::ChangeFactor(0.05)
    } else if name.eq_ignore_ascii_case(&keys.master_shrink) {
        Action::ChangeFactor(-0.05)
    } else {
        Action::None
    }
}

fn key_tag(name: &str) -> Option<usize> {
    // User-visible tags are 1..9, but vectors and bit positions are zero-based internally.
    let value = name.parse::<usize>().ok()?;
    (1..=9).contains(&value).then_some(value - 1)
}
