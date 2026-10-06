//! Compositor policy for the `ext-session-lock-v1` security boundary.
//!
//! Lock surfaces are deliberately not inserted into the ordinary desktop `Space`: doing so would
//! let stacking or layout operations expose normal clients. Backends instead switch to a separate
//! render path while this state is active and draw only the matching lock surface over black.

use crate::{Anvil, state::SessionLockSurface};
use smithay::{
    delegate_session_lock,
    input::pointer::MotionEvent,
    output::Output,
    reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    utils::{IsAlive, SERIAL_COUNTER},
    wayland::session_lock::{
        LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
    },
};

impl SessionLockHandler for Anvil {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        // Smithay turns a dropped, unconfirmed request into `finished`. Keeping the existing lock
        // active is the only secure response to a second locker racing the current owner.
        if self.session_lock.active {
            return;
        }

        self.session_lock.active = true;
        self.session_lock.surfaces.clear();
        self.session_lock.secured_outputs.clear();
        self.session_lock.previous_keyboard_focus = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        self.session_lock.confirmation = Some(confirmation);
        self.cancel_pointer_operation();

        let serial = SERIAL_COUNTER.next_serial();
        self.seat
            .get_keyboard()
            .unwrap()
            .set_focus(self, Option::<WlSurface>::None, serial);
        let pointer = self.seat.get_pointer().unwrap();
        let location = pointer.current_location();
        pointer.motion(
            self,
            None,
            &MotionEvent {
                location,
                serial,
                time: 0,
            },
        );
        pointer.frame(self);

        self.request_repaint();
        // A headless compositor has no stale scanout to hide, so confirmation can be immediate.
        self.confirm_session_lock_if_ready();
    }

    fn unlock(&mut self) {
        self.session_lock.active = false;
        self.session_lock.surfaces.clear();
        self.session_lock.secured_outputs.clear();
        self.session_lock.confirmation = None;

        let previous = self
            .session_lock
            .previous_keyboard_focus
            .take()
            .filter(|surface| surface.alive());
        self.seat
            .get_keyboard()
            .unwrap()
            .set_focus(self, previous, SERIAL_COUNTER.next_serial());

        let pointer = self.seat.get_pointer().unwrap();
        let location = pointer.current_location();
        let focus = self.surface_under(location);
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: 0,
            },
        );
        pointer.frame(self);
        self.request_repaint();
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let Some(output) = Output::from_resource(&wl_output) else {
            return;
        };
        if !self.session_lock.active {
            return;
        }

        let output_name = output.name();
        let Some(area) = self
            .outputs
            .iter()
            .find(|candidate| candidate.name == output_name)
            .map(|candidate| candidate.screen_area)
        else {
            return;
        };
        surface.with_pending_state(|state| {
            state.size = Some((area.width as u32, area.height as u32).into());
        });
        surface.send_configure();
        self.session_lock.surfaces.push(SessionLockSurface {
            output,
            surface: surface.clone(),
        });

        // Keyboard focus follows the lock surface on the focused display. If that surface has not
        // arrived yet, the first available lock surface is still safer than leaving focus empty.
        let focused = self.focused_output.as_deref() == Some(output_name.as_str())
            || self.session_lock.surfaces.len() == 1;
        if focused {
            self.seat.get_keyboard().unwrap().set_focus(
                self,
                Some(surface.wl_surface().clone()),
                SERIAL_COUNTER.next_serial(),
            );
        }

        let pointer = self.seat.get_pointer().unwrap();
        let location = pointer.current_location();
        let pointer_focus = self.surface_under(location);
        pointer.motion(
            self,
            pointer_focus,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: 0,
            },
        );
        pointer.frame(self);
        self.request_repaint();
    }
}

delegate_session_lock!(Anvil);
