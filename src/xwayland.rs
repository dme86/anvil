//! Optional XWayland process lifecycle.
//!
//! XWayland is deliberately started only when both the Cargo feature and runtime configuration
//! opt in. The event source owns the child, so ending the compositor also ends the compatibility
//! server; startup failures are logged and leave the native Wayland session usable.

use std::{os::fd::OwnedFd, process::Stdio};

use smithay::{
    reexports::calloop::EventLoop,
    utils::{Logical, Rectangle},
    wayland::{
        selection::SelectionTarget,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge, WmWindowProperty, XwmId},
    },
};

use crate::CalloopData;

impl XWaylandShellHandler for CalloopData {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.state.xwayland_shell_state
    }
}

impl XwmHandler for CalloopData {
    fn xwm_state(&mut self, id: XwmId) -> &mut X11Wm {
        self.state.xwm_state(id)
    }
    fn new_window(&mut self, id: XwmId, window: X11Surface) {
        self.state.new_window(id, window)
    }
    fn new_override_redirect_window(&mut self, id: XwmId, window: X11Surface) {
        self.state.new_override_redirect_window(id, window)
    }
    fn map_window_request(&mut self, id: XwmId, window: X11Surface) {
        self.state.map_window_request(id, window)
    }
    fn mapped_override_redirect_window(&mut self, id: XwmId, window: X11Surface) {
        self.state.mapped_override_redirect_window(id, window)
    }
    fn unmapped_window(&mut self, id: XwmId, window: X11Surface) {
        self.state.unmapped_window(id, window)
    }
    fn destroyed_window(&mut self, id: XwmId, window: X11Surface) {
        self.state.destroyed_window(id, window)
    }
    fn configure_request(
        &mut self,
        id: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        width: Option<u32>,
        height: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        self.state
            .configure_request(id, window, x, y, width, height, reorder)
    }
    fn configure_notify(
        &mut self,
        id: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        above: Option<u32>,
    ) {
        self.state.configure_notify(id, window, geometry, above)
    }
    fn property_notify(&mut self, id: XwmId, window: X11Surface, property: WmWindowProperty) {
        self.state.property_notify(id, window, property)
    }
    fn resize_request(&mut self, id: XwmId, window: X11Surface, button: u32, edge: ResizeEdge) {
        self.state.resize_request(id, window, button, edge)
    }
    fn move_request(&mut self, id: XwmId, window: X11Surface, button: u32) {
        self.state.move_request(id, window, button)
    }
    fn send_selection(&mut self, id: XwmId, selection: SelectionTarget, mime: String, fd: OwnedFd) {
        self.state.send_selection(id, selection, mime, fd)
    }
    fn disconnected(&mut self, id: XwmId) {
        self.state.disconnected(id)
    }
}

pub fn init(event_loop: &mut EventLoop<'static, CalloopData>, data: &mut CalloopData) {
    let (xwayland, client) = match XWayland::spawn(
        &data.display_handle,
        None,
        std::iter::empty::<(&str, &str)>(),
        true,
        Stdio::null(),
        Stdio::null(),
        |_| (),
    ) {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(%error, "XWayland is enabled but could not be started");
            return;
        }
    };

    let display = format!(":{}", xwayland.display_number());
    data.state.xwayland_display = Some(display.clone());
    let handle = event_loop.handle();
    let wm_handle = handle.clone();
    if let Err(error) = handle.insert_source(xwayland, move |event, _, data| match event {
        XWaylandEvent::Ready {
            x11_socket,
            display_number,
        } => match X11Wm::start_wm(wm_handle.clone(), x11_socket, client.clone()) {
            Ok(wm) => {
                data.state.xwm = Some(wm);
                tracing::info!(display = %format!(":{display_number}"), "XWayland is ready");
            }
            Err(error) => tracing::warn!(%error, "cannot attach the XWayland window manager"),
        },
        XWaylandEvent::Error => {
            data.state.xwm = None;
            data.state.xwayland_display = None;
            tracing::warn!("XWayland exited; native Wayland clients remain available");
        }
    }) {
        tracing::warn!(%error, "cannot register XWayland with the event loop");
        data.state.xwayland_display = None;
    }
}
