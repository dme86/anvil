//! Direct Linux backend using libseat, udev, DRM/KMS, GBM/EGL and libinput.
//!
//! Unlike the Winit development backend, this module owns a real seat and display controller. It
//! opens the DRM device through libseat (so VT switching remains safe), assigns every connected
//! connector a distinct CRTC, renders Anvil's `Space` into per-output GBM buffers and submits them
//! to KMS. libinput events feed the same backend-independent input policy used by nested mode.

use std::{cell::RefCell, path::Path, rc::Rc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use smithay::{
    backend::{
        allocator::{
            Fourcc, Modifier,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmDeviceNotifier, DrmEvent, DrmNode,
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLDevice, EGLDisplay, context::ContextPriority},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            ImportAll, ImportMem, ImportMemWl,
            element::{
                memory::MemoryRenderBufferRenderElement, solid::SolidColorRenderElement,
                surface::WaylandSurfaceRenderElement,
            },
            gles::GlesRenderer,
            multigpu::{GpuManager, gbm::GbmGlesBackend},
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent},
    },
    desktop::{Window, space::SpaceRenderElements},
    output::{Mode, Output, PhysicalProperties},
    reexports::{
        calloop::{
            EventLoop,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{Device as _, Mode as DrmMode, ModeTypeFlags, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Transform},
};

use anvil::config::parse_hex_color;

#[cfg(any(feature = "bar", feature = "launcher"))]
use crate::bar::BarRenderer;
use crate::{
    CalloopData,
    render::{FocusBorder, PointerMarker},
};

// These formats are deliberately conservative. Both are ubiquitous 8-bit scanout formats and
// avoid choosing a low-fidelity format merely because it happened to appear first in a driver list.
const SCANOUT_FORMATS: &[Fourcc] = &[Fourcc::Abgr8888, Fourcc::Argb8888];
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

type DirectRenderer<'a> = MultiRendererType<'a>;
type MultiRendererType<'a> = smithay::backend::renderer::multigpu::MultiRenderer<
    'a,
    'a,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

smithay::backend::renderer::element::render_elements! {
    /// One front-to-back list accepted by the DRM compositor.
    DirectRenderElement<R, E> where R: ImportAll + ImportMem;
    Space=SpaceRenderElements<R, E>,
    Border=SolidColorRenderElement,
    Texture=MemoryRenderBufferRenderElement<R>,
}

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
type OutputManager = DrmOutputManager<Allocator, Exporter, (), DrmDeviceFd>;
type KmsOutput = DrmOutput<Allocator, Exporter, (), DrmDeviceFd>;

struct SurfaceData {
    crtc: crtc::Handle,
    connector: connector::Handle,
    output: Output,
    // Keeping the id alive documents ownership; it is removed automatically when the display dies.
    _global: GlobalId,
    drm_output: KmsOutput,
    frame_pending: bool,
    #[cfg(feature = "bar")]
    bar: BarRenderer,
    #[cfg(all(feature = "launcher", not(feature = "bar")))]
    launcher: BarRenderer,
}

struct DirectBackend {
    // The session owns the libseat device handles and restores the VT when Anvil exits.
    _session: LibSeatSession,
    render_node: DrmNode,
    gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    output_manager: OutputManager,
    surfaces: Vec<SurfaceData>,
    border: FocusBorder,
    pointer: PointerMarker,
    active: bool,
}

impl DirectBackend {
    /// Reconciles KMS surfaces with the current connector graph. Removal happens before creation so
    /// unplugged CRTCs are dropped and become available to a newly attached monitor in the same
    /// udev transaction.
    fn rescan_outputs(&mut self, data: &mut CalloopData) -> Result<()> {
        let assignments = connected_outputs(self.output_manager.device()).unwrap_or_default();
        let connected = assignments
            .iter()
            .map(|(connector, _)| connector.handle())
            .collect::<Vec<_>>();
        let mut retained = Vec::with_capacity(self.surfaces.len());
        for surface in self.surfaces.drain(..) {
            if connected.contains(&surface.connector) {
                retained.push(surface);
            } else {
                let name = surface.output.name();
                data.state.space.unmap_output(&surface.output);
                data.state.remove_output(&name);
                data.display_handle
                    .remove_global::<crate::Anvil>(surface._global.clone());
                tracing::info!(output = %name, "DRM output disconnected");
                // Dropping SurfaceData drops DrmOutput, releasing its CRTC from the manager.
            }
        }
        self.surfaces = retained;

        for (connector, crtc) in assignments {
            if self
                .surfaces
                .iter()
                .any(|surface| surface.connector == connector.handle())
            {
                continue;
            }
            let drm_mode = select_drm_mode(connector.modes())?;
            let mode = Mode::from(drm_mode);
            let name = format!(
                "{}-{}",
                connector.interface().as_str(),
                connector.interface_id()
            );
            let (physical_width, physical_height) = connector.size().unwrap_or((0, 0));
            let output = Output::new(
                name.clone(),
                PhysicalProperties {
                    size: (physical_width as i32, physical_height as i32).into(),
                    subpixel: connector.subpixel().into(),
                    make: "unknown".into(),
                    model: "DRM".into(),
                },
            );
            output.set_preferred(mode);
            let global = output.create_global::<crate::Anvil>(&data.display_handle);
            let mut renderer = self
                .gpus
                .single_renderer(&self.render_node)
                .context("cannot acquire renderer for hotplugged output")?;
            let drm_output = self
                .output_manager
                .initialize_output::<_, DirectRenderElement<
                    DirectRenderer<'_>,
                    WaylandSurfaceRenderElement<DirectRenderer<'_>>,
                >>(
                    crtc,
                    drm_mode,
                    &[connector.handle()],
                    &output,
                    None,
                    &mut renderer,
                    &DrmOutputRenderElements::default(),
                )
                .map_err(|error| anyhow!("cannot initialize hotplugged output {name}: {error}"))?;
            drop(renderer);
            self.surfaces.push(SurfaceData {
                crtc,
                connector: connector.handle(),
                output,
                _global: global,
                drm_output,
                frame_pending: false,
                #[cfg(feature = "bar")]
                bar: BarRenderer::new(&data.state.config.bar)?,
                #[cfg(all(feature = "launcher", not(feature = "bar")))]
                launcher: BarRenderer::new(&data.state.config.bar)?,
            });
            tracing::info!(output = %name, "DRM output connected");
        }
        self.refresh_output_mode(data)
    }

    /// Re-reads the connector after a DRM udev change and adopts its new preferred mode.
    ///
    /// Virtio/QEMU updates the connector's mode list when UTM resizes its display. Treating that
    /// preferred mode as authoritative gives the VM the same live-resize behavior as established
    /// compositors without storing host-specific dimensions in Anvil's configuration.
    fn refresh_output_mode(&mut self, data: &mut CalloopData) -> Result<()> {
        let mut x = 0;
        for surface in &mut self.surfaces {
            let connector = self
                .output_manager
                .device()
                .get_connector(surface.connector, true)
                .context("cannot refresh DRM connector")?;
            if connector.state() != connector::State::Connected {
                continue;
            }
            let drm_mode = select_drm_mode(connector.modes())?;
            let mode = Mode::from(drm_mode);
            if surface.output.current_mode() != Some(mode) {
                let mut renderer =
                    self.gpus
                        .single_renderer(&self.render_node)
                        .map_err(|error| {
                            anyhow!("cannot acquire renderer for DRM mode change: {error}")
                        })?;
                surface
                    .drm_output
                    .use_mode(
                        drm_mode,
                        &mut renderer,
                        &DrmOutputRenderElements::<
                            DirectRenderer<'_>,
                            DirectRenderElement<
                                DirectRenderer<'_>,
                                WaylandSurfaceRenderElement<DirectRenderer<'_>>,
                            >,
                        >::default(),
                    )
                    .map_err(|error| anyhow!("cannot apply DRM mode: {error}"))?;
                surface.output.set_preferred(mode);
                surface.frame_pending = false;
            }
            surface.output.change_current_state(
                Some(mode),
                Some(Transform::Normal),
                None,
                Some((x, 0).into()),
            );
            data.state.space.map_output(&surface.output, (x, 0));
            data.state.configure_output(
                &surface.output.name(),
                anvil::layout::Rect::new(x, 0, mode.size.w, mode.size.h),
            );
            x += mode.size.w;
        }
        Ok(())
    }

    fn render(&mut self, data: &mut CalloopData, repaint_requested: bool) -> Result<()> {
        if !self.active || !repaint_requested {
            return Ok(());
        }
        // Consume the coalesced request only when KMS is ready. If a vblank is still pending, the
        // bit remains set and the next scheduler tick retries without rebuilding render elements.
        // Split the backend borrow so the renderer and output may be used together. The renderer
        // lives in GpuManager while DrmOutput owns the KMS swapchain; neither aliases the other.
        let DirectBackend {
            gpus,
            surfaces,
            render_node,
            pointer,
            border,
            ..
        } = self;
        let mut renderer = gpus
            .single_renderer(render_node)
            .map_err(|error| anyhow!("cannot acquire DRM renderer: {error}"))?;
        for surface in surfaces {
            if surface.frame_pending {
                data.state.repaint_requested = true;
                continue;
            }
            let Some(area) = data
                .state
                .outputs
                .iter()
                .find(|output| output.name == surface.output.name())
                .map(|output| output.screen_area)
            else {
                continue;
            };
            let focused = data.state.focused_window_geometry().and_then(|geometry| {
                let center_x = geometry.loc.x + geometry.size.w / 2;
                (center_x >= area.x && center_x < area.x + area.width).then(|| {
                    smithay::utils::Rectangle::new(
                        (geometry.loc.x - area.x, geometry.loc.y - area.y).into(),
                        geometry.size,
                    )
                })
            });
            let mut elements: Vec<
                DirectRenderElement<
                    DirectRenderer<'_>,
                    WaylandSurfaceRenderElement<DirectRenderer<'_>>,
                >,
            > = border
                .elements(focused, data.state.config.appearance.focus_border_width)
                .into_iter()
                .map(DirectRenderElement::Border)
                .collect();
            let pointer_location = data.state.seat.get_pointer().unwrap().current_location();
            if pointer_location.x >= f64::from(area.x)
                && pointer_location.x < f64::from(area.x + area.width)
                && pointer_location.y >= f64::from(area.y)
                && pointer_location.y < f64::from(area.y + area.height)
            {
                let local = (
                    pointer_location.x - f64::from(area.x),
                    pointer_location.y - f64::from(area.y),
                )
                    .into();
                let pointer_element = pointer
                    .element(&mut renderer, local)
                    .map_err(|error| anyhow!("cannot upload cursor texture: {error}"))?;
                elements.push(DirectRenderElement::Texture(pointer_element));
            }
            #[cfg(feature = "bar")]
            {
                let config = data.state.config.bar.clone();
                let snapshot = data.state.bar_snapshot(&surface.output.name());
                let element = surface
                    .bar
                    .element(&mut renderer, area.width, &config, &snapshot)
                    .map_err(|error| anyhow!("cannot upload bar texture: {error}"))?;
                elements.push(DirectRenderElement::Texture(element));
            }
            #[cfg(all(feature = "launcher", not(feature = "bar")))]
            if data.state.focused_output.as_deref() == Some(surface.output.name().as_str()) {
                if let Some(snapshot) = data.state.launcher_snapshot() {
                    let element = surface
                        .launcher
                        .launcher_element(
                            &mut renderer,
                            area.width,
                            area.height,
                            &data.state.config.bar,
                            &snapshot,
                        )
                        .map_err(|error| anyhow!("cannot upload launcher texture: {error}"))?;
                    elements.push(DirectRenderElement::Texture(element));
                }
            }
            let space_elements = smithay::desktop::space::space_render_elements::<_, Window, _>(
                &mut renderer,
                [&data.state.space],
                &surface.output,
                1.0,
            )
            .context("DRM output has no active mode")?;
            elements.extend(space_elements.into_iter().map(DirectRenderElement::Space));
            let frame = surface
                .drm_output
                .render_frame(
                    &mut renderer,
                    &elements,
                    data.state.config.appearance.background,
                    FrameFlags::DEFAULT,
                )
                .map_err(|error| anyhow!("DRM render failed: {error}"))?;
            if !frame.is_empty {
                surface
                    .drm_output
                    .queue_frame(())
                    .map_err(|error| anyhow!("KMS frame submission failed: {error}"))?;
                surface.frame_pending = true;
            }
            data.state.space.elements().for_each(|window| {
                window.send_frame(
                    &surface.output,
                    data.state.start_time.elapsed(),
                    Some(Duration::ZERO),
                    |_, _| Some(surface.output.clone()),
                )
            });
        }
        Ok(())
    }

    fn frame_submitted(&mut self, crtc: crtc::Handle) {
        if let Some(surface) = self
            .surfaces
            .iter_mut()
            .find(|surface| surface.crtc == crtc)
        {
            match surface.drm_output.frame_submitted() {
                Ok(_) => surface.frame_pending = false,
                Err(error) => tracing::warn!(%error, "failed to finish DRM frame"),
            }
        }
    }

    fn pause(&mut self) {
        self.active = false;
        self.output_manager.pause();
        for surface in &mut self.surfaces {
            surface.frame_pending = false;
        }
    }

    fn activate(&mut self) {
        if let Err(error) = self.output_manager.activate(false) {
            tracing::error!(%error, "failed to reactivate DRM device");
            return;
        }
        self.active = true;
        for surface in &mut self.surfaces {
            surface.frame_pending = false;
        }
    }
}

/// Places outputs from every DRM device into one shared logical desktop. UTM models additional
/// displays as separate virtio GPUs, while physical machines commonly expose several connectors
/// on one card; treating both collections uniformly here supports either topology.
fn reflow_outputs(backends: &[Rc<RefCell<DirectBackend>>], data: &mut CalloopData) {
    let mut x = 0;
    for backend in backends {
        for surface in &mut backend.borrow_mut().surfaces {
            let Some(mode) = surface.output.current_mode() else {
                continue;
            };
            surface.output.change_current_state(
                Some(mode),
                Some(Transform::Normal),
                None,
                Some((x, 0).into()),
            );
            data.state.space.map_output(&surface.output, (x, 0));
            data.state.configure_output(
                &surface.output.name(),
                anvil::layout::Rect::new(x, 0, mode.size.w, mode.size.h),
            );
            x += mode.size.w;
        }
    }
}

pub fn init(event_loop: &mut EventLoop<CalloopData>, data: &mut CalloopData) -> Result<()> {
    let (mut session, session_notifier) = LibSeatSession::new()
        .map_err(|error| anyhow!("cannot acquire a libseat session: {error}"))?;
    let seat_name = session.seat();
    let udev = UdevBackend::new(&seat_name)
        .map_err(|error| anyhow!("cannot enumerate DRM devices on {seat_name}: {error}"))?;

    let cards = udev.device_list().collect::<Vec<_>>();
    if cards.is_empty() {
        bail!("no DRM card found for the active seat");
    }
    let mut raw_backends = Vec::with_capacity(cards.len());
    let mut next_x = 0;
    for (_, path) in cards {
        let card_node = DrmNode::from_path(path)
            .map_err(|error| anyhow!("cannot identify DRM node {}: {error}", path.display()))?;
        tracing::info!(path = %path.display(), "opening DRM device");
        match create_backend(&mut session, path, card_node, next_x, data) {
            Ok((backend, notifier)) => {
                next_x = data.state.desktop_bounds().width;
                raw_backends.push((Rc::new(RefCell::new(backend)), notifier));
            }
            // A render-only or connector-less card is not fatal as long as another card supplies
            // a desktop. This matters on hybrid systems where udev exposes several DRM nodes.
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "skipping unusable DRM device")
            }
        }
    }
    if raw_backends.is_empty() {
        bail!("no DRM device could initialize an output");
    }
    let backends = raw_backends
        .iter()
        .map(|(backend, _)| backend.clone())
        .collect::<Vec<_>>();
    reflow_outputs(&backends, data);

    // Advertise all software-buffer formats the actual renderer can import. Without this update,
    // wl_shm clients could choose a format that the GBM/EGL renderer cannot consume.
    let mut first_backend = backends[0].borrow_mut();
    let first_render_node = first_backend.render_node;
    let shm_formats = first_backend
        .gpus
        .single_renderer(&first_render_node)
        .map_err(|error| anyhow!("cannot query renderer formats: {error}"))?
        .shm_formats();
    drop(first_backend);
    data.state.shm_state.update_formats(shm_formats);

    let hotplug_backends = backends.clone();
    event_loop
        .handle()
        .insert_source(udev, move |event, _, data| {
            if matches!(
                event,
                UdevEvent::Added { .. } | UdevEvent::Changed { .. } | UdevEvent::Removed { .. }
            ) {
                for backend in &hotplug_backends {
                    if let Err(error) = backend.borrow_mut().rescan_outputs(data) {
                        tracing::error!(%error, "cannot reconcile DRM outputs");
                    }
                }
                reflow_outputs(&hotplug_backends, data);
            }
        })
        .map_err(|error| anyhow!("cannot register DRM hotplug source: {error}"))?;

    for (drm_backend, drm_notifier) in raw_backends {
        event_loop
            .handle()
            .insert_source(drm_notifier, move |event, _, _data| match event {
                DrmEvent::VBlank(crtc) => drm_backend.borrow_mut().frame_submitted(crtc),
                DrmEvent::Error(error) => tracing::error!(%error, "DRM event error"),
            })
            .context("cannot register DRM event source")?;
    }

    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput_context
        .udev_assign_seat(&seat_name)
        .map_err(|_| anyhow!("cannot assign libinput seat {seat_name}"))?;
    let input_backend = LibinputInputBackend::new(libinput_context.clone());
    event_loop
        .handle()
        .insert_source(input_backend, |event: InputEvent<_>, _, data| {
            data.state.process_input_event(event)
        })
        .map_err(|_| anyhow!("cannot register libinput event source"))?;

    let session_backends = backends.clone();
    event_loop
        .handle()
        .insert_source(session_notifier, move |event, _, _data| match event {
            SessionEvent::PauseSession => {
                tracing::info!("pausing DRM/libinput session");
                libinput_context.suspend();
                for backend in &session_backends {
                    backend.borrow_mut().pause();
                }
            }
            SessionEvent::ActivateSession => {
                tracing::info!("resuming DRM/libinput session");
                if libinput_context.resume().is_err() {
                    tracing::error!("failed to resume libinput");
                }
                for backend in &session_backends {
                    backend.borrow_mut().activate();
                }
            }
        })
        .map_err(|_| anyhow!("cannot register libseat session source"))?;

    let repaint_backends = backends;
    event_loop
        .handle()
        .insert_source(Timer::from_duration(FRAME_INTERVAL), move |_, _, data| {
            #[cfg(feature = "bar")]
            {
                // Shell status commands have their own configured cadence. Only changed stdout
                // damages the bar; a clock that still shows the same minute causes no GPU work.
                let config = data.state.config.bar.clone();
                if data.state.bar.refresh(&config) {
                    data.state.request_repaint();
                }
            }
            // Consume the coalesced request once for the entire multi-GPU group. Each backend must
            // see the same snapshot; consuming it inside the first backend left later GPUs blank.
            let repaint_requested = data.state.repaint_requested;
            data.state.repaint_requested = false;
            for backend in &repaint_backends {
                if let Err(error) = backend.borrow_mut().render(data, repaint_requested) {
                    tracing::error!(%error, "direct-backend repaint failed");
                    data.state.request_repaint();
                }
            }
            TimeoutAction::ToDuration(FRAME_INTERVAL)
        })
        .map_err(|_| anyhow!("cannot register DRM repaint timer"))?;

    // Child applications launched from config must connect to this compositor, never to a stale
    // display inherited from the login environment.
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &data.state.socket_name);
        std::env::remove_var("DISPLAY");
    }
    Ok(())
}

fn create_backend(
    session: &mut LibSeatSession,
    path: &Path,
    primary_render_node: DrmNode,
    x_offset: i32,
    data: &mut CalloopData,
) -> Result<(DirectBackend, DrmDeviceNotifier)> {
    let fd = session
        .open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .map_err(|error| anyhow!("libseat cannot open {}: {error}", path.display()))?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));
    let (drm, drm_notifier) = DrmDevice::new(fd.clone(), true)
        .map_err(|error| anyhow!("cannot initialize DRM: {error}"))?;
    let gbm = GbmDevice::new(fd).context("cannot initialize GBM")?;

    let mut gpus = GpuManager::new(
        GbmGlesBackend::<GlesRenderer, DrmDeviceFd>::with_context_priority(ContextPriority::High),
    )
    .context("cannot create GPU manager")?;
    let display =
        unsafe { EGLDisplay::new(gbm.clone()) }.context("cannot initialize EGL display")?;
    let egl_device = EGLDevice::device_for_display(&display).context("cannot query EGL device")?;
    let render_node = egl_device
        .try_get_render_node()
        .ok()
        .flatten()
        .unwrap_or(primary_render_node);
    gpus.as_mut()
        .add_node(render_node, gbm.clone())
        .context("cannot add GPU renderer")?;

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let exporter = GbmFramebufferExporter::new(gbm.clone(), Some(render_node));
    let render_formats = gpus
        .single_renderer(&render_node)
        .context("cannot access EGL renderer")?
        .as_mut()
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .filter(|format| format.modifier != Modifier::Invalid)
        .copied()
        .collect::<FormatSet>();
    let mut output_manager = DrmOutputManager::new(
        drm,
        allocator,
        exporter,
        Some(gbm),
        SCANOUT_FORMATS.iter().copied(),
        render_formats,
    );

    let mut renderer = gpus
        .single_renderer(&render_node)
        .context("cannot acquire renderer for KMS output")?;
    let connectors = connected_outputs(output_manager.device())?;
    let mut surfaces = Vec::with_capacity(connectors.len());
    let mut x = x_offset;
    for (connector, crtc) in connectors {
        let drm_mode = select_drm_mode(connector.modes())?;
        let mode = Mode::from(drm_mode);
        let name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );
        let (physical_width, physical_height) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (physical_width as i32, physical_height as i32).into(),
                subpixel: connector.subpixel().into(),
                make: "unknown".into(),
                model: "DRM".into(),
            },
        );
        output.set_preferred(mode);
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            None,
            Some((x, 0).into()),
        );
        let global = output.create_global::<crate::Anvil>(&data.display_handle);
        data.state.space.map_output(&output, (x, 0));
        data.state.configure_output(
            &output.name(),
            anvil::layout::Rect::new(x, 0, mode.size.w, mode.size.h),
        );
        let drm_output =
            output_manager
                .initialize_output::<_, DirectRenderElement<
                    DirectRenderer<'_>,
                    WaylandSurfaceRenderElement<DirectRenderer<'_>>,
                >>(
                    crtc,
                    drm_mode,
                    &[connector.handle()],
                    &output,
                    None,
                    &mut renderer,
                    &DrmOutputRenderElements::default(),
                )
                .map_err(|error| anyhow!("cannot initialize KMS output {name}: {error}"))?;
        surfaces.push(SurfaceData {
            crtc,
            connector: connector.handle(),
            output,
            _global: global,
            drm_output,
            frame_pending: false,
            #[cfg(feature = "bar")]
            bar: BarRenderer::new(&data.state.config.bar)?,
            #[cfg(all(feature = "launcher", not(feature = "bar")))]
            launcher: BarRenderer::new(&data.state.config.bar)?,
        });
        x += mode.size.w;
    }
    drop(renderer);

    let border_color = parse_hex_color(&data.state.config.appearance.focus_border_color)
        .expect("configuration was validated before backend initialization");
    Ok((
        DirectBackend {
            _session: session.clone(),
            render_node,
            gpus,
            output_manager,
            surfaces,
            border: FocusBorder::new(border_color),
            pointer: PointerMarker::new()?,
            active: true,
        },
        drm_notifier,
    ))
}

/// Chooses the configured KMS mode or falls back to the connector's advertised preference.
///
/// DRM mode names alone do not identify refresh-rate variants, so selection compares the actual
/// pixel dimensions and integer vertical refresh reported by the kernel. Listing every available
/// mode in the error turns a typo or unsupported VM resolution into an actionable startup message.
fn select_drm_mode(modes: &[DrmMode]) -> Result<DrmMode> {
    modes
        .iter()
        .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| modes.first())
        .copied()
        .context("connected DRM output exposes no modes")
}

/// Assigns every connected desktop connector a distinct compatible CRTC.
///
/// Existing firmware assignments are preferred, then the encoder mask supplies the first unused
/// controller. Tracking used CRTCs is essential: initializing two connectors on the same CRTC
/// would make the second monitor fail even though the GPU has enough display pipelines.
fn connected_outputs(
    device: &impl smithay::reexports::drm::control::Device,
) -> Result<Vec<(connector::Info, crtc::Handle)>> {
    let resources = device
        .resource_handles()
        .context("cannot query DRM resources")?;
    let mut outputs = Vec::new();
    let mut used = Vec::new();
    for handle in resources.connectors() {
        let connector = device
            .get_connector(*handle, true)
            .context("cannot query DRM connector")?;
        if connector.state() != connector::State::Connected || connector.modes().is_empty() {
            continue;
        }

        let current = connector
            .current_encoder()
            .and_then(|encoder| device.get_encoder(encoder).ok())
            .and_then(|encoder| encoder.crtc())
            .filter(|crtc| !used.contains(crtc));
        let fallback = connector.encoders().iter().find_map(|encoder| {
            let encoder = device.get_encoder(*encoder).ok()?;
            resources
                .filter_crtcs(encoder.possible_crtcs())
                .into_iter()
                .find(|crtc| !used.contains(crtc))
        });
        if let Some(crtc) = current.or(fallback) {
            used.push(crtc);
            outputs.push((connector, crtc));
        }
    }
    if outputs.is_empty() {
        bail!("no connected DRM output with a compatible CRTC");
    }
    Ok(outputs)
}
