//! XWayland window-manager policy.
//!
//! X11 toplevels enter the same `ManagedWindow` list as native xdg toplevels. This intentionally
//! avoids a parallel workspace model: tags, layouts, focus, floating gestures and output moves
//! operate on Smithay's common `Window` wrapper for both protocols.

use std::os::fd::OwnedFd;

use smithay::{
    delegate_xwayland_shell,
    desktop::Window,
    utils::{Logical, Rectangle},
    wayland::{
        selection::SelectionTarget,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XwmHandler,
        xwm::{Reorder, ResizeEdge, WmWindowProperty, XwmId},
    },
};

use crate::Anvil;

impl XWaylandShellHandler for Anvil {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(
        &mut self,
        _: XwmId,
        _: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        surface: X11Surface,
    ) {
        tracing::debug!(
            window = surface.window_id(),
            "associated X11 and Wayland surfaces"
        );
        if surface.is_mapped() {
            self.manage_x11_window(surface);
        }
    }
}
delegate_xwayland_shell!(Anvil);

impl XwmHandler for Anvil {
    fn xwm_state(&mut self, _: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("XWM callback without active XWM")
    }

    fn new_window(&mut self, _: XwmId, window: X11Surface) {
        tracing::debug!(window = window.window_id(), "created X11 window");
    }
    fn new_override_redirect_window(&mut self, _: XwmId, _: X11Surface) {}

    fn map_window_request(&mut self, _: XwmId, window: X11Surface) {
        tracing::debug!(window = window.window_id(), "X11 window requested mapping");
        if let Err(error) = window.set_mapped(true) {
            tracing::warn!(%error, "cannot map X11 window");
            return;
        }
        if window.wl_surface().is_some() {
            self.manage_x11_window(window);
        }
    }

    fn mapped_override_redirect_window(&mut self, _: XwmId, window: X11Surface) {
        // Override-redirect helpers still need rendering. Their popup/transient metadata makes
        // them float, while the common Window wrapper keeps hit testing and lifetime cleanup
        // identical to ordinary X11 clients.
        if window.wl_surface().is_some() {
            self.manage_x11_window(window);
        }
    }

    fn unmapped_window(&mut self, _: XwmId, window: X11Surface) {
        self.remove_x11_window(window.window_id());
    }

    fn destroyed_window(&mut self, _: XwmId, window: X11Surface) {
        self.remove_x11_window(window.window_id());
    }

    fn configure_request(
        &mut self,
        _: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        width: Option<u32>,
        height: Option<u32>,
        _: Option<Reorder>,
    ) {
        let old = window.geometry();
        let requested = Rectangle::new(
            (x.unwrap_or(old.loc.x), y.unwrap_or(old.loc.y)).into(),
            (
                width.map_or(old.size.w, |value| value as i32),
                height.map_or(old.size.h, |value| value as i32),
            )
                .into(),
        );
        // Managed windows keep compositor-selected geometry. Before association/mapping, honoring
        // the request records a useful initial size without bypassing later tiling policy.
        if !self.has_x11_window(window.window_id()) {
            let _ = window.configure(requested);
        } else {
            let _ = window.configure(None);
        }
    }

    fn configure_notify(
        &mut self,
        _: XwmId,
        _: X11Surface,
        _: Rectangle<i32, Logical>,
        _: Option<u32>,
    ) {
    }

    fn property_notify(&mut self, _: XwmId, window: X11Surface, _: WmWindowProperty) {
        self.refresh_x11_rule(&window);
        self.request_repaint();
    }

    fn resize_request(&mut self, _: XwmId, _: X11Surface, _: u32, _: ResizeEdge) {}
    fn move_request(&mut self, _: XwmId, _: X11Surface, _: u32) {}
    fn send_selection(&mut self, _: XwmId, _: SelectionTarget, _: String, _: OwnedFd) {}

    fn disconnected(&mut self, _: XwmId) {
        self.xwm = None;
        self.xwayland_display = None;
        self.windows
            .retain(|managed| managed.window.x11_surface().is_none());
        self.arrange();
    }
}

impl Anvil {
    fn has_x11_window(&self, id: u32) -> bool {
        self.windows.iter().any(|managed| {
            managed
                .window
                .x11_surface()
                .is_some_and(|surface| surface.window_id() == id)
        })
    }

    fn manage_x11_window(&mut self, surface: X11Surface) {
        if self.has_x11_window(surface.window_id()) {
            return;
        }
        let window = Window::new_x11_window(surface.clone());
        self.add_window(window);
        self.refresh_x11_rule(&surface);
    }

    fn remove_x11_window(&mut self, id: u32) {
        let Some(surface) = self.windows.iter().find_map(|managed| {
            managed
                .window
                .x11_surface()
                .filter(|surface| surface.window_id() == id)
                .and_then(X11Surface::wl_surface)
        }) else {
            return;
        };
        self.remove_window(&surface);
    }

    fn refresh_x11_rule(&mut self, surface: &X11Surface) {
        let Some(wl_surface) = surface.wl_surface() else {
            return;
        };
        let class = surface.class();
        let title = surface.title();
        self.refresh_window_rule(
            &wl_surface,
            (!class.is_empty()).then_some(class.as_str()),
            (!title.is_empty()).then_some(title.as_str()),
            surface.is_popup() || surface.is_transient_for().is_some(),
        );
    }
}
