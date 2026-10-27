//! Central compositor state and window-management operations.
//!
//! Smithay protocol implementations are callback based. `Anvil` is the single state object shared
//! with those callbacks through Calloop, while this module owns the policy decisions that make the
//! program a window manager: window order, tags, focus, layout and process spawning.

use std::{
    collections::HashSet,
    ffi::OsString,
    path::PathBuf,
    process::{Child, Command},
    sync::Arc,
    time::Instant,
};

use anvil::{
    config::Config,
    layout::{LayoutMode, Rect, tile},
};
#[cfg(any(feature = "anvilctl", feature = "launcher"))]
use smithay::wayland::xdg_activation::XdgActivationToken;
#[cfg(feature = "xwayland")]
use smithay::wayland::xwayland_shell::XWaylandShellState;
#[cfg(feature = "xwayland")]
use smithay::xwayland::X11Wm;
use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState, keyboard::XkbConfig},
    output::Output,
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, generic::Generic,
            ping::Ping,
        },
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{IsAlive, Logical, Point, Rectangle, SERIAL_COUNTER, Serial},
    wayland::{
        compositor::{CompositorClientState, CompositorState, get_parent, with_states},
        dmabuf::DmabufState,
        fractional_scale::{FractionalScaleManagerState, with_fractional_scale},
        idle_inhibit::IdleInhibitManagerState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        relative_pointer::RelativePointerManagerState,
        seat::WaylandFocus,
        selection::data_device::DataDeviceState,
        selection::primary_selection::PrimarySelectionState,
        session_lock::{LockSurface, SessionLockManagerState, SessionLocker},
        shell::xdg::{XdgShellState, decoration::XdgDecorationState},
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
        xdg_activation::XdgActivationState,
    },
};

use crate::CalloopData;
#[cfg(feature = "bar")]
use crate::bar::{BarSnapshot, BarState, BarWindow};
use crate::handlers::idle::IdleNotificationState;
#[cfg(feature = "launcher")]
use crate::launcher::{LaunchCommand, LauncherSnapshot, LauncherState};
use crate::output_navigation::{self, Direction};
#[cfg(feature = "overview")]
use crate::overview::{OverviewSnapshot, OverviewTile};
#[cfg(feature = "anvilctl")]
use anvil::ipc::{OutputInfo, WindowInfo};
#[cfg(any(feature = "bar", feature = "anvilctl", feature = "overview"))]
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

pub(crate) const ACTIVATION_TOKEN_LIFETIME: std::time::Duration =
    std::time::Duration::from_secs(10);

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
    /// Initial placement may be chosen exactly once from delayed app-id/title metadata.
    /// Manual output/tag moves also finalize it; later title changes/reloads cannot relocate.
    pub placement_resolved: bool,
    /// Last independent geometry used by either a rule or the global Floating mode.
    /// Geometry and policy are separate so moving a window does not permanently change its rule.
    pub floating_geometry: Option<Rect>,
}

/// A child is reaped once after pidfd readiness, or via a slow fallback when
/// pidfds are unsupported/unavailable (older kernels and restricted containers).
struct TrackedChild {
    child: Child,
    fallback: bool,
}

/// Per-output desktop policy with independent layout modes for each of the nine possible tags.
/// The small fixed-size array avoids heap allocations, subscriptions and idle-time work.
#[derive(Clone, Debug)]
pub struct OutputWorkspace {
    pub name: String,
    pub screen_area: Rect,
    pub output_area: Rect,
    pub selected_tags: u16,
    /// Indexed by zero-based tag, not by currently focused client or global output.
    pub tag_layouts: [LayoutMode; 9],
}

impl OutputWorkspace {
    /// A multi-tag view takes its effective layout from the lowest selected tag. The same
    /// index is used for changing its layout, leaving all other selected tags untouched.
    fn active_tag(&self) -> usize {
        if self.selected_tags == 0 {
            return 0;
        }
        (self.selected_tags.trailing_zeros() as usize).min(self.tag_layouts.len() - 1)
    }

    fn layout_mode(&self) -> LayoutMode {
        self.tag_layouts[self.active_tag()]
    }

    fn cycle_layout_mode(&mut self) -> LayoutMode {
        let tag = self.active_tag();
        let next = self.tag_layouts[tag].next();
        self.tag_layouts[tag] = next;
        next
    }
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

/// Backend callback used to validate and import a client-provided DMA-BUF.
pub(crate) type DmabufImporter = Box<dyn FnMut(&Dmabuf) -> bool>;

/// One lock-screen surface and the protocol output it exclusively covers.
pub struct SessionLockSurface {
    pub output: Output,
    pub surface: LockSurface,
}

/// Security-sensitive state kept separate from ordinary window-management metadata.
pub struct SessionLockData {
    pub active: bool,
    pub surfaces: Vec<SessionLockSurface>,
    pub confirmation: Option<SessionLocker>,
    pub secured_outputs: HashSet<String>,
    pub previous_keyboard_focus: Option<WlSurface>,
}

impl SessionLockData {
    fn new() -> Self {
        Self {
            active: false,
            surfaces: Vec::new(),
            confirmation: None,
            secured_outputs: HashSet::new(),
            previous_keyboard_focus: None,
        }
    }
}

/// All mutable state required by the compositor and its Wayland protocol delegates.
///
/// Smithay's `*State` fields publish and implement individual protocol globals. They are stored
/// together because the delegate macros below the handler implementations need to retrieve them
/// from the application state whenever a client sends a request.
#[cfg(feature = "anvilctl")]
type ReloadHook = Box<dyn FnMut(&mut Anvil, &Config) -> anyhow::Result<()>>;

pub struct Anvil {
    /// Monotonic origin used for frame callback timestamps.
    pub start_time: std::time::Instant,
    pub(crate) diagnostics: crate::diagnostics::Diagnostics,
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
    /// The output displaying the cross-monitor overview, if enabled.
    #[cfg(feature = "overview")]
    pub overview_output: Option<String>,
    /// Stable selected window identity; closing another window must not shift selection.
    #[cfg(feature = "overview")]
    pub overview_selected: Option<Window>,
    /// Active compositor-owned mouse gesture. Its button is not forwarded to the client.
    pointer_operation: Option<PointerOperation>,
    /// Most recent physical key/button serial. xdg-activation compares client-provided serials
    /// against this value so an unrelated client cannot manufacture authority to steal focus.
    pub(crate) last_user_input: Option<(Serial, Instant)>,
    /// Allows a key binding or backend close event to stop Calloop cleanly.
    pub loop_signal: LoopSignal,
    pub config: Config,
    /// Runtime visibility of the built-in bar; persists across config reload and output hotplug.
    #[cfg(feature = "bar")]
    pub bar_visible: bool,
    /// Exact file selected at startup, reused by `anvilctl reload`.
    #[cfg(feature = "anvilctl")]
    pub config_path: Option<PathBuf>,
    /// Bound control socket removed when the compositor exits normally.
    #[cfg(feature = "anvilctl")]
    pub control_socket_path: Option<PathBuf>,
    /// Backend callback applies output and libinput changes before the new config is installed.
    #[cfg(feature = "anvilctl")]
    pub reload_hook: Option<ReloadHook>,
    /// Whether the direct backend must sample scene state and attempt a new KMS frame.
    ///
    /// Keeping this bit in shared compositor state lets protocol commits and input handlers wake
    /// rendering without forcing the DRM backend to redraw continuously while the desktop is idle.
    pub repaint_requested: bool,
    /// Wake the direct DRM renderer on demand instead of polling a 16-ms timer.
    pub(crate) repaint_ping: Option<Ping>,
    /// Child handles stay owned until kernel pidfd readiness / fallback confirms exit.
    children: Vec<TrackedChild>,
    child_event_loop: LoopHandle<'static, CalloopData>,
    #[cfg(feature = "bar")]
    pub bar: BarState,
    #[cfg(feature = "launcher")]
    pub launcher: LauncherState,
    // Protocol state objects retained for Smithay's generated dispatch implementations.
    pub compositor_state: CompositorState,
    pub(crate) capture_state: crate::handlers::capture::CaptureState,
    #[cfg(feature = "layer-shell")]
    pub layer_shell_state: smithay::wayland::shell::wlr_layer::WlrLayerShellState,
    #[cfg(feature = "layer-shell")]
    pub(crate) exclusive_layer_focus: Option<WlSurface>,
    #[cfg(feature = "layer-shell")]
    pub(crate) layer_surfaces: Vec<(smithay::desktop::LayerSurface, Output)>,
    pub xdg_shell_state: XdgShellState,
    pub activation_state: XdgActivationState,
    pub(crate) idle_notification_state: IdleNotificationState,
    /// One entry per surface, with a count because the protocol permits multiple inhibitors.
    pub(crate) idle_inhibitors: Vec<(WlSurface, usize)>,
    pub relative_pointer_state: RelativePointerManagerState,
    pub pointer_constraints_state: PointerConstraintsState,
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    pub viewporter_state: ViewporterState,
    /// Live surfaces that requested preferred fractional-scale notifications.
    pub(crate) fractional_scale_surfaces: Vec<WlSurface>,
    /// Negotiates client-side versus server-side title bars for xdg toplevels.
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    /// linux-dmabuf protocol bookkeeping and the active backend's renderer import probe.
    pub dmabuf_state: DmabufState,
    pub(crate) dmabuf_importer: Option<DmabufImporter>,
    pub session_lock_state: SessionLockManagerState,
    pub session_lock: SessionLockData,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Anvil>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    #[cfg(feature = "xwayland")]
    pub xwayland_shell_state: XWaylandShellState,
    #[cfg(feature = "xwayland")]
    pub xwm: Option<X11Wm>,
    #[cfg(feature = "xwayland")]
    pub xwayland_display: Option<String>,
    pub popups: PopupManager,
    pub seat: Seat<Self>,
}

impl Anvil {
    /// Builds protocol globals, input capabilities and the Wayland listening socket.
    pub fn new(
        event_loop: &mut EventLoop<'static, CalloopData>,
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
        let capture_state = crate::handlers::capture::CaptureState::new(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        #[cfg(feature = "layer-shell")]
        let layer_shell_state =
            smithay::wayland::shell::wlr_layer::WlrLayerShellState::new::<Self>(&dh);
        // Activation is compositor policy, not automatic focus. The handler validates each token
        // against recent input before it may reveal and focus a requested toplevel.
        let activation_state = XdgActivationState::new::<Self>(&dh);
        let idle_notification_state = IdleNotificationState::new(&dh, event_loop.handle());
        // The display owns the registered global; callbacks use `IdleInhibitHandler` directly, so
        // unlike stateful protocol helpers there is no manager value to retain on `Anvil`.
        IdleInhibitManagerState::new::<Self>(&dh);
        // Games and remote-desktop clients need raw deltas and may request that the compositor
        // keeps their pointer inside a surface.  These globals only add behavior for clients that
        // explicitly bind them; ordinary desktop pointer handling remains unchanged.
        let relative_pointer_state = RelativePointerManagerState::new::<Self>(&dh);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&dh);
        // Fractional-scale tells clients how densely to render; viewporter lets them submit a
        // correspondingly sized buffer while preserving logical surface geometry.
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        // Advertising xdg-decoration lets cooperating clients omit their own title bars. We select
        // server-side mode by default but deliberately draw no server frame, yielding undecorated
        // tiled windows without relying on toolkit-specific environment variables.
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        // The active graphics backend creates the global only after it knows the exact EGL format
        // and modifier set. Protocol state stays here so requests share Anvil's central dispatch.
        let dmabuf_state = DmabufState::new();
        // Any client may request the standard lock protocol. The handler still serializes requests
        // so only one client can own the secure display state at a time.
        let session_lock_state = SessionLockManagerState::new::<Self, _>(&dh, |_| true);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        #[cfg(feature = "xwayland")]
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);
        // A Wayland seat groups related input devices. The nested backend always exposes one
        // keyboard and pointer, so declaring both once is more honest than hot-plug bookkeeping.
        let mut seat = seat_state.new_wl_seat(&dh, "seat-0");
        let keyboard = &config.input.keyboard;
        seat.add_keyboard(
            XkbConfig {
                layout: &keyboard.layout,
                variant: &keyboard.variant,
                ..Default::default()
            },
            keyboard.repeat_delay,
            keyboard.repeat_rate,
        )
        .expect("keyboard initialization failed");
        seat.add_pointer();

        let socket_name = Self::init_wayland_listener(display, event_loop);
        #[cfg(feature = "bar")]
        let bar = BarState::new(&config.bar)?;
        Ok(Self {
            start_time: std::time::Instant::now(),
            diagnostics: crate::diagnostics::Diagnostics::default(),
            socket_name,
            display_handle: dh,
            space: Space::default(),
            windows: Vec::new(),
            outputs: Vec::new(),
            focused_output: None,
            #[cfg(feature = "overview")]
            overview_output: None,
            #[cfg(feature = "overview")]
            overview_selected: None,
            pointer_operation: None,
            last_user_input: None,
            loop_signal: event_loop.get_signal(),
            config,
            #[cfg(feature = "bar")]
            bar_visible: true,
            #[cfg(feature = "anvilctl")]
            config_path,
            #[cfg(feature = "anvilctl")]
            control_socket_path: None,
            #[cfg(feature = "anvilctl")]
            reload_hook: None,
            repaint_requested: true,
            repaint_ping: None,
            children: Vec::new(),
            child_event_loop: event_loop.handle(),
            #[cfg(feature = "bar")]
            bar,
            #[cfg(feature = "launcher")]
            launcher: LauncherState::new(),
            compositor_state,
            capture_state,
            #[cfg(feature = "layer-shell")]
            layer_shell_state,
            #[cfg(feature = "layer-shell")]
            exclusive_layer_focus: None,
            #[cfg(feature = "layer-shell")]
            layer_surfaces: Vec::new(),
            xdg_shell_state,
            activation_state,
            idle_notification_state,
            idle_inhibitors: Vec::new(),
            relative_pointer_state,
            pointer_constraints_state,
            fractional_scale_manager_state,
            viewporter_state,
            fractional_scale_surfaces: Vec::new(),
            xdg_decoration_state,
            shm_state,
            dmabuf_state,
            dmabuf_importer: None,
            session_lock_state,
            session_lock: SessionLockData::new(),
            output_manager_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            #[cfg(feature = "xwayland")]
            xwayland_shell_state,
            #[cfg(feature = "xwayland")]
            xwm: None,
            #[cfg(feature = "xwayland")]
            xwayland_display: None,
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
        let mut child = Command::new("/bin/sh");
        child.arg("-c").arg(command);
        #[cfg(feature = "xwayland")]
        if let Some(display) = &self.xwayland_display {
            child.env("DISPLAY", display);
        }
        match child.spawn() {
            Ok(child) => self.track_child(child),
            Err(error) => tracing::error!(%error, %command, "failed to start command"),
        }
    }

    #[cfg(any(feature = "anvilctl", feature = "launcher"))]
    /// Starts an IPC-requested process without involving a shell or reinterpreting its arguments.
    pub fn spawn_argv(&mut self, argv: &[String]) -> anyhow::Result<u32> {
        self.spawn_argv_with_activation(argv, None)
    }

    #[cfg(any(feature = "anvilctl", feature = "launcher"))]
    fn spawn_argv_with_activation(
        &mut self,
        argv: &[String],
        activation_token: Option<&XdgActivationToken>,
    ) -> anyhow::Result<u32> {
        let (program, arguments) = argv
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("spawn requires a program"))?;
        let mut command = Command::new(program);
        command.args(arguments);
        #[cfg(feature = "xwayland")]
        if let Some(display) = &self.xwayland_display {
            command.env("DISPLAY", display);
        }
        if let Some(token) = activation_token {
            // Toolkits consume this standard environment variable when their first toplevel is
            // created and present the token back through xdg-activation.
            command.env("XDG_ACTIVATION_TOKEN", token.as_str());
        }
        let child = command.spawn()?;
        let pid = child.id();
        self.track_child(child);
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
        // A launcher selection is itself a trusted compositor action. Its external token has no
        // client serial, distinguishing it from client-created tokens validated by the handler.
        self.activation_state
            .retain_tokens(|_, token| token.timestamp.elapsed() <= ACTIVATION_TOKEN_LIFETIME);
        let token = self.activation_state.create_external_token(None).0.clone();
        if let Err(error) = self.spawn_argv_with_activation(&argv, Some(&token)) {
            self.activation_state.remove_token(&token);
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
                let (title, app_id, _) = window_metadata(&managed.window);
                WindowInfo {
                    index,
                    title: title.unwrap_or_else(|| "untitled".into()),
                    app_id: app_id.unwrap_or_default(),
                    tags: (0..self.config.general.tags)
                        .filter_map(|tag| (managed.tags & (1_u16 << tag) != 0).then_some(tag + 1))
                        .collect(),
                    focused: focused.as_ref().is_some_and(|surface| {
                        managed.window.wl_surface().as_deref() == Some(surface)
                    }),
                    floating: self
                        .outputs
                        .iter()
                        .find(|output| output.name == managed.output)
                        .is_some_and(|output| output.layout_mode() == LayoutMode::Floating)
                        || managed.rule_floating,
                }
            })
            .collect()
    }

    #[cfg(feature = "anvilctl")]
    pub fn control_output_list(&self) -> Vec<OutputInfo> {
        fn mode_string(mode: smithay::output::Mode) -> String {
            format!(
                "{}x{}@{:.3}",
                mode.size.w,
                mode.size.h,
                f64::from(mode.refresh) / 1_000.0
            )
        }
        self.space
            .outputs()
            .map(|output| {
                let name = output.name();
                let workspace = self.outputs.iter().find(|entry| entry.name == name);
                let position = workspace
                    .map(|entry| [entry.screen_area.x, entry.screen_area.y])
                    .unwrap_or([0, 0]);
                let transform = match output.current_transform() {
                    smithay::utils::Transform::Normal => "normal",
                    smithay::utils::Transform::_90 => "90",
                    smithay::utils::Transform::_180 => "180",
                    smithay::utils::Transform::_270 => "270",
                    smithay::utils::Transform::Flipped => "flipped",
                    smithay::utils::Transform::Flipped90 => "flipped-90",
                    smithay::utils::Transform::Flipped180 => "flipped-180",
                    smithay::utils::Transform::Flipped270 => "flipped-270",
                };
                OutputInfo {
                    name,
                    current_mode: output.current_mode().map(mode_string),
                    preferred_mode: output.preferred_mode().map(mode_string),
                    position,
                    scale: output.current_scale().fractional_scale(),
                    transform: transform.into(),
                }
            })
            .collect()
    }

    #[cfg(feature = "anvilctl")]
    /// Atomically validates and installs the startup-selected configuration.
    pub fn reload_config(&mut self) -> anyhow::Result<Option<PathBuf>> {
        let (config, loaded_path) = Config::load(self.config_path.as_deref())?;
        validate_reload_transition(&self.config, &config)?;
        // Apply backend changes while the previous configuration is still active. A failed
        // modeset or validation keeps the previous active config, and attempts to restore any
        // already-modified outputs before returning an actionable error to anvilctl.
        if let Some(mut hook) = self.reload_hook.take() {
            if let Err(error) = hook(self, &config) {
                let previous = self.config.clone();
                let rollback = hook(self, &previous);
                self.reload_hook = Some(hook);
                if let Err(rollback_error) = rollback {
                    anyhow::bail!(
                        "reload failed: {error:#}; additionally restoring backend state failed: {rollback_error:#}"
                    );
                }
                anyhow::bail!("reload failed; previous configuration retained: {error:#}");
            }
            self.reload_hook = Some(hook);
        }
        #[cfg(feature = "bar")]
        let previous_bar = self.config.bar.clone();
        self.config = config;
        self.config_path = loaded_path.clone();
        #[cfg(feature = "bar")]
        if previous_bar.status_commands != self.config.bar.status_commands
            || previous_bar.blocks != self.config.bar.blocks
            || previous_bar.refresh_interval_ms != self.config.bar.refresh_interval_ms
        {
            self.bar.jobs.reconfigure(&self.config.bar);
            self.bar.jobs.refresh();
        }
        // Updating the bar's reserved height requires rebuilding each logical work area.
        // Merely rearranging windows keeps the old output_area and places clients under the bar.
        #[cfg(feature = "bar")]
        if previous_bar.height != self.config.bar.height {
            let areas = self
                .outputs
                .iter()
                .map(|output| (output.name.clone(), output.screen_area))
                .collect::<Vec<_>>();
            for (name, area) in areas {
                self.configure_output(&name, area);
            }
        }
        // A smaller tag count must not leave the visible mask pointing only to deleted
        // tags. Keep the fixed per-tag layouts untouched so a later increase restores them.
        let valid_tags = (1_u16 << self.config.general.tags) - 1;
        for output in &mut self.outputs {
            output.selected_tags &= valid_tags;
            if output.selected_tags == 0 {
                output.selected_tags = 1;
            }
        }
        // Metadata-driven floating rules must be recalculated as part of the same reload rather
        // than waiting for an application to happen to change its title later.
        let metadata = self
            .windows
            .iter()
            .filter_map(|managed| {
                let surface = managed.window.wl_surface()?.into_owned();
                let (title, app_id, is_dialog) = window_metadata(&managed.window);
                Some((surface, app_id, title, is_dialog))
            })
            .collect::<Vec<_>>();
        for (surface, app_id, title, is_dialog) in metadata {
            self.refresh_window_rule(&surface, app_id.as_deref(), title.as_deref(), is_dialog);
        }
        self.arrange();
        self.request_repaint();
        Ok(loaded_path)
    }

    /// Start monitoring a newly launched child using its kernel pidfd. Registering
    /// before the handle is stored is safe: Calloop only dispatches after control returns.
    fn track_child(&mut self, child: Child) {
        let fallback = !crate::child_lifecycle::watch_child(&child, &self.child_event_loop);
        self.children.push(TrackedChild { child, fallback });
    }

    /// Called *only* for a readable pidfd. No polling of healthy long-lived apps.
    pub(crate) fn child_exited(&mut self, pid: u32) {
        let Some(index) = self
            .children
            .iter()
            .position(|entry| entry.child.id() == pid)
        else {
            return;
        };
        match self.children[index].child.try_wait() {
            Ok(Some(status)) => {
                tracing::debug!(pid, ?status, "child exited and was reaped");
                self.children.swap_remove(index);
            }
            Ok(None) => {
                // A spurious readable fd or an interrupted exit: bound future checks
                // rather than repeatedly receiving readiness notifications.
                self.children[index].fallback = true;
            }
            Err(error) => {
                tracing::warn!(pid, %error, "failed to reap pidfd child");
                self.children.swap_remove(index);
            }
        }
    }

    /// Compatibility fallback for a kernel without pidfd_open or registration failure.
    /// Called only once per second, never at every event-loop iteration.
    pub(crate) fn reap_fallback_children(&mut self) {
        self.children.retain_mut(|entry| {
            if !entry.fallback {
                return true;
            }
            match entry.child.try_wait() {
                Ok(None) => true,
                Ok(Some(_)) => false,
                Err(error) => {
                    tracing::warn!(pid = entry.child.id(), %error, "failed to reap fallback child");
                    false
                }
            }
        });
    }

    /// Coalesce redraws and wake the direct backend on the first pending request.
    pub fn request_repaint(&mut self) {
        self.diagnostics.requested_repaints = self.diagnostics.requested_repaints.saturating_add(1);
        if !self.repaint_requested {
            self.repaint_requested = true;
            self.wake_pending_repaint();
        }
    }

    /// Install the wakeup after backend registration, then request the initial frame.
    pub(crate) fn install_repaint_ping(&mut self, ping: Ping) {
        self.repaint_ping = Some(ping);
        self.wake_pending_repaint();
    }

    pub(crate) fn wake_pending_repaint(&self) {
        if self.repaint_requested {
            if let Some(ping) = &self.repaint_ping {
                ping.ping();
            }
        }
    }

    pub(crate) fn cancel_pointer_operation(&mut self) {
        self.pointer_operation = None;
    }

    /// Whether protocol input and rendering must be isolated from the normal desktop.
    pub fn session_locked(&self) -> bool {
        self.session_lock.active
    }

    /// Returns the live lock surface assigned to a backend output, if the lock client created one.
    pub fn session_lock_surface(&self, output: &Output) -> Option<&LockSurface> {
        self.session_lock
            .surfaces
            .iter()
            .find(|entry| entry.output.name() == output.name() && entry.surface.alive())
            .map(|entry| &entry.surface)
    }

    /// Records that a backend submitted a secure frame for this output.
    ///
    /// The protocol's `locked` event is delayed until every currently connected display has shown
    /// either its lock surface or the compositor's black fallback. This closes the interval in
    /// which a locker believes the session is secure while an old desktop frame is still scanned
    /// out. Hot-unplug is handled by comparing against the current output list each time.
    pub fn mark_session_lock_output_secured(&mut self, output_name: &str) {
        if !self.session_lock.active {
            return;
        }
        self.session_lock
            .secured_outputs
            .insert(output_name.to_owned());
        self.confirm_session_lock_if_ready();
    }

    pub fn confirm_session_lock_if_ready(&mut self) {
        let all_secured = session_lock_outputs_secured(
            self.outputs.iter().map(|output| output.name.as_str()),
            &self.session_lock.secured_outputs,
        );
        if all_secured {
            if let Some(confirmation) = self.session_lock.confirmation.take() {
                confirmation.lock();
            }
        }
    }

    /// Updates the lock client's required buffer dimensions after hotplug or mode changes.
    fn configure_session_lock_surface(&mut self, output_name: &str) {
        let Some(area) = self
            .outputs
            .iter()
            .find(|output| output.name == output_name)
            .map(|output| output.screen_area)
        else {
            return;
        };
        if let Some(surface) = self
            .session_lock
            .surfaces
            .iter()
            .find(|entry| entry.output.name() == output_name)
            .map(|entry| entry.surface.clone())
        {
            surface.with_pending_state(|state| {
                state.size = Some((area.width as u32, area.height as u32).into());
            });
            surface.send_configure();
            self.request_repaint();
        }
    }

    /// Show or hide the bar on all outputs, giving the freed space back to clients.
    #[cfg(feature = "bar")]
    pub fn toggle_bar(&mut self) {
        self.bar_visible = !self.bar_visible;
        let areas = self
            .outputs
            .iter()
            .map(|output| (output.name.clone(), output.screen_area))
            .collect::<Vec<_>>();
        for (name, area) in areas {
            self.configure_output(&name, area);
        }
        self.request_repaint();
    }

    /// Registers or resizes one logical output. Backends choose the global position; keeping that
    /// choice here makes input, layout, popups, bars and launcher placement share one coordinate
    /// system. Existing tag/layout state survives a mode change.
    pub fn configure_output(&mut self, name: &str, area: Rect) {
        let first_output = self.outputs.is_empty();
        #[cfg(feature = "bar")]
        let bar_height = reserved_bar_height(area.height, self.config.bar.height, self.bar_visible);
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
                tag_layouts: [LayoutMode::default(); 9],
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
                managed.tags = self.outputs.first().map_or(1, |o| o.selected_tags);
                managed.floating_geometry = None;
            }
        }
        #[cfg(feature = "layer-shell")]
        self.arrange_layers();
        self.configure_session_lock_surface(name);
        self.arrange();
    }

    /// Removes an unplugged output and migrates its windows to the nearest surviving output. This
    /// guarantees that hot-unplug never strands a live client outside the visible desktop.
    pub fn remove_output(&mut self, name: &str) {
        let removed_area = self
            .outputs
            .iter()
            .find(|output| output.name == name)
            .map(|output| output.screen_area);
        #[cfg(feature = "layer-shell")]
        self.close_output_layers(name);
        self.outputs.retain(|output| output.name != name);
        #[cfg(feature = "overview")]
        if self.overview_output.as_deref() == Some(name) {
            self.overview_output = self.outputs.first().map(|output| output.name.clone());
        }
        self.session_lock
            .surfaces
            .retain(|entry| entry.output.name() != name);
        self.session_lock.secured_outputs.remove(name);
        // Prefer the geometrically nearest remaining display, not an arbitrary
        // connector/index. A removed window must always use the fallback's visible tags.
        let disconnected = name.to_owned();
        let focused_window = self.seat.get_keyboard().unwrap().current_focus();
        let fallback = self
            .outputs
            .iter()
            .enumerate()
            .min_by_key(|(index, output)| {
                let previous = removed_area.unwrap_or(output.screen_area);
                let cx = i64::from(previous.x) * 2 + i64::from(previous.width);
                let cy = i64::from(previous.y) * 2 + i64::from(previous.height);
                let nx = i64::from(output.screen_area.x) * 2 + i64::from(output.screen_area.width);
                let ny = i64::from(output.screen_area.y) * 2 + i64::from(output.screen_area.height);
                ((cx - nx).pow(2).saturating_add((cy - ny).pow(2)), *index)
            })
            .map(|(_, output)| output.name.clone());
        if let Some(fallback) = fallback {
            let visible = self
                .outputs
                .iter()
                .find(|output| output.name == fallback)
                .map_or(1, |output| output.selected_tags);
            for managed in &mut self.windows {
                if managed.output == disconnected {
                    managed.output.clone_from(&fallback);
                    managed.tags = visible;
                    managed.floating_geometry = None;
                    managed.placement_resolved = true;
                }
            }
            if self.focused_output.as_deref() == Some(name) {
                self.focused_output = Some(fallback.clone());
                self.arrange();
                // Keep the keyboard on the same toplevel after hot-unplug if possible.
                if let Some(index) = self.visible_indices_for(&fallback).iter().position(|&i| {
                    focused_window.as_ref().is_some_and(|surface| {
                        self.windows[i].window.wl_surface().as_deref() == Some(surface)
                    })
                }) {
                    self.focus_index(index);
                } else {
                    self.focus_index(0);
                }
            }
        } else {
            self.focused_output = None;
        }
        self.confirm_session_lock_if_ready();
        self.arrange();
    }

    #[cfg(feature = "overview")]
    pub fn toggle_overview(&mut self) {
        if self.session_locked() {
            return;
        }
        if self.overview_output.is_some() {
            self.close_overview();
            return;
        }
        self.overview_output = self.focused_output.clone();
        let focused = self.seat.get_keyboard().unwrap().current_focus();
        self.overview_selected = self
            .windows
            .iter()
            .find(|managed| {
                managed.window.alive()
                    && focused.as_ref().is_some_and(|surface| {
                        managed.window.wl_surface().as_deref() == Some(surface)
                    })
            })
            .or_else(|| self.windows.iter().find(|managed| managed.window.alive()))
            .map(|managed| managed.window.clone());
        self.request_repaint();
    }

    #[cfg(feature = "overview")]
    pub fn close_overview(&mut self) {
        if self.overview_output.take().is_some() {
            self.overview_selected = None;
            self.request_repaint();
        }
    }

    #[cfg(feature = "overview")]
    pub fn overview_snapshot(&self, width: i32, height: i32) -> OverviewSnapshot {
        let candidates = self.windows.iter().filter(|managed| managed.window.alive());
        let selected = self.overview_selected.as_ref().filter(|selected| {
            self.windows
                .iter()
                .any(|managed| managed.window == **selected && managed.window.alive())
        });
        let mut tiles: Vec<OverviewTile> = candidates
            .map(|managed| {
                let (title, app_id, _) = window_metadata(&managed.window);
                OverviewTile {
                    selected: selected.is_some_and(|selected| *selected == managed.window),
                    window: managed.window.clone(),
                    title: title.unwrap_or_else(|| "Untitled window".into()),
                    app_id: app_id.unwrap_or_default(),
                    output: managed.output.clone(),
                    tags: managed.tags,
                    page: 0,
                    card: Rect::new(0, 0, 0, 0),
                    preview: Rect::new(0, 0, 0, 0),
                }
            })
            .collect();
        if !tiles.is_empty() && !tiles.iter().any(|tile| tile.selected) {
            tiles[0].selected = true;
        }
        let outputs = self
            .outputs
            .iter()
            .map(|output| output.name.clone())
            .collect::<Vec<_>>();
        OverviewSnapshot::new(width, height, tiles, &outputs)
    }

    #[cfg(feature = "overview")]
    pub fn overview_navigate(&mut self, horizontal: isize, vertical: isize) {
        let Some(name) = self.overview_output.as_deref() else {
            return;
        };
        let Some(area) = self
            .outputs
            .iter()
            .find(|output| output.name == name)
            .map(|output| output.screen_area)
        else {
            return;
        };
        let snapshot = self.overview_snapshot(area.width, area.height);
        let count = snapshot.tiles.len();
        if count == 0 {
            return;
        }
        let current = snapshot
            .tiles
            .iter()
            .position(|tile| tile.selected)
            .unwrap_or(0);
        let next = if horizontal < 0 {
            (current + count - 1) % count
        } else if horizontal > 0 {
            (current + 1) % count
        } else if vertical != 0 {
            // Use actual card positions: a grouped layout has header rows and app
            // sections, so the old flat-grid column calculation is no longer valid.
            let here = &snapshot.tiles[current];
            let cx = here.card.x + here.card.width / 2;
            let cy = here.card.y + here.card.height / 2;
            snapshot
                .tiles
                .iter()
                .enumerate()
                .filter(|(_, tile)| {
                    tile.page == here.page
                        && ((vertical < 0 && tile.card.y + tile.card.height / 2 < cy)
                            || (vertical > 0 && tile.card.y + tile.card.height / 2 > cy))
                })
                .min_by_key(|(_, tile)| {
                    let dx = i64::from(tile.card.x + tile.card.width / 2 - cx).abs();
                    let dy = i64::from(tile.card.y + tile.card.height / 2 - cy).abs();
                    dy * 4 + dx
                })
                .map(|(index, _)| index)
                // At the edge of a page, move to the previous/next item. This
                // automatically scrolls to the new page via the selected tile.
                .unwrap_or_else(|| {
                    if vertical < 0 {
                        current.saturating_sub(1)
                    } else {
                        (current + 1).min(count - 1)
                    }
                })
        } else {
            current
        };
        self.overview_selected = Some(snapshot.tiles[next].window.clone());
        self.request_repaint();
    }

    #[cfg(feature = "overview")]
    pub fn overview_activate(&mut self) {
        if self.overview_output.is_none() || self.session_locked() {
            return;
        }
        let target = self
            .overview_selected
            .as_ref()
            .filter(|selected| {
                self.windows
                    .iter()
                    .any(|managed| managed.window == **selected && managed.window.alive())
            })
            .cloned()
            .or_else(|| {
                self.windows
                    .iter()
                    .find(|managed| managed.window.alive())
                    .map(|managed| managed.window.clone())
            });
        self.close_overview();
        if let Some(target) = target {
            self.activate_window(&target);
        }
    }

    #[cfg(feature = "overview")]
    pub fn overview_click(&mut self, position: Point<f64, Logical>) {
        let Some(name) = self.overview_output.as_deref() else {
            return;
        };
        let Some(area) = self
            .outputs
            .iter()
            .find(|output| output.name == name)
            .map(|output| output.screen_area)
        else {
            return;
        };
        let local_x = position.x - f64::from(area.x);
        let local_y = position.y - f64::from(area.y);
        let snapshot = self.overview_snapshot(area.width, area.height);
        let visible_page = snapshot.page;
        let target = snapshot
            .tiles
            .into_iter()
            .filter(|tile| tile.page == visible_page)
            .find(|tile| {
                let card = tile.card;
                local_x >= f64::from(card.x)
                    && local_x < f64::from(card.x + card.width)
                    && local_y >= f64::from(card.y)
                    && local_y < f64::from(card.y + card.height)
            })
            .map(|tile| tile.window);
        if let Some(target) = target {
            self.close_overview();
            self.activate_window(&target);
        }
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
        // Retain clients created during a zero-output interval. configure_output assigns
        // orphan windows when the first display returns; dropping them here strands the
        // client's live toplevel forever even after reconnecting a monitor.
        let (output_name, selected_tags) = self.active_output().map_or_else(
            || (String::new(), 1),
            |output| (output.name.clone(), output.selected_tags),
        );
        self.windows.push(ManagedWindow {
            window,
            output: output_name.clone(),
            tags: selected_tags,
            rule_floating: false,
            placement_resolved: false,
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
        let Some(removed_index) = self
            .windows
            .iter()
            .position(|managed| managed.window.wl_surface().as_deref() == Some(surface))
        else {
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
        if !self.session_locked() && self.focused_window_geometry().is_none() {
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
            match output.layout_mode() {
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
            #[cfg(feature = "xwayland")]
            if let Some(surface) = window.x11_surface() {
                let _ = surface.configure(Rectangle::new(
                    (geometry.x, geometry.y).into(),
                    (geometry.width, geometry.height).into(),
                ));
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
                (managed.window.wl_surface().as_deref() == Some(&focused))
                    .then(|| managed.window.clone())
            }) {
                self.space.raise_element(&window, true);
            }
        }
        // Ownership may have changed even when logical geometry happens to be identical. Notify
        // clients after placements are applied so their next buffer uses the destination scale.
        self.refresh_fractional_scales();
        // Idle inhibitors are effective only while their owning toplevel is actually visible.
        // Re-evaluate after every tag, layout, hotplug or window-lifetime rearrangement.
        self.refresh_idle_inhibition();
    }

    /// Sends each opted-in surface the scale of the display that owns its top-level window.
    pub(crate) fn refresh_fractional_scales(&mut self) {
        self.fractional_scale_surfaces.retain(WlSurface::alive);
        for surface in &self.fractional_scale_surfaces {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            let Some(output_name) = self.windows.iter().find_map(|managed| {
                (managed.window.wl_surface().as_deref() == Some(&root))
                    .then_some(managed.output.as_str())
            }) else {
                continue;
            };
            let Some(scale) = self
                .space
                .outputs()
                .find(|output| output.name() == output_name)
                .map(|output| output.current_scale().fractional_scale())
            else {
                continue;
            };
            with_states(surface, |states| {
                with_fractional_scale(states, |fractional| fractional.set_preferred_scale(scale))
            });
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
        let mode = output.cycle_layout_mode();
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
        if output.layout_mode() != LayoutMode::Floating && !self.windows[index].rule_floating {
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
        let placement = self.config.window_placement(app_id, title);
        let default_width = self.config.floating.default_width;
        let default_height = self.config.floating.default_height;
        let outer_gap = self.config.layout.outer_gap;
        let active_output = self
            .focused_output
            .as_deref()
            .and_then(|name| self.outputs.iter().find(|output| output.name == name))
            .or_else(|| self.outputs.first());
        let fallback = active_output.map(|o| (o.name.clone(), o.selected_tags));
        let desired = placement
            .output
            .as_ref()
            .and_then(|name| self.outputs.iter().find(|o| &o.name == name))
            .map(|o| (o.name.clone(), o.selected_tags));
        let Some(managed) = self
            .windows
            .iter_mut()
            .find(|managed| managed.window.wl_surface().as_deref() == Some(surface))
        else {
            return;
        };
        let mut placement_changed = false;
        if !managed.placement_resolved {
            let matching_placement = placement.output.is_some() || placement.tag.is_some();
            if matching_placement {
                if let Some(output_name) = placement.output.as_ref() {
                    if let Some((name, visible_tags)) = desired {
                        let tags = placement.tag.map_or(visible_tags, |t| 1 << (t - 1));
                        placement_changed = managed.output != name || managed.tags != tags;
                        managed.output = name;
                        managed.tags = tags;
                    } else {
                        // Requested connector is absent; keep client on a visible tag.
                        let (name, tags) = fallback.unwrap_or_else(|| (String::new(), 1));
                        placement_changed = managed.output != name || managed.tags != tags;
                        managed.output = name;
                        managed.tags = tags;
                        tracing::info!(requested = %output_name, actual = %managed.output,
                            "rule output unavailable; window placed on visible fallback");
                    }
                } else if let Some(tag) = placement.tag {
                    let tags = 1 << (tag - 1);
                    placement_changed = managed.tags != tags;
                    managed.tags = tags;
                }
                managed.placement_resolved = true;
                if placement_changed {
                    managed.floating_geometry = None;
                }
            } else if app_id.is_some() && title.is_some() {
                // Once a full identity is known, future title changes must not
                // unexpectedly trigger a newly matching initial placement rule.
                managed.placement_resolved = true;
            }
        }
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
        if was_floating != should_float || placement_changed {
            self.arrange();
            // If the originally focused toplevel moved to a hidden output/tag,
            // restore ordinary focus on the currently selected visible tag.
            if placement_changed && !self.session_locked() {
                let focused = self.seat.get_keyboard().unwrap().current_focus();
                if focused.as_ref() == Some(surface) {
                    let still_visible = self
                        .windows
                        .iter()
                        .find(|managed| managed.window.wl_surface().as_deref() == Some(surface))
                        .is_some_and(|managed| {
                            self.outputs.iter().any(|output| {
                                self.focused_output.as_deref() == Some(output.name.as_str())
                                    && managed.output == output.name
                                    && managed.tags & output.selected_tags != 0
                            })
                        });
                    if !still_visible {
                        self.focus_index(0);
                    }
                }
            }
        }
    }

    pub fn focus_index(&mut self, visible_index: usize) {
        // Client creation/destruction callbacks can run while a locker owns the seat.
        if self.session_locked() {
            return;
        }
        let Some(output_name) = self.focused_output.clone() else {
            return;
        };
        let visible = self.visible_indices_for(&output_name);
        let Some(&index) = visible.get(visible_index) else {
            // Selecting an empty tag must not leave input routed to its now-hidden old client.
            self.seat.get_keyboard().unwrap().set_focus(
                self,
                Option::<WlSurface>::None,
                SERIAL_COUNTER.next_serial(),
            );
            self.request_repaint();
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
            .map_or(LayoutMode::Tiling, |output| output.layout_mode());
        if layout_mode != LayoutMode::Tiling || self.windows[index].rule_floating {
            self.space.raise_element(&target, true);
        }
        self.seat.get_keyboard().unwrap().set_focus(
            self,
            target.wl_surface().map(|surface| surface.into_owned()),
            serial,
        );
    }

    /// Reveals and focuses a managed surface named by a valid activation request.
    ///
    /// Tags and layouts are per monitor, so merely assigning keyboard focus would leave a hidden
    /// window invisible. Selecting its owning monitor and tag first makes activation predictable.
    pub(crate) fn activate_surface(&mut self, surface: &WlSurface) {
        let target = self
            .windows
            .iter()
            .find(|managed| managed.window.wl_surface().as_deref() == Some(surface))
            .map(|managed| managed.window.clone());
        if let Some(target) = target {
            self.activate_window(&target);
        }
    }

    /// One activation path shared by protocol activation and overview selection.
    fn activate_window(&mut self, target: &Window) {
        let Some((output_name, tags)) = self.windows.iter().find_map(|managed| {
            (managed.window == *target && managed.window.alive())
                .then(|| (managed.output.clone(), managed.tags))
        }) else {
            return;
        };
        let Some(output) = self
            .outputs
            .iter_mut()
            .find(|output| output.name == output_name)
        else {
            return;
        };
        output.selected_tags = tags;
        self.focused_output = Some(output_name.clone());
        self.arrange();
        if let Some(visible_index) = self
            .visible_indices_for(&output_name)
            .iter()
            .position(|&i| self.windows[i].window == *target)
        {
            self.focus_index(visible_index);
        }
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
                visible
                    .iter()
                    .position(|&i| self.windows[i].window.wl_surface().as_deref() == Some(&surface))
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
                visible
                    .iter()
                    .position(|&i| self.windows[i].window.wl_surface().as_deref() == Some(&surface))
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
            if let Some(managed) = self
                .windows
                .iter_mut()
                .find(|m| m.window.wl_surface().as_deref() == Some(&surface))
            {
                managed.tags = 1 << tag;
                managed.placement_resolved = true;
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
        self.move_focused_window_to_output(&destination);
    }

    /// Select the nearest geometrically adjacent output without cyclic wrapping.
    /// Shift-modified bindings use the same destination for focused-window moves.
    pub fn directional_output(&mut self, direction: Direction, move_window: bool) {
        let Some(source) = self.focused_output.as_deref() else {
            return;
        };
        let Some(current) = self.outputs.iter().position(|output| output.name == source) else {
            return;
        };
        let geometry = self
            .outputs
            .iter()
            .map(|output| (output.name.as_str(), output.screen_area))
            .collect::<Vec<_>>();
        let Some(next) = output_navigation::neighbor(&geometry, current, direction) else {
            return;
        };
        let destination = self.outputs[next].name.clone();
        if move_window {
            self.move_focused_window_to_output(&destination);
        } else {
            self.focused_output = Some(destination);
            self.focus_index(0);
            self.request_repaint();
        }
    }

    /// Shared move policy for legacy cycling and geometric navigation.
    fn move_focused_window_to_output(&mut self, destination: &str) {
        let Some(destination_tags) = self
            .outputs
            .iter()
            .find(|o| o.name == destination)
            .map(|output| output.selected_tags)
        else {
            return;
        };
        let focused = self.seat.get_keyboard().unwrap().current_focus();
        let moved_window = self
            .windows
            .iter_mut()
            .find(|managed| {
                focused
                    .as_ref()
                    .is_some_and(|surface| managed.window.wl_surface().as_deref() == Some(surface))
            })
            .map(|managed| {
                managed.output = destination.to_owned();
                // A move must always leave its window visible at the destination.
                managed.tags = destination_tags;
                managed.placement_resolved = true;
                managed.floating_geometry = None;
                managed.window.clone()
            });
        if let Some(moved_window) = moved_window {
            self.focused_output = Some(destination.to_owned());
            self.arrange();
            let moved = self
                .visible_indices()
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
            } else {
                #[cfg(feature = "xwayland")]
                if let Some(x11) = self.windows.iter().find_map(|managed| {
                    managed
                        .window
                        .x11_surface()
                        .filter(|x11| x11.wl_surface().as_ref() == Some(&surface))
                }) {
                    let _ = x11.close();
                }
            }
        }
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        if self.session_lock.active {
            // Lock surfaces are the only legal input target while locked. Their configured size is
            // exactly the display area, so the display origin is also the surface-tree origin.
            return self.outputs.iter().find_map(|output| {
                let area = output.screen_area;
                let inside = pos.x >= f64::from(area.x)
                    && pos.x < f64::from(area.x + area.width)
                    && pos.y >= f64::from(area.y)
                    && pos.y < f64::from(area.y + area.height);
                inside.then(|| {
                    self.session_lock
                        .surfaces
                        .iter()
                        .find(|entry| entry.output.name() == output.name && entry.surface.alive())
                        .map(|entry| {
                            (
                                entry.surface.wl_surface().clone(),
                                (f64::from(area.x), f64::from(area.y)).into(),
                            )
                        })
                })?
            });
        }
        #[cfg(feature = "layer-shell")]
        if let Some(hit) = self.layer_surface_under(pos, true) {
            return Some(hit);
        }
        // Pointer focus needs the concrete wl_surface and surface-local origin, not merely Anvil's
        // top-level Window. `surface_under` descends into subsurfaces such as client-side menus.
        let window_hit = self
            .space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(surface, point)| (surface, (point + location).to_f64()))
            });
        #[cfg(feature = "layer-shell")]
        let window_hit = window_hit.or_else(|| self.layer_surface_under(pos, false));
        window_hit
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
                let window = &self.windows[index].window;
                let (title, app_id, _) = window_metadata(window);
                BarWindow {
                    title: title.or(app_id).unwrap_or_else(|| "untitled".into()),
                    focused: focused
                        .as_ref()
                        .is_some_and(|surface| window.wl_surface().as_deref() == Some(surface)),
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
            layout_symbol: output.layout_mode().symbol(),
            window_counts,
            windows,
            status: self.bar.text().to_owned(),
            network: self.bar.network(),
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
        let window = self
            .windows
            .iter()
            .find(|managed| managed.window.wl_surface().as_deref() == Some(&focused))?;
        self.space.element_geometry(&window.window)
    }
}

#[cfg(any(feature = "bar", feature = "anvilctl", feature = "overview"))]
fn window_metadata(window: &Window) -> (Option<String>, Option<String>, bool) {
    if let Some(toplevel) = window.toplevel() {
        let (title, app_id) = with_states(toplevel.wl_surface(), |states| {
            let attributes = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .expect("xdg toplevel role data missing")
                .lock()
                .unwrap();
            (attributes.title.clone(), attributes.app_id.clone())
        });
        return (title, app_id, toplevel.parent().is_some());
    }
    #[cfg(feature = "xwayland")]
    if let Some(surface) = window.x11_surface() {
        let title = surface.title();
        let class = surface.class();
        return (
            (!title.is_empty()).then_some(title),
            (!class.is_empty()).then_some(class),
            surface.is_popup() || surface.is_transient_for().is_some(),
        );
    }
    (None, None, false)
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

/// Returns true only after every currently connected display has submitted a secure frame.
fn session_lock_outputs_secured<'a>(
    outputs: impl IntoIterator<Item = &'a str>,
    secured: &HashSet<String>,
) -> bool {
    outputs.into_iter().all(|output| secured.contains(output))
}

#[cfg(test)]
mod tests {
    use super::{
        centered_floating_geometry, clamp_floating_geometry, session_lock_outputs_secured,
        successor_focus_index,
    };
    use anvil::layout::Rect;
    use std::collections::HashSet;

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

    #[test]
    fn session_lock_waits_for_every_current_display() {
        let mut secured = HashSet::from(["DP-1".to_owned()]);
        assert!(session_lock_outputs_secured(["DP-1"], &secured));
        assert!(!session_lock_outputs_secured(
            ["DP-1", "HDMI-A-1"],
            &secured
        ));

        secured.insert("HDMI-A-1".to_owned());
        assert!(session_lock_outputs_secured(["DP-1", "HDMI-A-1"], &secured));
        // Removing a display must not leave the lock confirmation waiting on stale hardware.
        assert!(session_lock_outputs_secured(["HDMI-A-1"], &secured));
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

#[cfg(all(test, feature = "anvilctl"))]
mod recovery_tests {
    use super::*;
    use smithay::output::{Mode, PhysicalProperties, Subpixel};
    use std::{
        io::{BufRead, BufReader},
        process::Stdio,
        sync::mpsc,
        time::Duration,
    };

    struct TestClient(Child);
    impl Drop for TestClient {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn start_client(
        event_loop: &mut EventLoop<CalloopData>,
        data: &mut CalloopData,
        mode: &str,
        role: &str,
    ) -> TestClient {
        let client = Command::new("target/protocol-tests/client")
            .args([mode, role])
            .env("WAYLAND_DISPLAY", &data.state.socket_name)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut client = TestClient(client);
        let stdout = client.0.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).unwrap();
            let _ = tx.send(line);
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            event_loop
                .dispatch(Duration::from_millis(10), data)
                .unwrap();
            data.display_handle.flush_clients().unwrap();
            if let Ok(line) = rx.try_recv() {
                assert_eq!(line.trim(), "READY");
                break;
            }
            assert!(Instant::now() < deadline, "client did not configure");
        }
        reader.join().unwrap();
        client
    }

    /// Invoked explicitly by graphical CI after the C client is compiled. Normal unit runs do
    /// not require an XDG runtime directory or permission to bind Wayland sockets.
    #[test]
    #[ignore = "requires Wayland sockets and tests/protocols/build.sh"]
    fn logical_output_recovery() {
        let mut event_loop: EventLoop<CalloopData> = EventLoop::try_new().unwrap();
        let display = Display::<Anvil>::new().unwrap();
        let dh = display.handle();
        let state = Anvil::new(&mut event_loop, display, Config::default(), None).unwrap();
        let mut data = CalloopData {
            state,
            display_handle: dh.clone(),
        };
        let mut outputs = Vec::new();
        for (name, x) in [("left", 0), ("right", 800)] {
            let output = Output::new(
                name.into(),
                PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: Subpixel::Unknown,
                    make: "test".into(),
                    model: "logical".into(),
                },
            );
            output.create_global::<Anvil>(&dh);
            output.change_current_state(
                Some(Mode {
                    size: (800, 600).into(),
                    refresh: 60_000,
                }),
                None,
                None,
                Some((x, 0).into()),
            );
            data.state.space.map_output(&output, (x, 0));
            data.state.configure_output(name, Rect::new(x, 0, 800, 600));
            outputs.push(output);
        }
        data.state.focused_output = Some("right".into());
        data.state
            .outputs
            .iter_mut()
            .find(|o| o.name == "right")
            .unwrap()
            .tag_layouts[0] = LayoutMode::Fullscreen;
        let mode = if cfg!(feature = "layer-shell") {
            "layer-shell"
        } else {
            "default"
        };
        let mut client = start_client(&mut event_loop, &mut data, mode, "hold");
        assert_eq!(data.state.windows.len(), 1);
        assert_eq!(data.state.windows[0].output, "right");
        data.state.space.unmap_output(&outputs[1]);
        data.state.remove_output("right");
        assert_eq!(data.state.windows[0].output, "left");
        assert_eq!(data.state.visible_indices_for("left").len(), 1);
        let location = data
            .state
            .space
            .element_location(&data.state.windows[0].window)
            .unwrap();
        assert!(location.x >= 0 && location.x < 800);
        // Simulate the already-confirmed lock boundary while changing logical display policy.
        data.state.session_lock.active = true;
        data.state
            .session_lock
            .secured_outputs
            .insert("left".into());
        data.state
            .configure_output("new", Rect::new(800, 0, 800, 600));
        assert!(data.state.session_locked());
        assert!(!data.state.session_lock.secured_outputs.contains("new"));
        data.state.remove_output("left");
        assert_eq!(data.state.windows[0].output, "new");
        assert!(!data.state.session_lock.secured_outputs.contains("left"));
        data.state.remove_output("new");
        assert!(data.state.session_locked());
        assert_eq!(data.state.windows.len(), 1);
        assert!(
            data.state
                .space
                .element_location(&data.state.windows[0].window)
                .is_none()
        );
        let mut late_client = start_client(&mut event_loop, &mut data, mode, "locked-window");
        assert_eq!(data.state.windows.len(), 2);
        assert!(data.state.windows[1].output.is_empty());
        assert!(
            data.state
                .space
                .element_location(&data.state.windows[1].window)
                .is_none()
        );
        data.state
            .configure_output("returned", Rect::new(0, 0, 640, 480));
        for managed in &data.state.windows {
            assert_eq!(managed.output, "returned");
            assert!(data.state.space.element_location(&managed.window).is_some());
        }
        assert!(data.state.session_locked());
        client.0.kill().unwrap();
        client.0.wait().unwrap();
        late_client.0.kill().unwrap();
        late_client.0.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !data.state.windows.is_empty() {
            event_loop
                .dispatch(Duration::from_millis(10), &mut data)
                .unwrap();
            data.display_handle.flush_clients().unwrap();
            assert!(Instant::now() < deadline, "dead client retained windows");
        }
    }
}

#[cfg(test)]
mod per_tag_layout_tests {
    use super::*;

    fn workspace(name: &str) -> OutputWorkspace {
        OutputWorkspace {
            name: name.to_string(),
            screen_area: Rect::new(0, 0, 800, 600),
            output_area: Rect::new(0, 22, 800, 578),
            selected_tags: 1,
            tag_layouts: [LayoutMode::default(); 9],
        }
    }

    #[test]
    fn tag_modes_are_independent_and_restore_on_switch() {
        let mut output = workspace("DP-1");
        assert_eq!(output.layout_mode(), LayoutMode::Tiling);
        output.cycle_layout_mode();
        assert_eq!(output.layout_mode(), LayoutMode::Fullscreen);
        output.selected_tags = 1 << 1;
        assert_eq!(output.layout_mode(), LayoutMode::Tiling);
        output.cycle_layout_mode();
        output.cycle_layout_mode();
        assert_eq!(output.layout_mode(), LayoutMode::Floating);
        output.selected_tags = 1 << 2;
        assert_eq!(output.layout_mode(), LayoutMode::Tiling);
        output.selected_tags = 1;
        assert_eq!(output.layout_mode(), LayoutMode::Fullscreen);
        output.selected_tags = 1 << 1;
        assert_eq!(output.layout_mode(), LayoutMode::Floating);
    }

    #[test]
    fn layout_modes_belong_to_output_as_well_as_tag() {
        let mut left = workspace("DP-1");
        let mut right = workspace("HDMI-A-1");
        left.selected_tags = 1 << 1;
        right.selected_tags = 1 << 1;
        left.cycle_layout_mode();
        assert_eq!(left.layout_mode(), LayoutMode::Fullscreen);
        assert_eq!(right.layout_mode(), LayoutMode::Tiling);
        right.cycle_layout_mode();
        right.cycle_layout_mode();
        assert_eq!(right.layout_mode(), LayoutMode::Floating);
        assert_eq!(left.layout_mode(), LayoutMode::Fullscreen);
    }

    #[test]
    fn multi_tag_uses_lowest_set_bit_and_only_changes_that_tag() {
        let mut output = workspace("DP-1");
        output.selected_tags = (1 << 2) | (1 << 5);
        assert_eq!(output.active_tag(), 2);
        output.cycle_layout_mode();
        assert_eq!(output.layout_mode(), LayoutMode::Fullscreen);
        output.selected_tags = 1 << 5;
        assert_eq!(output.layout_mode(), LayoutMode::Tiling);
        output.selected_tags = 1 << 2;
        assert_eq!(output.layout_mode(), LayoutMode::Fullscreen);
        output.selected_tags = 0;
        assert_eq!(output.active_tag(), 0);
    }

    #[test]
    fn existing_output_preserves_modes_across_resize_and_bar_visibility() {
        let mut output = workspace("DP-1");
        output.selected_tags = 1 << 8;
        output.cycle_layout_mode();
        output.screen_area = Rect::new(0, 0, 1920, 1080);
        output.output_area = Rect::new(0, 0, 1920, 1080);
        assert_eq!(output.layout_mode(), LayoutMode::Fullscreen);
        output.selected_tags = 1;
        assert_eq!(output.layout_mode(), LayoutMode::Tiling);
        // Nine enum values are stored inline; no per-tag heap allocation or polling.
        assert!(std::mem::size_of_val(&output.tag_layouts) <= 36);
    }
}

#[cfg(feature = "bar")]
fn reserved_bar_height(output_height: i32, configured_height: i32, visible: bool) -> i32 {
    if visible {
        configured_height
            .min(output_height.saturating_sub(1))
            .max(0)
    } else {
        0
    }
}

#[cfg(all(test, feature = "bar"))]
mod bar_visibility_tests {
    use super::reserved_bar_height;

    #[test]
    fn toggled_bar_releases_the_entire_output_area() {
        assert_eq!(reserved_bar_height(800, 26, true), 26);
        assert_eq!(reserved_bar_height(800, 26, false), 0);
        assert_eq!(reserved_bar_height(12, 26, true), 11);
        assert_eq!(reserved_bar_height(12, 26, false), 0);
        assert_eq!(reserved_bar_height(0, 26, true), 0);
    }
}

#[cfg(feature = "anvilctl")]
fn validate_reload_transition(previous: &Config, next: &Config) -> anyhow::Result<()> {
    #[cfg(feature = "bar")]
    if next.bar.font != previous.bar.font {
        anyhow::bail!("bar.font changed: restart required");
    }
    if next.bar.network.enabled != previous.bar.network.enabled
        || next.bar.network.icon_font != previous.bar.network.icon_font
    {
        anyhow::bail!("bar.network.enabled or icon_font changed: restart required");
    }
    if next.compat != previous.compat {
        anyhow::bail!("compat settings changed: restart required");
    }
    if next.general.startup != previous.general.startup {
        anyhow::bail!("general.startup changed: restart required");
    }
    if next.input.keyboard != previous.input.keyboard {
        // Re-adding a keyboard seat tears down the existing capability for every client.
        anyhow::bail!(
            "input.keyboard changed: restart required (recreating the keyboard seat would interrupt clients)"
        );
    }
    Ok(())
}

#[cfg(all(test, feature = "anvilctl"))]
mod config_reload_tests {
    use super::*;

    #[test]
    fn reload_checks_reject_restart_only_changes_without_mutating_old_config() {
        let previous = Config::default();
        let mut new = previous.clone();
        new.compat.xwayland = !new.compat.xwayland;
        assert!(
            validate_reload_transition(&previous, &new)
                .unwrap_err()
                .to_string()
                .contains("restart required")
        );
        assert_ne!(previous, new);
        new = previous.clone();
        new.input.keyboard.layout = "de".into();
        assert!(
            validate_reload_transition(&previous, &new)
                .unwrap_err()
                .to_string()
                .contains("restart required")
        );
        new = previous.clone();
        new.layout.gap += 1;
        new.outputs.push(anvil::config::OutputConfig {
            name: "DP-1".into(),
            ..Default::default()
        });
        assert!(validate_reload_transition(&previous, &new).is_ok());
    }
}
