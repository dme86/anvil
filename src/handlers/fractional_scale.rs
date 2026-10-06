//! Fractional output scale and surface viewport protocol glue.
//!
//! Output scale is already part of Smithay's `Output` state and Anvil's renderer consumes the same
//! value. This module connects that existing state to clients, avoiding a second scale model that
//! could disagree with layout or rendering after a window moves between displays.

use crate::Anvil;
use smithay::{
    delegate_fractional_scale, delegate_viewporter,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::fractional_scale::FractionalScaleHandler,
};

impl FractionalScaleHandler for Anvil {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        if !self
            .fractional_scale_surfaces
            .iter()
            .any(|known| known == &surface)
        {
            self.fractional_scale_surfaces.push(surface);
        }
        self.refresh_fractional_scales();
    }
}

delegate_fractional_scale!(Anvil);
delegate_viewporter!(Anvil);
