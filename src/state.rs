//! Central compositor state and window-management operations.
//!
//! Smithay protocol implementations are callback based. `Anvil` is the single state object shared
//! with those callbacks through Calloop, while this module owns the policy decisions that make the
//! program a window manager: window order, tags, focus, layout and process spawning.

use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Child, Command},
    sync::Arc,
};

use anvil::{
    config::Config,
    layout::{LayoutMode, Rect, tile},
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
#[cfg(feature = "bar")]
use crate::bar::{BarSnapshot, BarState, BarWindow};
#[cfg(feature = "launcher")]
use crate::launcher::{LaunchCommand, LauncherSnapshot, LauncherState};
#[cfg(feature = "anvilctl")]
use anvil::ipc::WindowInfo;
#[cfg(any(feature = "bar", feature = "anvilctl"))]
use smithay::wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData};

/// A Smithay window plus Anvil-specific metadata.
///
/// Tags are a bit mask instead of a workspace number, matching dwm's model. A window can therefore
/// be extended to appear on several tags without replacing the data representation later.
pub struct ManagedWindow {
    pub window: Window,
    /// Stable wl_output name owning this window. Tags are interpreted inside this output only.
    pub output: String,
    pub tags: u16,
    /// Whether metadata/rules keep this client floating even while the global mode is Tiling.
    pub rule_floating: bool,
    /// Last independent geometry used by either a rule or the global Floating mode.
    /// Geometry and policy are separate so moving a window does not permanently change its rule.
    pub floating_geometry: Option<Rect>,
}

/// Per-output desktop policy. Geometry is stored in global compositor coordinates, while every
/// output owns its tag selection and layout mode just like an independent dwm screen.
#[derive(Clone, Debug)]
pub struct OutputWorkspace {
    pub name: String,
    pub screen_area: Rect,
    pub output_area: Rect,
    pub selected_tags: u16,
    pub layout_mode: LayoutMode,
}

#[derive(Clone, Copy, Debug)]
pub enum PointerOperationKind {
    Move,
    ResizeHorizontal,
    ResizeVertical,
    ResizeBoth,
}

#[derive(Clone, Debug)]
struct PointerOperation {
    window: Window,
    button: u32,
    start_pointer: Point<f64, Logical>,
    start_geometry: Rect,
    kind: PointerOperationKind,
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
    /// Connected displays and their independent tag/layout state, ordered left to right.
    pub outputs: Vec<OutputWorkspace>,
    /// Output receiving keyboard commands and newly created windows.
    pub focused_output: Option<String>,
    /// Active compositor-owned mouse gesture. Its button is not forwarded to the client.
    pointer_operation: Option<PointerOperation>,
    /// Allows a key binding or backend close event to stop Calloop cleanly.
    pub loop_signal: LoopSignal,
    pub config: Config,
    /// Exact file selected at startup, reused by `anvilctl reload`.
    #[cfg(feature = "anvilctl")]
    pub config_path: Option<PathBuf>,
    /// Bound control socket removed when the compositor exits normally.
    #[cfg(feature = "anvilctl")]
    pub control_socket_path: Option<PathBuf>,
    /// Whether the direct backend must sample scene state and attempt a new KMS frame.
    ///
    /// Keeping this bit in shared compositor state lets protocol commits and input handlers wake
    /// rendering without forcing the DRM backend to redraw continuously while the desktop is idle.
    pub repaint_requested: bool,
    /// Spawned commands remain owned until `try_wait` observes their exit. Dropping a `Child`
    /// handle without waiting leaves a zombie on Linux, which is especially visible in a tiny VM.
    children: Vec<Child>,
    #[cfg(feature = "bar")]
    pub bar: BarState,
    #[cfg(feature = "launcher")]
    pub launcher: LauncherState,
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
        #[cfg(feature = "anvilctl")] config_path: Option<PathBuf>,
        #[cfg(not(feature = "anvilctl"))] _config_path: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
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
        #[cfg(feature = "bar")]
        let bar = BarState::new(&config.bar)?;
        Ok(Self {
            start_time: std::time::Instant::now(),
            socket_name,
            display_handle: dh,
            space: Space::default(),
            windows: Vec::new(),
            outputs: Vec::new(),
            focused_output: None,
            pointer_operation: None,
            loop_signal: event_loop.get_signal(),
            config,
            #[cfg(feature = "anvilctl")]
            config_path,
            #[cfg(feature = "anvilctl")]
            control_socket_path: None,
            repaint_requested: true,
            children: Vec::new(),
            #[cfg(feature = "bar")]
            bar,
            #[cfg(feature = "launcher")]
            launcher: LauncherState::new(),
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            popups: PopupManager::default(),
            seat,
        })
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

    pub fn spawn(&mut self, command: &str) {
        // Use a shell because configuration commands commonly include arguments, quoting or
        // pipelines. Children inherit WAYLAND_DISPLAY, so Wayland applications connect to Anvil.
        match Command::new("/bin/sh").arg("-c").arg(command).spawn() {
            Ok(child) => self.children.push(child),
            Err(error) => tracing::error!(%error, %command, "failed to start command"),
        }
    }

    #[cfg(any(feature = "anvilctl", feature = "launcher"))]
    /// Starts an IPC-requested process without involving a shell or reinterpreting its arguments.
    pub fn spawn_argv(&mut self, argv: &[String]) -> anyhow::Result<u32> {
        let (program, arguments) = argv
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("spawn requires a program"))?;
        let child = Command::new(program).args(arguments).spawn()?;
        let pid = child.id();
        self.children.push(child);
        Ok(pid)
    }

    #[cfg(feature = "launcher")]
    pub fn open_launcher(&mut self) {
        self.launcher.open();
        self.request_repaint();
    }

    #[cfg(feature = "launcher")]
    pub fn close_launcher(&mut self) {
        self.launcher.close();
        self.request_repaint();
    }

    #[cfg(feature = "launcher")]
    pub fn launcher_insert(&mut self, character: char) {
        self.launcher.insert(character);
        self.request_repaint();
    }

    #[cfg(feature = "launcher")]
    pub fn launcher_backspace(&mut self) {
        self.launcher.backspace();
        self.request_repaint();
    }

    #[cfg(feature = "launcher")]
    pub fn launcher_select(&mut self, delta: isize) {
        self.launcher.select_relative(delta);
        self.request_repaint();
    }

    #[cfg(feature = "launcher")]
    pub fn launcher_accept(&mut self) {
        let Some(LaunchCommand { mut argv, terminal }) = self.launcher.accept() else {
            return;
        };
        if terminal {
            let Some(mut terminal_argv) = shlex::split(&self.config.general.terminal) else {
                tracing::error!("cannot parse terminal command for launcher");
                return;
            };
            terminal_argv.push("-e".into());
            terminal_argv.append(&mut argv);
            argv = terminal_argv;
        }
        if let Err(error) = self.spawn_argv(&argv) {
            tracing::error!(%error, ?argv, "launcher failed to start application");
        }
        self.request_repaint();
    }

    #[cfg(feature = "launcher")]
    pub fn launcher_snapshot(&self) -> Option<LauncherSnapshot> {
        self.launcher.snapshot()
    }

    #[cfg(feature = "anvilctl")]
    /// Returns a stable, script-friendly snapshot without exposing Smithay implementation types.
    pub fn control_window_list(&self) -> Vec<WindowInfo> {
        let focused = self.seat.get_keyboard().unwrap().current_focus();
        self.windows
            .iter()
            .enumerate()
            .map(|(index, managed)| {
                let (title, app_id) = managed.window.toplevel().map_or_else(
                    || (None, None),
                    |toplevel| {
                        with_states(toplevel.wl_surface(), |states| {
                            let attributes = states
                                .data_map
                                .get::<XdgToplevelSurfaceData>()
                                .expect("xdg toplevel role data missing")
                                .lock()
                                .unwrap();
                            (attributes.title.clone(), attributes.app_id.clone())
                        })
                    },
                );
                WindowInfo {
                    index,
                    title: title.unwrap_or_else(|| "untitled".into()),
                    app_id: app_id.unwrap_or_default(),
                    tags: (0..self.config.general.tags)
                        .filter_map(|tag| (managed.tags & (1_u16 << tag) != 0).then_some(tag + 1))
                        .collect(),
                    focused: focused.as_ref().is_some_and(|surface| {
                        managed
                            .window
                            .toplevel()
                            .is_some_and(|toplevel| toplevel.wl_surface() == surface)
                    }),
                    floating: self
                        .outputs
                        .iter()
                        .find(|output| output.name == managed.output)
                        .is_some_and(|output| output.layout_mode == LayoutMode::Floating)
                        || managed.rule_floating,
                }
            })
            .collect()
    }

    #[cfg(feature = "anvilctl")]
    /// Atomically validates and installs the startup-selected configuration.
    pub fn reload_config(&mut self) -> anyhow::Result<Option<PathBuf>> {
        let (config, loaded_path) = Config::load(self.config_path.as_deref())?;
        #[cfg(feature = "bar")]
        if config.bar.font != self.config.bar.font {
            anyhow::bail!("changing bar.font requires restarting Anvil");
        }
        self.config = config;
        self.config_path = loaded_path.clone();
        for output in &mut self.outputs {
            if output.selected_tags.trailing_zeros() as usize >= self.config.general.tags {
                output.selected_tags = 1;
            }
        }
        // Metadata-driven floating rules must be recalculated as part of the same reload rather
        // than waiting for an application to happen to change its title later.
        let metadata = self
            .windows
            .iter()
            .filter_map(|managed| managed.window.toplevel())
            .map(|toplevel| {
                let surface = toplevel.wl_surface().clone();
                let (app_id, title) = with_states(&surface, |states| {
                    let attributes = states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .expect("xdg toplevel role data missing")
                        .lock()
                        .unwrap();
                    (attributes.app_id.clone(), attributes.title.clone())
                });
                (surface, app_id, title, toplevel.parent().is_some())
            })
            .collect::<Vec<_>>();
        for (surface, app_id, title, is_dialog) in metadata {
            self.refresh_window_rule(&surface, app_id.as_deref(), title.as_deref(), is_dialog);
        }
        self.arrange();
        Ok(loaded_path)
    }

    /// Reaps completed compositor-launched commands without blocking the event loop.
    pub fn reap_children(&mut self) {
        self.children.retain_mut(|child| match child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(error) => {
                tracing::warn!(pid = child.id(), %error, "cannot query child process");
                false
            }
        });
    }

    pub fn request_repaint(&mut self) {
        self.repaint_requested = true;
    }

    /// Registers or resizes one logical output. Backends choose the global position; keeping that
    /// choice here makes input, layout, popups, bars and launcher placement share one coordinate
    /// system. Existing tag/layout state survives a mode change.
    pub fn configure_output(&mut self, name: &str, area: Rect) {
        let first_output = self.outputs.is_empty();
        #[cfg(feature = "bar")]
        let bar_height = self
            .config
            .bar
            .height
            .min(area.height.saturating_sub(1))
            .max(0);
        #[cfg(not(feature = "bar"))]
        let bar_height = 0;
        let usable = Rect::new(
            area.x,
            area.y + bar_height,
            area.width,
            (area.height - bar_height).max(1),
        );
        if let Some(output) = self.outputs.iter_mut().find(|output| output.name == name) {
            output.screen_area = area;
            output.output_area = usable;
        } else {
            self.outputs.push(OutputWorkspace {
                name: name.to_owned(),
                screen_area: area,
                output_area: usable,
                selected_tags: 1,
                layout_mode: LayoutMode::default(),
            });
        }
        self.outputs.sort_by_key(|output| output.screen_area.x);
        if self.focused_output.is_none() {
            self.focused_output = Some(name.to_owned());
        }
        if first_output {
            // Windows survive a period with zero connected monitors. Once a display returns,
            // attach every orphan to it instead of requiring the client to recreate its surface.
            for managed in &mut self.windows {
                managed.output = name.to_owned();
                managed.floating_geometry = None;
            }
        }
        self.arrange();
    }

    /// Removes an unplugged output and migrates its windows to the nearest surviving output. This
    /// guarantees that hot-unplug never strands a live client outside the visible desktop.
    pub fn remove_output(&mut self, name: &str) {
        self.outputs.retain(|output| output.name != name);
        let fallback = self.outputs.first().map(|output| output.name.clone());
        if let Some(fallback) = fallback {
            for managed in &mut self.windows {
                if managed.output == name {
                    managed.output.clone_from(&fallback);
                    managed.floating_geometry = None;
                }
            }
            if self.focused_output.as_deref() == Some(name) {
                self.focused_output = Some(fallback);
            }
        } else {
            self.focused_output = None;
        }
        self.arrange();
    }

    pub fn desktop_bounds(&self) -> Rect {
        let right = self
            .outputs
            .iter()
            .map(|output| output.screen_area.x + output.screen_area.width)
            .max()
            .unwrap_or(1);
        let bottom = self
            .outputs
            .iter()
            .map(|output| output.screen_area.y + output.screen_area.height)
            .max()
            .unwrap_or(1);
        Rect::new(0, 0, right.max(1), bottom.max(1))
    }

    pub fn focus_output_at(&mut self, point: Point<f64, Logical>) {
        if let Some(output) = self.outputs.iter().find(|output| {
            let area = output.screen_area;
            point.x >= f64::from(area.x)
                && point.x < f64::from(area.x + area.width)
                && point.y >= f64::from(area.y)
                && point.y < f64::from(area.y + area.height)
        }) {
            self.focused_output = Some(output.name.clone());
        }
    }

    fn active_output(&self) -> Option<&OutputWorkspace> {
        let name = self.focused_output.as_deref()?;
        self.outputs.iter().find(|output| output.name == name)
    }

    pub fn add_window(&mut self, window: Window) {
        // New windows inherit the active tag, exactly like dwm. Insertion order is layout order;
        // keeping that rule explicit makes “swap master” a simple vector swap.
        let Some(output) = self.active_output() else {
            tracing::warn!("ignoring new window until an output is connected");
            return;
        };
        let output_name = output.name.clone();
        let selected_tags = output.selected_tags;
        self.windows.push(ManagedWindow {
            window,
            output: output_name.clone(),
            tags: selected_tags,
            rule_floating: false,
            floating_geometry: None,
        });
        self.arrange();
        self.focus_index(
            self.visible_indices_for(&output_name)
                .len()
                .saturating_sub(1),
        );
    }

    /// Removes a destroyed toplevel and immediately closes the hole it occupied in the layout.
    ///
    /// A Wayland object disappearing does not itself tell `Space` to retile the surviving
    /// windows. Keeping this transition explicit is what gives Anvil dwm's dynamic behavior: when
    /// the master closes, the first stack entry becomes index zero and therefore receives the
    /// master rectangle during `arrange`.
    pub fn remove_window(&mut self, surface: &WlSurface) {
        let Some(removed_index) = self.windows.iter().position(|managed| {
            managed
                .window
                .toplevel()
                .is_some_and(|toplevel| toplevel.wl_surface() == surface)
        }) else {
            return;
        };

        // Remember the window's position among visible clients before removing it. If it owned
        // keyboard focus, the client that slides into this position is the least surprising focus
        // successor; when the last stack client closes, clamping selects its predecessor.
        let removed_output = self.windows[removed_index].output.clone();
        let removed_visible_index = self
            .visible_indices_for(&removed_output)
            .iter()
            .position(|&index| index == removed_index);
        let removed = self.windows.remove(removed_index);
        self.space.unmap_elem(&removed.window);
        self.arrange();

        // Preserve an unaffected focus. A destroyed focused surface, however, no longer resolves
        // to a managed window, so move focus to the promoted/succeeding tile and move the border
        // with it. An empty tag must explicitly clear the seat's stale surface reference.
        if self.focused_window_geometry().is_none() {
            let remaining = self.visible_indices_for(&removed_output).len();
            match successor_focus_index(removed_visible_index.unwrap_or(0), remaining) {
                Some(index) => self.focus_index(index),
                None => self.seat.get_keyboard().unwrap().set_focus(
                    self,
                    Option::<WlSurface>::None,
                    SERIAL_COUNTER.next_serial(),
                ),
            }
        }
    }

    pub fn arrange(&mut self) {
        self.request_repaint();
        // Smithay resources may die asynchronously after a client disconnects. Prune dead handles
        // before computing geometry so closed windows never reserve a tile.
        self.windows.retain(|managed| managed.window.alive());
        // `Space` contains only visible windows. Unmapping everything first also removes windows
        // from previous tags/outputs; the persistent vector still retains their ownership.
        for managed in &self.windows {
            self.space.unmap_elem(&managed.window);
        }
        let mut placements = Vec::new();
        // Clone the compact output descriptors so window geometry may be updated without aliasing
        // the immutable output borrow. Connector counts are tiny, so this is not a hot-path cost.
        for output in self.outputs.clone() {
            let visible = self.visible_indices_for(&output.name);
            let tiled: Vec<usize> = visible
                .iter()
                .copied()
                .filter(|&index| !self.windows[index].rule_floating)
                .collect();
            let tiled_geometries = tile(output.output_area, tiled.len(), &self.config.layout);
            for &index in &visible {
                if let Some(current) = self.windows[index].floating_geometry {
                    self.windows[index].floating_geometry = Some(clamp_floating_geometry(
                        output.output_area,
                        current,
                        self.config.layout.outer_gap,
                    ));
                }
            }
            match output.layout_mode {
                LayoutMode::Tiling => {
                    placements.extend(tiled.into_iter().zip(tiled_geometries));
                    placements.extend(
                        visible
                            .iter()
                            .copied()
                            .filter(|&index| self.windows[index].rule_floating)
                            .map(|index| {
                                (
                                    index,
                                    self.windows[index]
                                        .floating_geometry
                                        .expect("rule-floating window has no geometry"),
                                )
                            }),
                    );
                }
                LayoutMode::Fullscreen => placements.extend(
                    visible
                        .iter()
                        .copied()
                        .map(|index| (index, output.output_area)),
                ),
                LayoutMode::Floating => placements.extend(visible.iter().copied().enumerate().map(
                    |(position, index)| {
                        let geometry = self.windows[index].floating_geometry.unwrap_or_else(|| {
                            let mut geometry = centered_floating_geometry(
                                output.output_area,
                                self.config.floating.default_width,
                                self.config.floating.default_height,
                                self.config.layout.outer_gap,
                            );
                            let offset = (position as i32 * 24).min(120);
                            geometry.x += offset;
                            geometry.y += offset;
                            clamp_floating_geometry(
                                output.output_area,
                                geometry,
                                self.config.layout.outer_gap,
                            )
                        });
                        (index, geometry)
                    },
                )),
            }
        }
        for (index, geometry) in placements {
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
        // Monocle windows overlap exactly. Restore the focused surface to the top after every
        // arrange (including output resize), otherwise vector order rather than user focus would
        // decide which full-size client is visible.
        if let Some(focused) = self.seat.get_keyboard().unwrap().current_focus() {
            if let Some(window) = self.windows.iter().find_map(|managed| {
                managed
                    .window
                    .toplevel()
                    .filter(|toplevel| toplevel.wl_surface() == &focused)
                    .map(|_| managed.window.clone())
            }) {
                self.space.raise_element(&window, true);
            }
        }
    }

    /// Cycles the three layout policies while retaining the currently focused window.
    pub fn cycle_layout_mode(&mut self) {
        let Some(output_name) = self.focused_output.clone() else {
            return;
        };
        let Some(output) = self
            .outputs
            .iter_mut()
            .find(|output| output.name == output_name)
        else {
            return;
        };
        output.layout_mode = output.layout_mode.next();
        let mode = output.layout_mode;
        let area = output.output_area;
        if mode == LayoutMode::Floating {
            let outer_gap = self.config.layout.outer_gap;
            for (position, index) in self
                .visible_indices_for(&output_name)
                .into_iter()
                .enumerate()
            {
                if self.windows[index].floating_geometry.is_none() {
                    let mut geometry = centered_floating_geometry(
                        area,
                        self.config.floating.default_width,
                        self.config.floating.default_height,
                        outer_gap,
                    );
                    let offset = (position as i32 * 24).min(120);
                    geometry.x += offset;
                    geometry.y += offset;
                    self.windows[index].floating_geometry =
                        Some(clamp_floating_geometry(area, geometry, outer_gap));
                }
            }
        }
        self.arrange();
    }

    /// Starts a compositor move/resize gesture for the window underneath `location`.
    ///
    /// A true result tells input dispatch to consume the initiating button. Otherwise the client
    /// would interpret the same drag as text selection or a widget click at the same time.
    pub fn begin_pointer_operation(
        &mut self,
        location: Point<f64, Logical>,
        button: u32,
        kind: PointerOperationKind,
    ) -> bool {
        let Some(window) = self
            .space
            .element_under(location)
            .map(|(window, _)| window.clone())
        else {
            return false;
        };
        let Some(index) = self
            .windows
            .iter()
            .position(|managed| managed.window == window)
        else {
            return false;
        };
        let output_name = self.windows[index].output.clone();
        let Some(output) = self
            .outputs
            .iter()
            .find(|output| output.name == output_name)
        else {
            return false;
        };
        if output.layout_mode != LayoutMode::Floating && !self.windows[index].rule_floating {
            return false;
        }
        let Some(current) = self.space.element_geometry(&window) else {
            return false;
        };
        let geometry = Rect::new(current.loc.x, current.loc.y, current.size.w, current.size.h);
        self.windows[index].floating_geometry = Some(geometry);
        if let Some(visible_index) = self
            .visible_indices_for(&output_name)
            .iter()
            .position(|&candidate| candidate == index)
        {
            self.focus_index(visible_index);
        }
        self.pointer_operation = Some(PointerOperation {
            window,
            button,
            start_pointer: location,
            start_geometry: geometry,
            kind,
        });
        true
    }

    /// Applies one pointer sample to the active operation and renegotiates the client's size.
    pub fn update_pointer_operation(&mut self, location: Point<f64, Logical>) -> bool {
        let Some(operation) = self.pointer_operation.clone() else {
            return false;
        };
        let Some(index) = self
            .windows
            .iter()
            .position(|managed| managed.window == operation.window)
        else {
            self.pointer_operation = None;
            return false;
        };
        let dx = (location.x - operation.start_pointer.x).round() as i32;
        let dy = (location.y - operation.start_pointer.y).round() as i32;
        let mut geometry = operation.start_geometry;
        match operation.kind {
            PointerOperationKind::Move => {
                geometry.x += dx;
                geometry.y += dy;
            }
            PointerOperationKind::ResizeHorizontal => geometry.width += dx,
            PointerOperationKind::ResizeVertical => geometry.height += dy,
            PointerOperationKind::ResizeBoth => {
                geometry.width += dx;
                geometry.height += dy;
            }
        }
        let Some(area) = self
            .outputs
            .iter()
            .find(|output| output.name == self.windows[index].output)
            .map(|output| output.output_area)
        else {
            return false;
        };
        self.windows[index].floating_geometry = Some(clamp_floating_geometry(
            area,
            geometry,
            self.config.layout.outer_gap,
        ));
        self.arrange();
        true
    }

    /// Ends only the gesture owned by this physical button.
    pub fn finish_pointer_operation(&mut self, button: u32) -> bool {
        if self
            .pointer_operation
            .as_ref()
            .is_some_and(|operation| operation.button == button)
        {
            self.pointer_operation = None;
            true
        } else {
            false
        }
    }

    /// Re-evaluates a toplevel after app-id, title or parent metadata changes.
    ///
    /// Clients are allowed to publish these properties after constructing the xdg object. Doing
    /// this in all metadata callbacks prevents a startup race where a Steam utility window is
    /// tiled permanently merely because its title arrived one request later than `get_toplevel`.
    pub fn refresh_window_rule(
        &mut self,
        surface: &WlSurface,
        app_id: Option<&str>,
        title: Option<&str>,
        is_dialog: bool,
    ) {
        let should_float = self.config.window_should_float(app_id, title, is_dialog);
        let default_width = self.config.floating.default_width;
        let default_height = self.config.floating.default_height;
        let outer_gap = self.config.layout.outer_gap;
        let Some(managed) = self.windows.iter_mut().find(|managed| {
            managed
                .window
                .toplevel()
                .is_some_and(|toplevel| toplevel.wl_surface() == surface)
        }) else {
            return;
        };

        let area = self
            .outputs
            .iter()
            .find(|output| output.name == managed.output)
            .map(|output| output.output_area)
            .unwrap_or_default();
        let was_floating = managed.rule_floating;
        managed.rule_floating = should_float;
        if should_float && managed.floating_geometry.is_none() {
            managed.floating_geometry = Some(centered_floating_geometry(
                area,
                default_width,
                default_height,
                outer_gap,
            ));
        }
        if was_floating != should_float {
            self.arrange();
        }
    }

    pub fn focus_index(&mut self, visible_index: usize) {
        let Some(output_name) = self.focused_output.clone() else {
            return;
        };
        let visible = self.visible_indices_for(&output_name);
        let Some(&index) = visible.get(visible_index) else {
            return;
        };
        // Focus changes affect the border and bar even if neither client commits a new buffer.
        self.request_repaint();
        // Wayland uses monotonically increasing serials to order focus and input transitions.
        let serial = SERIAL_COUNTER.next_serial();
        let target = self.windows[index].window.clone();
        // Activation is separate from keyboard focus. `set_activated` reports whether it actually
        // changed the pending xdg state; only then send a configure. Previously every focus step
        // configured every open client, causing all terminals to wake up and redraw even though
        // only the old and new focus windows can possibly change appearance.
        for managed in &self.windows {
            let changed = managed.window.set_activated(managed.window == target);
            if changed {
                if let Some(toplevel) = managed.window.toplevel() {
                    toplevel.send_pending_configure();
                }
            }
        }
        // Changing Space's stacking order can invalidate every overlapping surface in Smithay's
        // damage tracker. Tiled clients never overlap, so raising one on every keyboard repeat did
        // expensive full-window composition work without changing a single visible pixel. Only
        // monocle/global-floating layouts and rule-floating clients actually need the focused
        // element brought to the front.
        let layout_mode = self
            .outputs
            .iter()
            .find(|output| output.name == output_name)
            .map_or(LayoutMode::Tiling, |output| output.layout_mode);
        if layout_mode != LayoutMode::Tiling || self.windows[index].rule_floating {
            self.space.raise_element(&target, true);
        }
        self.seat.get_keyboard().unwrap().set_focus(
            self,
            target
                .toplevel()
                .map(|surface| surface.wl_surface().clone()),
            serial,
        );
    }

    pub fn focus_relative(&mut self, delta: isize) {
        let Some(output_name) = self.focused_output.clone() else {
            return;
        };
        let visible = self.visible_indices_for(&output_name);
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
        let Some(output_name) = self.focused_output.clone() else {
            return;
        };
        let visible = self.visible_indices_for(&output_name);
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
        if let Some(output) = self
            .outputs
            .iter_mut()
            .find(|output| self.focused_output.as_deref() == Some(output.name.as_str()))
        {
            output.selected_tags = 1 << tag;
        }
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

    /// Focuses the adjacent output, wrapping at either desktop edge.
    pub fn focus_output_relative(&mut self, delta: isize) {
        if self.outputs.is_empty() {
            return;
        }
        let current = self
            .focused_output
            .as_deref()
            .and_then(|name| self.outputs.iter().position(|output| output.name == name))
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(self.outputs.len() as isize) as usize;
        self.focused_output = Some(self.outputs[next].name.clone());
        self.focus_index(0);
        self.request_repaint();
    }

    /// Moves the focused client to the adjacent output while preserving its tag number when
    /// possible. Floating geometry is rebuilt for the destination so differing resolutions cannot
    /// leave it off-screen.
    pub fn move_focused_to_output(&mut self, delta: isize) {
        if self.outputs.len() < 2 {
            return;
        }
        let Some(source) = self.focused_output.clone() else {
            return;
        };
        let current = self
            .outputs
            .iter()
            .position(|output| output.name == source)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(self.outputs.len() as isize) as usize;
        let destination = self.outputs[next].name.clone();
        let destination_tags = self.outputs[next].selected_tags;
        let focused = self.seat.get_keyboard().unwrap().current_focus();
        let moved_window = self
            .windows
            .iter_mut()
            .find(|managed| {
                focused.as_ref().is_some_and(|surface| {
                    managed
                        .window
                        .toplevel()
                        .is_some_and(|toplevel| toplevel.wl_surface() == surface)
                })
            })
            .map(|managed| {
                managed.output.clone_from(&destination);
                // Moving between outputs is an explicit visibility action. Assign the destination's
                // currently viewed tag so the window cannot apparently vanish after the transfer.
                managed.tags = destination_tags;
                managed.floating_geometry = None;
                managed.window.clone()
            });
        if let Some(moved_window) = moved_window {
            self.focused_output = Some(destination);
            self.arrange();
            let visible = self.visible_indices();
            let moved = visible
                .iter()
                .position(|&index| self.windows[index].window == moved_window);
            if let Some(index) = moved {
                self.focus_index(index);
            }
        }
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
        // Nested mode has exactly one synthetic output but still exercises the same output-aware
        // state used by DRM. This avoids maintaining a second single-monitor policy path.
        self.configure_output("winit", Rect::new(0, 0, width, height));
    }

    #[cfg(feature = "bar")]
    /// Captures tag occupancy, visible titles, keyboard focus and refreshed shell status.
    pub fn bar_snapshot(&mut self, output_name: &str) -> BarSnapshot {
        self.bar.refresh(&self.config.bar);
        let output = self
            .outputs
            .iter()
            .find(|output| output.name == output_name)
            .cloned()
            .expect("backend requested a bar for an unknown output");
        let focused = self.seat.get_keyboard().unwrap().current_focus();
        let mut window_counts = vec![0; self.config.general.tags];
        // Count membership, not visibility: hidden tags need indicators too. Iterating every tag
        // also preserves dwm's multi-tag semantics, where one window may intentionally contribute
        // one marker to more than one tag.
        for window in self
            .windows
            .iter()
            .filter(|window| window.output == output_name)
        {
            for (tag, count) in window_counts.iter_mut().enumerate() {
                if window.tags & (1_u16 << tag) != 0 {
                    *count += 1;
                }
            }
        }
        let windows = self
            .visible_indices_for(output_name)
            .into_iter()
            .map(|index| {
                let toplevel = self.windows[index].window.toplevel().unwrap();
                let (title, app_id) = with_states(toplevel.wl_surface(), |states| {
                    let attributes = states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .expect("xdg toplevel role data missing")
                        .lock()
                        .unwrap();
                    (attributes.title.clone(), attributes.app_id.clone())
                });
                BarWindow {
                    title: title.or(app_id).unwrap_or_else(|| "untitled".into()),
                    focused: focused
                        .as_ref()
                        .is_some_and(|surface| surface == toplevel.wl_surface()),
                }
            })
            .collect();
        BarSnapshot {
            output_focused: self.focused_output.as_deref() == Some(output_name),
            multiple_outputs: self.outputs.len() > 1,
            selected_tags: output.selected_tags,
            occupied_tags: self
                .windows
                .iter()
                .filter(|window| window.output == output_name)
                .fold(0, |tags, window| tags | window.tags),
            tag_count: self.config.general.tags,
            layout_symbol: output.layout_mode.symbol(),
            window_counts,
            windows,
            status: self.bar.text().to_owned(),
            #[cfg(feature = "launcher")]
            launcher: (self.focused_output.as_deref() == Some(output_name))
                .then(|| self.launcher_snapshot())
                .flatten(),
        }
    }

    pub(crate) fn visible_indices(&self) -> Vec<usize> {
        self.focused_output
            .as_deref()
            .map_or_else(Vec::new, |name| self.visible_indices_for(name))
    }

    fn visible_indices_for(&self, output_name: &str) -> Vec<usize> {
        // A non-zero bit intersection implements dwm-style tag visibility and already supports a
        // future multi-tag view without changing individual window records.
        let selected_tags = self
            .outputs
            .iter()
            .find(|output| output.name == output_name)
            .map_or(0, |output| output.selected_tags);
        self.windows
            .iter()
            .enumerate()
            .filter_map(|(i, w)| {
                (w.output == output_name && w.tags & selected_tags != 0).then_some(i)
            })
            .collect()
    }

    pub fn output_rectangle(&self) -> Rectangle<i32, Logical> {
        // Popup helpers operate on Smithay's typed rectangles, whereas the pure layout module uses
        // its backend-independent Rect. Keep the conversion at this integration boundary.
        let area = self
            .active_output()
            .map_or_else(|| Rect::new(0, 0, 1, 1), |output| output.output_area);
        Rectangle::new((area.x, area.y).into(), (area.width, area.height).into())
    }

    /// Returns one named output's usable rectangle for popup constraints rooted on an output that
    /// is not currently focused.
    pub fn output_rectangle_for(&self, name: &str) -> Rectangle<i32, Logical> {
        let area = self
            .outputs
            .iter()
            .find(|output| output.name == name)
            .map_or_else(|| Rect::new(0, 0, 1, 1), |output| output.output_area);
        Rectangle::new((area.x, area.y).into(), (area.width, area.height).into())
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

/// Creates a centered initial rectangle inside the output's outer-gap safe area.
#[cfg(feature = "anvilctl")]
impl Drop for Anvil {
    fn drop(&mut self) {
        if let Some(path) = self.control_socket_path.take() {
            // Unlinking a bound Unix socket is safe: existing connections remain valid while a
            // later compositor start no longer mistakes a clean shutdown for a stale crash.
            let _ = std::fs::remove_file(path);
        }
    }
}

fn centered_floating_geometry(area: Rect, width: i32, height: i32, outer_gap: i32) -> Rect {
    let gap = outer_gap.max(0);
    let maximum_width = (area.width - gap * 2).max(1);
    let maximum_height = (area.height - gap * 2).max(1);
    let width = width.clamp(1, maximum_width);
    let height = height.clamp(1, maximum_height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// Keeps an existing floating rectangle fully visible without resetting its chosen position.
fn clamp_floating_geometry(area: Rect, geometry: Rect, outer_gap: i32) -> Rect {
    let gap = outer_gap.max(0);
    let width = geometry.width.clamp(1, (area.width - gap * 2).max(1));
    let height = geometry.height.clamp(1, (area.height - gap * 2).max(1));
    let minimum_x = area.x + gap;
    let minimum_y = area.y + gap;
    let maximum_x = (area.x + area.width - gap - width).max(minimum_x);
    let maximum_y = (area.y + area.height - gap - height).max(minimum_y);
    Rect::new(
        geometry.x.clamp(minimum_x, maximum_x),
        geometry.y.clamp(minimum_y, maximum_y),
        width,
        height,
    )
}

/// Chooses the visible client that should receive focus after a removal.
///
/// Removing from a vector shifts the following entry into the removed slot. Clamping only matters
/// when the old last entry disappeared, in which case the previous entry is the natural fallback.
fn successor_focus_index(removed_index: usize, remaining: usize) -> Option<usize> {
    remaining
        .checked_sub(1)
        .map(|last_index| removed_index.min(last_index))
}

#[cfg(test)]
mod tests {
    use super::{centered_floating_geometry, clamp_floating_geometry, successor_focus_index};
    use anvil::layout::Rect;

    #[test]
    fn closing_master_selects_promoted_stack_head() {
        assert_eq!(successor_focus_index(0, 2), Some(0));
    }

    #[test]
    fn floating_geometry_is_centered_and_clamped_to_output() {
        let area = Rect::new(0, 0, 1280, 720);
        assert_eq!(
            centered_floating_geometry(area, 800, 600, 8),
            Rect::new(240, 60, 800, 600)
        );
        assert_eq!(
            centered_floating_geometry(area, 2000, 1000, 8),
            Rect::new(8, 8, 1264, 704)
        );
    }

    #[test]
    fn existing_float_stays_reachable_after_output_shrinks() {
        assert_eq!(
            clamp_floating_geometry(Rect::new(0, 0, 1024, 768), Rect::new(900, 700, 500, 400), 8,),
            Rect::new(516, 360, 500, 400)
        );
    }

    #[test]
    fn closing_last_client_clears_focus() {
        assert_eq!(successor_focus_index(0, 0), None);
    }

    #[test]
    fn closing_stack_tail_selects_its_predecessor() {
        assert_eq!(successor_focus_index(2, 2), Some(1));
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
