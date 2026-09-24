//! Glue for Smithay protocol delegation that does not need a dedicated policy module.
//!
//! Smithay's delegate macros generate Wayland `Dispatch` implementations. The traits below point
//! those generated implementations at the protocol state stored inside `Anvil` and provide the few
//! callbacks where the compositor must make a decision.

mod compositor;
mod xdg_shell;

use crate::Anvil;
use smithay::{
    delegate_data_device, delegate_output, delegate_seat,
    input::{Seat, SeatHandler, SeatState, pointer::CursorImageStatus},
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    wayland::{
        output::OutputHandler,
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
        },
    },
};

impl SeatHandler for Anvil {
    // This minimal compositor focuses wl_surfaces directly. More elaborate compositors often use a
    // wrapper enum here to focus layer-shell, lock-screen and XWayland surfaces as well.
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
    fn cursor_image(&mut self, _: &Seat<Self>, _: CursorImageStatus) {}
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        // Clipboard/drag-and-drop focus follows keyboard focus. The data-device protocol operates
        // on clients rather than individual surfaces, hence the surface-to-client lookup.
        let client = focused.and_then(|surface| self.display_handle.get_client(surface.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client);
    }
}
delegate_seat!(Anvil);

// Data-device state supplies the standard Wayland clipboard and drag-and-drop protocol. Anvil does
// not inspect payloads, so empty grab handlers and unit user data are sufficient.
impl SelectionHandler for Anvil {
    type SelectionUserData = ();
}
impl DataDeviceHandler for Anvil {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl ClientDndGrabHandler for Anvil {}
impl ServerDndGrabHandler for Anvil {}
delegate_data_device!(Anvil);

// Output resources and hotplug globals are owned by the active backend. Smithay handles client
// requests uniformly for one nested output or several independently configured DRM connectors.
impl OutputHandler for Anvil {}
delegate_output!(Anvil);
