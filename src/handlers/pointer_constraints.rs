//! Relative-pointer and pointer-constraint protocol policy.
//!
//! Smithay owns the protocol objects and automatically deactivates an active constraint when its
//! surface loses pointer focus. Anvil only decides when a newly focused constraint becomes active,
//! how motion is restricted, and how a lock's final cursor-position hint maps back to desktop
//! coordinates.

use crate::Anvil;
use smithay::{
    delegate_pointer_constraints, delegate_relative_pointer,
    input::pointer::PointerHandle,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
    wayland::pointer_constraints::{
        PointerConstraint, PointerConstraintsHandler, with_pointer_constraint,
    },
};

impl PointerConstraintsHandler for Anvil {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // A constraint is meaningful only while its surface owns pointer focus. Activating here
        // handles clients that create it after enter; the input path handles the inverse order.
        if pointer.current_focus().as_ref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(constraint) = constraint.filter(|constraint| !constraint.is_active()) {
                    constraint.activate();
                }
            });
        }
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        // Position hints are surface-local and only affect an active lock. They become the logical
        // cursor position used after unlocking; clients never observe an artificial motion event.
        let active_lock = with_pointer_constraint(surface, pointer, |constraint| {
            constraint.is_some_and(|constraint| {
                constraint.is_active() && matches!(*constraint, PointerConstraint::Locked(_))
            })
        });
        if active_lock {
            if let Some((_, origin)) = self.pointer_focus_with_origin(surface) {
                pointer.set_location(origin + location);
            }
        }
    }
}

delegate_relative_pointer!(Anvil);
delegate_pointer_constraints!(Anvil);
