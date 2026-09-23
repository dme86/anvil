//! Central compositor state and window-management operations.
//!
//! Smithay protocol implementations are callback based. `Anvil` is the single state object shared
//! with those callbacks through Calloop, while this module owns the policy decisions that make the
//! program a window manager: window order, tags, focus, layout and process spawning.

use std::{ffi::OsString, process::Command, sync::Arc};

use anvil::{
    config::Config,
    layout::{Rect, tile},
};
use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState},
    reexports::{
        calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction, generic::Generic},
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{IsAlive, Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::xdg::{XdgShellState, decoration::XdgDecorationState},
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};

use crate::CalloopData;

/// A Smithay window plus Anvil-specific metadata.
///
/// Tags are a bit mask instead of a workspace number, matching dwm's model. A window can therefore
/// be extended to appear on several tags without replacing the data representation later.
pub struct ManagedWindow {
    pub window: Window,
    pub tags: u16,
}

/// All mutable state required by the compositor and its Wayland protocol delegates.
///
/// Smithay's `*State` fields publish and implement individual protocol globals. They are stored
/// together because the delegate macros below the handler implementations need to retrieve them
/// from the application state whenever a client sends a request.
pub struct Anvil {
    /// Monotonic origin used for frame callback timestamps.
    pub start_time: std::time::Instant,
    /// Auto-selected socket name advertised to child processes as `WAYLAND_DISPLAY`.
    pub socket_name: OsString,
    /// Handle used to add, inspect and flush Wayland clients.
    pub display_handle: DisplayHandle,
    /// Smithay's map of visible windows and outputs in logical coordinates.
    pub space: Space<Window>,
    /// Stable window order and tag ownership, including currently hidden windows.
    pub windows: Vec<ManagedWindow>,
    /// Bit mask of tags currently shown; the MVP selects one bit at a time.
    pub selected_tags: u16,
    /// Usable logical output area passed to the backend-independent layout engine.
    pub output_area: Rect,
    /// Allows a key binding or backend close event to stop Calloop cleanly.
    pub loop_signal: LoopSignal,
    pub config: Config,
    // Protocol state objects retained for Smithay's generated dispatch implementations.
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    /// Negotiates client-side versus server-side title bars for xdg toplevels.
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Anvil>,
    pub data_device_state: DataDeviceState,
    pub popups: PopupManager,
    pub seat: Seat<Self>,
}

impl Anvil {
    /// Builds protocol globals, input capabilities and the Wayland listening socket.
    pub fn new(
        event_loop: &mut EventLoop<CalloopData>,
        display: Display<Self>,
        config: Config,
    ) -> Self {
        let dh = display.handle();
        // Creating these state objects registers the corresponding globals with the display. A
        // client cannot create surfaces, xdg toplevels, shared-memory buffers, seats or clipboard
        // objects until the matching global has been advertised.
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        // Advertising xdg-decoration lets cooperating clients omit their own title bars. We select
        // server-side mode by default but deliberately draw no server frame, yielding undecorated
        // tiled windows without relying on toolkit-specific environment variables.
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        // A Wayland seat groups related input devices. The nested backend always exposes one
        // keyboard and pointer, so declaring both once is more honest than hot-plug bookkeeping.
        let mut seat = seat_state.new_wl_seat(&dh, "seat-0");
        seat.add_keyboard(Default::default(), 200, 25)
            .expect("keyboard initialization failed");
        seat.add_pointer();

        let socket_name = Self::init_wayland_listener(display, event_loop);
        Self {
            start_time: std::time::Instant::now(),
            socket_name,
            display_handle: dh,
            space: Space::default(),
            windows: Vec::new(),
            // Start on tag 1. Tags are zero-indexed in code, hence the least significant bit.
            selected_tags: 1,
            output_area: Rect::default(),
            loop_signal: event_loop.get_signal(),
            config,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            popups: PopupManager::default(),
            seat,
        }
    }

    fn init_wayland_listener(
        display: Display<Self>,
        event_loop: &mut EventLoop<CalloopData>,
    ) -> OsString {
        // Let Wayland choose the first free `wayland-N` socket. Hard-coding `wayland-0` would
        // collide with the host compositor when Anvil is run nested.
        let socket = ListeningSocketSource::new_auto().expect("failed to create Wayland socket");
        let name = socket.socket_name().to_os_string();
        let handle = event_loop.handle();
        // Accepting a Unix socket connection only creates a transport. `insert_client` attaches it
        // to the Wayland display and adds per-client compositor state used during dispatch.
        handle
            .insert_source(socket, |stream, _, data| {
                data.display_handle
                    .insert_client(stream, Arc::new(ClientState::default()))
                    .unwrap();
            })
            .expect("failed to register Wayland socket");
        // The display itself is an event source as well. When client bytes become readable,
        // dispatching converts them into the typed handler calls implemented under `handlers/`.
        handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, data| {
                    // SAFETY: the display stays owned by this event source for the loop lifetime.
                    unsafe {
                        display.get_mut().dispatch_clients(&mut data.state).unwrap();
                    }
                    Ok(PostAction::Continue)
                },
            )
            .expect("failed to register Wayland display");
        name
    }

    pub fn spawn(&self, command: &str) {
        // Use a shell because configuration commands commonly include arguments, quoting or
        // pipelines. Children inherit WAYLAND_DISPLAY, so Wayland applications connect to Anvil.
        if let Err(error) = Command::new("/bin/sh").arg("-c").arg(command).spawn() {
            tracing::error!(%error, %command, "failed to start command");
        }
    }

    pub fn add_window(&mut self, window: Window) {
        // New windows inherit the active tag, exactly like dwm. Insertion order is layout order;
        // keeping that rule explicit makes “swap master” a simple vector swap.
        self.windows.push(ManagedWindow {
            window,
            tags: self.selected_tags,
        });
        self.arrange();
        self.focus_index(self.visible_indices().len().saturating_sub(1));
    }

    pub fn arrange(&mut self) {
        // Smithay resources may die asynchronously after a client disconnects. Prune dead handles
        // before computing geometry so closed windows never reserve a tile.
        self.windows.retain(|managed| managed.window.alive());
        let visible = self.visible_indices();
        let geometries = tile(self.output_area, visible.len(), &self.config.layout);
        // `Space` contains only visible windows. Unmapping everything first also removes windows
        // from the previous tag; the persistent `windows` vector still retains their metadata.
        for managed in &self.windows {
            self.space.unmap_elem(&managed.window);
        }
        for (index, geometry) in visible.into_iter().zip(geometries) {
            let window = self.windows[index].window.clone();
            if let Some(toplevel) = window.toplevel() {
                // xdg-shell sizes are negotiated, not imposed by mutating a buffer. Write the
                // desired size into pending state and send configure so the client can redraw.
                toplevel.with_pending_state(|state| {
                    state.size = Some((geometry.width, geometry.height).into())
                });
                toplevel.send_pending_configure();
            }
            // Once mapped, `Space` supplies hit testing, stacking and render traversal.
            self.space
                .map_element(window, (geometry.x, geometry.y), false);
        }
    }

    pub fn focus_index(&mut self, visible_index: usize) {
        let visible = self.visible_indices();
        let Some(&index) = visible.get(visible_index) else {
            return;
        };
        // Wayland uses monotonically increasing serials to order focus and input transitions.
        let serial = SERIAL_COUNTER.next_serial();
        let target = self.windows[index].window.clone();
        // Activation is separate from keyboard focus. Clients use it to draw active/inactive UI,
        // so keep every toplevel's xdg state synchronized with the chosen target.
        for managed in &self.windows {
            managed.window.set_activated(managed.window == target);
            if let Some(toplevel) = managed.window.toplevel() {
                toplevel.send_pending_configure();
            }
        }
        // Raising matters for popups and any future floating windows even though tiled rectangles
        // normally do not overlap.
        self.space.raise_element(&target, true);
        self.seat.get_keyboard().unwrap().set_focus(
            self,
            target
                .toplevel()
                .map(|surface| surface.wl_surface().clone()),
            serial,
        );
    }

    pub fn focus_relative(&mut self, delta: isize) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        let focused = self.seat.get_keyboard().unwrap().current_focus();
        let current = focused
            .and_then(|surface| {
                visible.iter().position(|&i| {
                    self.windows[i]
                        .window
                        .toplevel()
                        .is_some_and(|t| t.wl_surface() == &surface)
                })
            })
            .unwrap_or(0);
        // Euclidean remainder wraps in both directions; ordinary `%` would stay negative for `-1`.
        self.focus_index((current as isize + delta).rem_euclid(visible.len() as isize) as usize);
    }

    pub fn swap_master(&mut self) {
        let visible = self.visible_indices();
        if visible.len() < 2 {
            return;
        }
        let focused = self.seat.get_keyboard().unwrap().current_focus();
        let selected = focused
            .and_then(|surface| {
                visible.iter().position(|&i| {
                    self.windows[i]
                        .window
                        .toplevel()
                        .is_some_and(|t| t.wl_surface() == &surface)
                })
            })
            .unwrap_or(1);
        // The layout engine assigns the first visible entry to master, so changing vector order is
        // enough. No separate “master window” reference can become stale.
        self.windows.swap(visible[0], visible[selected]);
        self.arrange();
        self.focus_index(0);
    }

    pub fn select_tag(&mut self, tag: usize) {
        // Selecting a tag changes visibility only; client surfaces remain alive and continue to be
        // owned by their applications while absent from `Space`.
        self.selected_tags = 1 << tag;
        self.arrange();
        self.focus_index(0);
    }

    pub fn move_focused_to_tag(&mut self, tag: usize) {
        let focused = self.seat.get_keyboard().unwrap().current_focus();
        if let Some(surface) = focused {
            if let Some(managed) = self.windows.iter_mut().find(|m| {
                m.window
                    .toplevel()
                    .is_some_and(|t| t.wl_surface() == &surface)
            }) {
                managed.tags = 1 << tag;
            }
        }
        self.arrange();
        self.focus_index(0);
    }

    pub fn close_focused(&mut self) {
        let focused = self.seat.get_keyboard().unwrap().current_focus();
        if let Some(surface) = focused {
            if let Some(toplevel) = self
                .windows
                .iter()
                .find_map(|m| m.window.toplevel().filter(|t| t.wl_surface() == &surface))
            {
                // xdg-shell defines close as a polite request. The client decides when it has
                // saved state and destroyed the surface; force-killing it would risk user data.
                toplevel.send_close();
            }
        }
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        // Pointer focus needs the concrete wl_surface and surface-local origin, not merely Anvil's
        // top-level Window. `surface_under` descends into subsurfaces such as client-side menus.
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(surface, point)| (surface, (point + location).to_f64()))
            })
    }

    pub fn set_output_size(&mut self, width: i32, height: i32) {
        // A backend resize invalidates every tile, so treat it like any other layout change.
        self.output_area = Rect::new(0, 0, width, height);
        self.arrange();
    }

    fn visible_indices(&self) -> Vec<usize> {
        // A non-zero bit intersection implements dwm-style tag visibility and already supports a
        // future multi-tag view without changing individual window records.
        self.windows
            .iter()
            .enumerate()
            .filter_map(|(i, w)| (w.tags & self.selected_tags != 0).then_some(i))
            .collect()
    }

    pub fn output_rectangle(&self) -> Rectangle<i32, Logical> {
        // Popup helpers operate on Smithay's typed rectangles, whereas the pure layout module uses
        // its backend-independent Rect. Keep the conversion at this integration boundary.
        Rectangle::new(
            (self.output_area.x, self.output_area.y).into(),
            (self.output_area.width, self.output_area.height).into(),
        )
    }

    /// Returns the visible geometry of the keyboard-focused toplevel.
    ///
    /// Rendering asks for this rather than caching a second “focused window” field. The Wayland
    /// seat remains the single source of truth, preventing border and keyboard focus from drifting
    /// apart after a tag switch, client exit or pointer focus change.
    pub fn focused_window_geometry(&self) -> Option<Rectangle<i32, Logical>> {
        let focused = self.seat.get_keyboard()?.current_focus()?;
        let window = self.windows.iter().find(|managed| {
            managed
                .window
                .toplevel()
                .is_some_and(|toplevel| toplevel.wl_surface() == &focused)
        })?;
        self.space.element_geometry(&window.window)
    }
}

#[derive(Default)]
/// Per-client data required by Smithay's compositor implementation.
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}
