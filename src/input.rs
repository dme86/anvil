//! Translation from backend input events into Wayland events and window-manager actions.
//!
//! Input has two possible destinations. Ordinary keys, buttons and scrolling are forwarded to the
//! focused client through Smithay's seat. Compositor shortcuts are intercepted first and converted
//! into small `Action` values, then executed after the keyboard callback releases its borrow.

#[cfg(feature = "bar")]
use crate::bar::BarHit;
use crate::{Anvil, state::PointerOperationKind};
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{FilterResult, ModifiersState, xkb},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{SERIAL_COUNTER, Serial},
    wayland::{
        compositor::RegionAttributes,
        pointer_constraints::{PointerConstraint, with_pointer_constraint},
    },
};

/// Determines whether confinement permits a proposed surface-local position.
///
/// A client region replaces the full surface as the legal area. Keeping this calculation separate
/// makes the policy testable without constructing a Wayland client and pointer resource.
fn confinement_accepts(
    region: Option<&RegionAttributes>,
    local: smithay::utils::Point<i32, smithay::utils::Logical>,
    surface_contains: bool,
) -> bool {
    region.map_or(surface_contains, |region| region.contains(local))
}

#[derive(Debug)]
/// A compositor command produced while filtering one keyboard event.
///
/// Returning data from the filter instead of mutating `Anvil` inside it avoids overlapping mutable
/// borrows of the keyboard handle and global compositor state.
enum Action {
    None,
    Quit,
    Terminal,
    RunCommand(String),
    Focus(isize),
    FocusOutput(isize),
    MoveToOutput(isize),
    CycleLayoutMode,
    SwapMaster,
    ChangeFactor(f64),
    Close,
    SelectTag(usize),
    MoveToTag(usize),
    #[cfg(feature = "launcher")]
    OpenLauncher,
    #[cfg(feature = "launcher")]
    LauncherClose,
    #[cfg(feature = "launcher")]
    LauncherInsert(char),
    #[cfg(feature = "launcher")]
    LauncherBackspace,
    #[cfg(feature = "launcher")]
    LauncherSelect(isize),
    #[cfg(feature = "launcher")]
    LauncherAccept,
}

impl Anvil {
    /// Returns the focused surface together with its global surface-tree origin.
    ///
    /// Smithay's relative-pointer API expects the same origin that ordinary motion uses. Deriving
    /// it from the current hit test also handles popup and subsurface focus without inventing a
    /// second geometry model.
    pub(crate) fn pointer_focus_with_origin(
        &self,
        surface: &WlSurface,
    ) -> Option<(
        WlSurface,
        smithay::utils::Point<f64, smithay::utils::Logical>,
    )> {
        self.surface_under(self.seat.get_pointer()?.current_location())
            .filter(|(focused, _)| focused == surface)
    }

    /// Activates a constraint after pointer motion has established focus on its surface.
    pub(crate) fn activate_focused_pointer_constraint(&mut self) {
        let pointer = self.seat.get_pointer().unwrap();
        if let Some(surface) = pointer.current_focus() {
            with_pointer_constraint(&surface, &pointer, |constraint| {
                if let Some(constraint) = constraint.filter(|constraint| !constraint.is_active()) {
                    constraint.activate();
                }
            });
        }
    }

    /// Applies the active focused constraint to a proposed global pointer location.
    ///
    /// A lock preserves the old location. Confinement accepts movement only when the target lies
    /// on the constrained surface and, when supplied, inside the client's surface-local region.
    /// Rejecting a jump rather than projecting it avoids accidentally crossing holes in a region.
    fn constrain_pointer_motion(
        &self,
        current: smithay::utils::Point<f64, smithay::utils::Logical>,
        proposed: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> smithay::utils::Point<f64, smithay::utils::Logical> {
        let pointer = self.seat.get_pointer().unwrap();
        let Some(surface) = pointer.current_focus() else {
            return proposed;
        };
        let Some((_, origin)) = self.pointer_focus_with_origin(&surface) else {
            return proposed;
        };

        with_pointer_constraint(&surface, &pointer, |constraint| match constraint {
            Some(constraint) if constraint.is_active() => match &*constraint {
                PointerConstraint::Locked(_) => current,
                PointerConstraint::Confined(confined) => {
                    let surface_contains = self
                        .surface_under(proposed)
                        .is_some_and(|(candidate, _)| candidate == surface);
                    let inside = confinement_accepts(
                        confined.region(),
                        (proposed - origin).to_i32_round(),
                        surface_contains,
                    );
                    if inside { proposed } else { current }
                }
            },
            _ => proposed,
        })
    }

    /// Handles the backend-independent input event stream exposed by Smithay.
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => self.keyboard_event::<I>(event),
            InputEvent::PointerMotionAbsolute { event, .. } => {
                self.notify_idle_activity();
                // The compositor draws its own DRM cursor, so motion itself damages the frame even
                // when no client surface commits. Keyboard events are intentionally different:
                // their concrete WM action or resulting client commit requests the repaint.
                self.request_repaint();
                // Tablet/VM input arrives normalized. Transform it into logical output pixels so
                // hit testing and rendering use the same coordinate system.
                let bounds = self.desktop_bounds();
                let proposed = event.position_transformed((bounds.width, bounds.height).into());
                let current = self.seat.get_pointer().unwrap().current_location();
                let pos = self.constrain_pointer_motion(current, proposed);
                self.focus_output_at(pos);
                self.update_pointer_operation(pos);
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
                self.activate_focused_pointer_constraint();
                pointer.frame(self);
            }
            InputEvent::PointerMotion { event, .. } => {
                self.notify_idle_activity();
                self.request_repaint();
                // Real libinput mice report relative deltas. Add them to the seat's current
                // location and clamp to the logical output so hit testing never escapes the KMS
                // framebuffer. Winit commonly supplies absolute events, which is why this path was
                // not needed until Anvil gained a direct backend.
                let pointer = self.seat.get_pointer().unwrap();
                let current = pointer.current_location();
                let bounds = self.desktop_bounds();
                let proposed = (
                    (current.x + event.delta().x).clamp(0.0, bounds.width as f64 - 1.0),
                    (current.y + event.delta().y).clamp(0.0, bounds.height as f64 - 1.0),
                )
                    .into();
                // Relative motion is independent of the visible cursor position. A locked client
                // still receives every raw delta, while the compositor deliberately leaves the
                // absolute location (and therefore focus) unchanged.
                let focus = pointer
                    .current_focus()
                    .and_then(|surface| self.pointer_focus_with_origin(&surface));
                pointer.relative_motion(
                    self,
                    focus,
                    &RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time_msec() as u64 * 1_000,
                    },
                );
                let next = self.constrain_pointer_motion(current, proposed);
                self.focus_output_at(next);
                self.update_pointer_operation(next);
                pointer.motion(
                    self,
                    self.surface_under(next),
                    &MotionEvent {
                        location: next,
                        serial: SERIAL_COUNTER.next_serial(),
                        time: event.time_msec(),
                    },
                );
                self.activate_focused_pointer_constraint();
                pointer.frame(self);
            }
            InputEvent::PointerButton { event, .. } => {
                self.notify_idle_activity();
                self.request_repaint();
                let pointer = self.seat.get_pointer().unwrap();
                let serial = SERIAL_COUNTER.next_serial();
                if event.state() == ButtonState::Pressed {
                    self.remember_user_input(serial);
                }
                let button = event.button_code();
                if self.session_locked() {
                    // Lock clients exclusively own pointer input. Bypass bars, window focus and
                    // compositor move/resize gestures even when the configured modifier is held.
                    if event.state() == ButtonState::Pressed {
                        let location = pointer.current_location();
                        if let Some((surface, _)) = self.surface_under(location) {
                            self.seat.get_keyboard().unwrap().set_focus(
                                self,
                                Some(surface),
                                serial,
                            );
                        }
                    }
                    pointer.button(
                        self,
                        &ButtonEvent {
                            button,
                            state: event.state(),
                            serial,
                            time: event.time_msec(),
                        },
                    );
                    pointer.frame(self);
                    return;
                }
                let mut compositor_consumed =
                    event.state() == ButtonState::Released && self.finish_pointer_operation(button);
                // Focus follows a deliberate click. Do not change it during an active client grab
                // (for example while a popup owns the pointer), because that would break protocol
                // ordering and could send the matching release to another surface.
                if event.state() == ButtonState::Pressed && !pointer.is_grabbed() {
                    let location = pointer.current_location();
                    self.focus_output_at(location);
                    let modifiers = self.seat.get_keyboard().unwrap().modifier_state();
                    if configured_modifier_active(&self.config.keys, modifiers) {
                        let operation = match button {
                            0x110 => Some(PointerOperationKind::Move),
                            0x111 if modifiers.shift => {
                                Some(PointerOperationKind::ResizeHorizontal)
                            }
                            0x111 => Some(PointerOperationKind::ResizeBoth),
                            0x112 => Some(PointerOperationKind::ResizeVertical),
                            _ => None,
                        };
                        if let Some(operation) = operation {
                            compositor_consumed =
                                self.begin_pointer_operation(location, button, operation);
                        }
                    }
                    #[cfg(feature = "bar")]
                    let bar_output = self
                        .outputs
                        .iter()
                        .find(|output| {
                            let area = output.screen_area;
                            location.x >= f64::from(area.x)
                                && location.x < f64::from(area.x + area.width)
                                && location.y >= f64::from(area.y)
                                && location.y < f64::from(area.y + self.config.bar.height)
                        })
                        .cloned();
                    #[cfg(feature = "bar")]
                    let bar_consumed = match bar_output.as_ref() {
                        Some(output) if !compositor_consumed => {
                            // Only the conventional left button activates bar controls. Other buttons
                            // are deliberately consumed over the compositor-owned strip so they do not
                            // clear keyboard focus or leak to a previously focused client.
                            if event.button_code() == 0x110 {
                                let config = self.config.bar.clone();
                                let snapshot = self.bar_snapshot(&output.name);
                                match self.bar.hit_test(
                                    output.screen_area.width,
                                    &config,
                                    &snapshot,
                                    location.x.floor() as i32 - output.screen_area.x,
                                    location.y.floor() as i32 - output.screen_area.y,
                                ) {
                                    Some(BarHit::Tag(tag)) => self.select_tag(tag),
                                    Some(BarHit::LayoutMode) => self.cycle_layout_mode(),
                                    Some(BarHit::Window(index)) => self.focus_index(index),
                                    None => {}
                                }
                            }
                            true
                        }
                        _ => false,
                    };
                    #[cfg(not(feature = "bar"))]
                    let bar_consumed = false;
                    compositor_consumed |= bar_consumed;

                    #[cfg(feature = "layer-shell")]
                    let layer_clicked =
                        !compositor_consumed && self.focus_layer_at(location, serial);
                    #[cfg(not(feature = "layer-shell"))]
                    let layer_clicked = false;
                    if !compositor_consumed && !layer_clicked {
                        if let Some((window, _)) = self
                            .space
                            .element_under(location)
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
                }
                // A compositor gesture owns its press/release pair. Ordinary focus clicks are
                // still forwarded because clients need the click itself for widgets and content.
                if !compositor_consumed {
                    pointer.button(
                        self,
                        &ButtonEvent {
                            button,
                            state: event.state(),
                            serial,
                            time: event.time_msec(),
                        },
                    );
                }
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event, .. } => {
                self.notify_idle_activity();
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
        self.notify_idle_activity();
        let state = event.state();
        let serial = SERIAL_COUNTER.next_serial();
        if state == KeyState::Pressed {
            self.remember_user_input(serial);
        }
        // Clone the small binding table because the closure runs while the keyboard handle also
        // borrows `self`. This is simpler and safer than introducing interior mutability.
        let keys = self.config.keys.clone();
        let media = self.config.media.clone();
        let tag_count = self.config.general.tags;
        let session_locked = self.session_locked();
        #[cfg(feature = "launcher")]
        let launcher_active = self.launcher.active();
        let action = self
            .seat
            .get_keyboard()
            .unwrap()
            .input(
                self,
                event.key_code(),
                state,
                serial,
                event.time_msec(),
                |_, modifiers, handle| {
                    if session_locked {
                        return FilterResult::Forward;
                    }
                    // Releases for non-intercepted keys must reach the client. Actions trigger once
                    // on press; executing again on release would spawn or rearrange twice.
                    if state != KeyState::Pressed {
                        #[cfg(feature = "launcher")]
                        if launcher_active {
                            return FilterResult::Intercept(Action::None);
                        }
                        return FilterResult::Forward;
                    }
                    // Compare resolved keysyms rather than hardware keycodes so bindings continue
                    // to follow the user's active XKB keyboard layout.
                    let name = xkb::keysym_get_name(handle.modified_sym());
                    #[cfg(feature = "launcher")]
                    if launcher_active {
                        return FilterResult::Intercept(launcher_action(
                            *modifiers,
                            &name,
                            handle.modified_sym().key_char(),
                        ));
                    }
                    let mut action = media_command(&media, *modifiers, &name).map_or_else(
                        || shortcut(&keys, tag_count, *modifiers, &name),
                        Action::RunCommand,
                    );
                    if matches!(action, Action::None) {
                        // Shift changes digits/punctuation (2 -> @, comma -> less), but it
                        // also selects move-to-tag/output actions. Fall back to the current
                        // layout's base symbol while keeping explicitly configured shifted
                        // symbols and launcher text input intact.
                        for symbol in handle.raw_syms() {
                            action = shortcut(
                                &keys,
                                tag_count,
                                *modifiers,
                                &xkb::keysym_get_name(symbol),
                            );
                            if !matches!(action, Action::None) {
                                break;
                            }
                        }
                    }
                    match action {
                        Action::None => FilterResult::Forward,
                        action => FilterResult::Intercept(action),
                    }
                },
            )
            .unwrap_or(Action::None);
        self.run_action(action);
    }

    /// Records only discrete physical input, never synthetic focus changes or pointer motion.
    /// Clients receive this serial with the corresponding event and can later prove that a launch
    /// or activation request originated from a real user action.
    fn remember_user_input(&mut self, serial: Serial) {
        self.last_user_input = Some((serial, std::time::Instant::now()));
    }

    fn run_action(&mut self, action: Action) {
        // All window-manager mutation happens after Smithay's keyboard filter returns. Besides
        // satisfying Rust's borrowing rules, this keeps protocol input handling and policy cleanly
        // separated.
        match action {
            Action::None => {}
            Action::Quit => self.loop_signal.stop(),
            Action::Terminal => {
                let command = self.config.general.terminal.clone();
                self.spawn(&command);
            }
            Action::RunCommand(command) => self.spawn(&command),
            Action::Focus(delta) => self.focus_relative(delta),
            Action::FocusOutput(delta) => self.focus_output_relative(delta),
            Action::MoveToOutput(delta) => self.move_focused_to_output(delta),
            Action::CycleLayoutMode => self.cycle_layout_mode(),
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
            #[cfg(feature = "launcher")]
            Action::OpenLauncher => self.open_launcher(),
            #[cfg(feature = "launcher")]
            Action::LauncherClose => self.close_launcher(),
            #[cfg(feature = "launcher")]
            Action::LauncherInsert(character) => self.launcher_insert(character),
            #[cfg(feature = "launcher")]
            Action::LauncherBackspace => self.launcher_backspace(),
            #[cfg(feature = "launcher")]
            Action::LauncherSelect(delta) => self.launcher_select(delta),
            #[cfg(feature = "launcher")]
            Action::LauncherAccept => self.launcher_accept(),
        }
    }

    fn visible_indices_for_input(&self) -> Vec<usize> {
        self.visible_indices()
    }
}

/// Recognize XF86 media keys independently of the regular Super shortcuts.
/// Commands are intentionally opt-in: an empty entry forwards the event.
fn media_command(
    media: &anvil::config::Media,
    modifiers: ModifiersState,
    name: &str,
) -> Option<String> {
    if modifiers.ctrl || modifiers.alt || modifiers.logo || modifiers.shift {
        return None;
    }
    let command = match name {
        "XF86AudioRaiseVolume" => &media.volume_up,
        "XF86AudioLowerVolume" => &media.volume_down,
        "XF86AudioMute" => &media.volume_mute,
        "XF86MonBrightnessUp" => &media.brightness_up,
        "XF86MonBrightnessDown" => &media.brightness_down,
        _ => return None,
    };
    (!command.trim().is_empty()).then(|| command.to_owned())
}

fn shortcut(
    keys: &anvil::config::Keys,
    tag_count: usize,
    modifiers: ModifiersState,
    name: &str,
) -> Action {
    // The modifier is configurable, while unknown spellings intentionally fall back to Super: the
    // compositor must always retain a usable command modifier instead of matching every key.
    if !configured_modifier_active(keys, modifiers) {
        return Action::None;
    }

    // Numeric tag bindings are structural and shared by view/move actions. Shift selects the latter
    // just as it does in dwm.
    if let Some(tag) = key_tag(name).filter(|tag| *tag < tag_count) {
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
    if name.eq_ignore_ascii_case(&keys.output_previous) {
        return if modifiers.shift {
            Action::MoveToOutput(-1)
        } else {
            Action::FocusOutput(-1)
        };
    }
    if name.eq_ignore_ascii_case(&keys.output_next) {
        return if modifiers.shift {
            Action::MoveToOutput(1)
        } else {
            Action::FocusOutput(1)
        };
    }
    #[cfg(feature = "launcher")]
    if name.eq_ignore_ascii_case(&keys.launcher) {
        return Action::OpenLauncher;
    }
    if name.eq_ignore_ascii_case(&keys.terminal) {
        Action::Terminal
    } else if name.eq_ignore_ascii_case(&keys.focus_next) {
        Action::Focus(1)
    } else if name.eq_ignore_ascii_case(&keys.focus_previous) {
        Action::Focus(-1)
    } else if name.eq_ignore_ascii_case(&keys.layout_mode) {
        Action::CycleLayoutMode
    } else if name.eq_ignore_ascii_case(&keys.master_grow) {
        Action::ChangeFactor(0.05)
    } else if name.eq_ignore_ascii_case(&keys.master_shrink) {
        Action::ChangeFactor(-0.05)
    } else {
        Action::None
    }
}

#[cfg(feature = "launcher")]
fn launcher_action(modifiers: ModifiersState, name: &str, character: Option<char>) -> Action {
    match name {
        "Escape" => Action::LauncherClose,
        "Return" | "KP_Enter" => Action::LauncherAccept,
        "BackSpace" => Action::LauncherBackspace,
        "Up" => Action::LauncherSelect(-1),
        "Down" | "Tab" => Action::LauncherSelect(1),
        _ if !modifiers.ctrl && !modifiers.alt && !modifiers.logo => character
            .filter(|character| !character.is_control())
            .map_or(Action::None, Action::LauncherInsert),
        _ => Action::None,
    }
}

/// Resolves the same configurable command modifier for keyboard shortcuts and mouse gestures.
fn configured_modifier_active(keys: &anvil::config::Keys, modifiers: ModifiersState) -> bool {
    match keys.modifier.to_ascii_lowercase().as_str() {
        "alt" => modifiers.alt,
        "ctrl" | "control" => modifiers.ctrl,
        _ => modifiers.logo,
    }
}

fn key_tag(name: &str) -> Option<usize> {
    // User-visible tags are 1..9, but vectors and bit positions are zero-based internally.
    let value = name.parse::<usize>().ok()?;
    (1..=9).contains(&value).then_some(value - 1)
}

#[cfg(test)]
mod pointer_constraint_tests {
    use super::confinement_accepts;
    use smithay::{
        utils::Rectangle,
        wayland::compositor::{RectangleKind, RegionAttributes},
    };

    #[test]
    fn confinement_without_region_follows_surface_bounds() {
        assert!(confinement_accepts(None, (5, 5).into(), true));
        assert!(!confinement_accepts(None, (5, 5).into(), false));
    }

    #[test]
    fn client_region_replaces_full_surface_area() {
        let region = RegionAttributes {
            rects: vec![(
                RectangleKind::Add,
                Rectangle::new((10, 10).into(), (20, 20).into()),
            )],
        };
        assert!(confinement_accepts(Some(&region), (15, 15).into(), false));
        assert!(!confinement_accepts(Some(&region), (5, 5).into(), true));
    }
}

#[cfg(test)]
mod media_key_tests {
    use super::media_command;
    use anvil::config::Media;
    use smithay::input::keyboard::ModifiersState;

    #[test]
    fn unmodified_xf86_keys_execute_only_configured_actions() {
        let media = Media {
            volume_up: "kova-osd volume up".into(),
            volume_down: "kova-osd volume down".into(),
            volume_mute: "kova-osd volume mute".into(),
            brightness_up: "kova-osd brightness up".into(),
            brightness_down: "kova-osd brightness down".into(),
        };
        let normal = ModifiersState::default();
        for (key, expected) in [
            ("XF86AudioRaiseVolume", "kova-osd volume up"),
            ("XF86AudioLowerVolume", "kova-osd volume down"),
            ("XF86AudioMute", "kova-osd volume mute"),
            ("XF86MonBrightnessUp", "kova-osd brightness up"),
            ("XF86MonBrightnessDown", "kova-osd brightness down"),
        ] {
            assert_eq!(
                media_command(&media, normal, key).as_deref(),
                Some(expected)
            );
        }
        assert!(media_command(&media, normal, "XF86AudioPlay").is_none());
        assert!(media_command(&Media::default(), normal, "XF86AudioRaiseVolume").is_none());
        let held_super = ModifiersState {
            logo: true,
            ..ModifiersState::default()
        };
        assert!(media_command(&media, held_super, "XF86AudioRaiseVolume").is_none());
    }
}
