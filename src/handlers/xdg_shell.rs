//! Policy for xdg-shell toplevels and popups.
//!
//! xdg-shell turns raw `wl_surface`s into desktop concepts. Toplevel creation enters Anvil's tiled
//! window list, while popups are tracked separately because their position is relative to a parent
//! surface and they should never consume a tile.

use crate::{Anvil, state::ManagedWindow};
use smithay::{
    delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{
        PopupKind, PopupManager, Window, find_popup_root_surface, get_popup_toplevel_coords,
    },
    reexports::{
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode, shell::server::xdg_toplevel,
        },
        wayland_server::protocol::{wl_seat, wl_surface::WlSurface},
    },
    utils::Serial,
    wayland::{
        compositor::with_states,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData, decoration::XdgDecorationHandler,
        },
    },
};

impl XdgShellHandler for Anvil {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // `Window` gives Space a unified geometry/render/hit-test abstraction over the xdg surface.
        self.add_window(Window::new_wayland_window(surface));
    }
    fn new_popup(&mut self, surface: PopupSurface, _: PositionerState) {
        // Constrain menus and tooltips before tracking them so they remain inside the output even
        // when their requested anchor is close to an edge.
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }
    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // This callback is the authoritative end of an xdg-toplevel's lifetime. Removing it here
        // makes a close operation immediately recompute the dynamic layout instead of leaving the
        // dead master rectangle behind until some unrelated key binding happens to call arrange.
        self.remove_window(surface.wl_surface());
    }
    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        // Reposition requests are double-buffered just like other xdg state. Store the new request,
        // apply output constraints, then acknowledge the client's token.
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }
    // Client-initiated interactive move/resize requests are intentionally ignored. Tiled clients
    // receive compositor-selected geometry; floating support can add explicit grabs later.
    fn move_request(&mut self, _: ToplevelSurface, _: wl_seat::WlSeat, _: Serial) {}
    fn resize_request(
        &mut self,
        _: ToplevelSurface,
        _: wl_seat::WlSeat,
        _: Serial,
        _: xdg_toplevel::ResizeEdge,
    ) {
    }
    // Popup grabs (click-outside dismissal and nested popup chains) are a later protocol milestone.
    fn grab(&mut self, _: PopupSurface, _: wl_seat::WlSeat, _: Serial) {}
}
delegate_xdg_shell!(Anvil);

impl XdgDecorationHandler for Anvil {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // Negotiate a mode as soon as a client creates its decoration object. Server-side mode
        // means “do not draw client-side controls”; because Anvil draws no SSD frame, the resulting
        // tiled window has no minimize, maximize or close buttons at all.
        self.configure_decoration(&toplevel, None);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, requested: Mode) {
        // A client preference is only honored when explicitly enabled in config. Otherwise every
        // request is overridden with ServerSide so toolkits cannot reintroduce a title bar.
        self.configure_decoration(&toplevel, Some(requested));
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        // Unsetting returns to Anvil's configured default, not the toolkit's implicit default.
        self.configure_decoration(&toplevel, None);
    }
}
delegate_xdg_decoration!(Anvil);

pub fn handle_commit(popups: &mut PopupManager, windows: &[ManagedWindow], surface: &WlSurface) {
    // A newly created xdg_toplevel must receive configure before it is allowed to attach its first
    // real buffer. Track Smithay's handshake flag to send this exactly once.
    if let Some(window) = windows.iter().find(|w| {
        w.window
            .toplevel()
            .is_some_and(|t| t.wl_surface() == surface)
    }) {
        let sent = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });
        if !sent {
            window.window.toplevel().unwrap().send_configure();
        }
    }
    // PopupManager needs every commit to update popup surface trees and destruction state.
    popups.commit(surface);
    if let Some(PopupKind::Xdg(popup)) = popups.find_popup(surface) {
        if !popup.is_initial_configure_sent() {
            popup
                .send_configure()
                .expect("initial popup configure failed");
        }
    }
}

impl Anvil {
    fn configure_decoration(&mut self, toplevel: &ToplevelSurface, requested: Option<Mode>) {
        let mode = if self.config.appearance.client_side_decorations
            && requested != Some(Mode::ServerSide)
        {
            Mode::ClientSide
        } else {
            Mode::ServerSide
        };

        // Decoration mode is part of xdg_toplevel's double-buffered configure state. If the normal
        // initial configure has already happened, send a fresh configure so the client observes
        // this later negotiation immediately.
        toplevel.with_pending_state(|state| state.decoration_mode = Some(mode));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        // Popup positioner coordinates are relative to their parent tree. Smithay's unconstrain
        // helper expects the allowed rectangle in that same coordinate system, so translate the
        // output rectangle by both the popup-root offset and the tiled window location.
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self
            .windows
            .iter()
            .find(|w| w.window.toplevel().is_some_and(|t| t.wl_surface() == &root))
        else {
            return;
        };
        let Some(location) = self.space.element_location(&window.window) else {
            return;
        };
        let mut target = self.output_rectangle();
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= location;
        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target)
        });
    }
}
