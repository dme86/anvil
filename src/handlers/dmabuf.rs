//! linux-dmabuf protocol policy shared by the Wayland frontend and graphics backend.
//!
//! Protocol objects belong to `Anvil`, while the renderer remains owned by the selected backend.
//! The backend installs an import probe during initialization so a client buffer is accepted only
//! after the compositor renderer imports its exact format and modifier successfully.

use crate::Anvil;
use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    delegate_dmabuf,
    wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
};

impl DmabufHandler for Anvil {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        let imported = self
            .dmabuf_importer
            .as_mut()
            .is_some_and(|importer| importer(&dmabuf));
        if imported {
            let _ = notifier.successful::<Anvil>();
        } else {
            // Smithay translates this into the protocol-defined asynchronous import rejection.
            notifier.failed();
        }
    }
}

delegate_dmabuf!(Anvil);
