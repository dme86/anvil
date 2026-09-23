//! Core `wl_compositor`, shared-memory buffer and surface-commit handling.
//!
//! Wayland state is double-buffered: clients attach buffers and update properties, then atomically
//! apply them with `wl_surface.commit`. This module connects that commit boundary to Smithay's
//! desktop abstractions and the xdg-shell configure lifecycle.

use crate::{Anvil, state::ClientState};
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_shm,
    reexports::wayland_server::{
        Client,
        protocol::{wl_buffer, wl_surface::WlSurface},
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface,
        },
        shm::{ShmHandler, ShmState},
    },
};

use super::xdg_shell;

impl CompositorHandler for Anvil {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }
    fn commit(&mut self, surface: &WlSurface) {
        // Import or update the newly attached buffer before the renderer tries to use it.
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            // Commits can originate from a child subsurface. Walk to the root so the owning Window
            // refreshes its combined geometry and surface tree exactly once.
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self
                .windows
                .iter()
                .find(|w| w.window.toplevel().is_some_and(|t| t.wl_surface() == &root))
            {
                window.window.on_commit();
            }
        }
        // xdg-shell requires an initial configure handshake for both toplevels and popups. It is
        // driven by the first surface commit, so perform it after generic buffer bookkeeping.
        xdg_shell::handle_commit(&mut self.popups, &self.windows, surface);
    }
}

impl BufferHandler for Anvil {
    // Renderer resources are associated with the wl_buffer by Smithay and disappear with it; Anvil
    // has no additional cache to clean up in the MVP.
    fn buffer_destroyed(&mut self, _: &wl_buffer::WlBuffer) {}
}
impl ShmHandler for Anvil {
    // Shared-memory buffers are the baseline Wayland transport and are also used by software-only
    // clients, so expose Smithay's initialized ShmState through this trait.
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
delegate_compositor!(Anvil);
delegate_shm!(Anvil);
