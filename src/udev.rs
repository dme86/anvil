//! Direct Linux backend using libseat, udev, DRM/KMS, GBM/EGL and libinput.
//!
//! Unlike the Winit development backend, this module owns a real seat and display controller. It
//! opens the DRM device through libseat (so VT switching remains safe), scans a connected output,
//! renders Anvil's existing `Space` into GBM buffers and submits them to KMS. libinput events feed
//! the same backend-independent input policy used by nested mode.

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
            DrmDevice, DrmDeviceFd, DrmDeviceNotifier, DrmEvent, DrmNode, NodeType,
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
        udev::{UdevBackend, all_gpus, primary_gpu},
    },
    desktop::{Window, space::SpaceRenderElements},
    output::{Mode, Output, PhysicalProperties},
    reexports::{
        calloop::{
            EventLoop,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{Mode as DrmMode, ModeTypeFlags, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Transform},
};

use anvil::config::{Output as OutputConfig, parse_hex_color};

#[cfg(feature = "bar")]
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
    Bar=MemoryRenderBufferRenderElement<R>,
}

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
type OutputManager = DrmOutputManager<Allocator, Exporter, (), DrmDeviceFd>;
type KmsOutput = DrmOutput<Allocator, Exporter, (), DrmDeviceFd>;

struct SurfaceData {
    crtc: crtc::Handle,
    output: Output,
    // Keeping the id alive documents ownership; it is removed automatically when the display dies.
    _global: GlobalId,
    drm_output: KmsOutput,
    frame_pending: bool,
}

struct DirectBackend {
    // The session owns the libseat device handles and restores the VT when Anvil exits.
    _session: LibSeatSession,
    render_node: DrmNode,
    gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    output_manager: OutputManager,
    surface: SurfaceData,
    border: FocusBorder,
    pointer: PointerMarker,
    #[cfg(feature = "bar")]
    bar: BarRenderer,
    active: bool,
}

impl DirectBackend {
    fn render(&mut self, data: &mut CalloopData) -> Result<()> {
        if !self.active || self.surface.frame_pending || !data.state.repaint_requested {
            return Ok(());
        }
        // Consume the coalesced request only when KMS is ready. If a vblank is still pending, the
        // bit remains set and the next scheduler tick retries without rebuilding render elements.
        data.state.repaint_requested = false;

        let mut overlay_elements = self
            .pointer
            .elements(data.state.seat.get_pointer().unwrap().current_location())
            .into_iter()
            .collect::<Vec<_>>();
        overlay_elements.extend(self.border.elements(
            data.state.focused_window_geometry(),
            data.state.config.appearance.focus_border_width,
        ));

        // Split the backend borrow so the renderer and output may be used together. The renderer
        // lives in GpuManager while DrmOutput owns the KMS swapchain; neither aliases the other.
        let DirectBackend {
            gpus,
            surface,
            render_node,
            #[cfg(feature = "bar")]
            bar,
            ..
        } = self;
        let mut renderer = gpus
            .single_renderer(render_node)
            .map_err(|error| anyhow!("cannot acquire DRM renderer: {error}"))?;

        let mut elements: Vec<
            DirectRenderElement<
                DirectRenderer<'_>,
                WaylandSurfaceRenderElement<DirectRenderer<'_>>,
            >,
        > = overlay_elements
            .into_iter()
            .map(DirectRenderElement::Border)
            .collect();
        #[cfg(feature = "bar")]
        {
            let config = data.state.config.bar.clone();
            let snapshot = data.state.bar_snapshot();
            let bar_element = bar
                .element(
                    &mut renderer,
                    data.state.screen_area.width,
                    &config,
                    &snapshot,
                )
                .map_err(|error| anyhow!("cannot upload bar texture: {error}"))?;
            elements.push(DirectRenderElement::Bar(bar_element));
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

        // An empty frame means damage tracking found nothing new. Queueing it would return an
        // EmptyFrame error, so the periodic repaint timer simply checks again on its next tick.
        if !frame.is_empty {
            surface
                .drm_output
                .queue_frame(())
                .map_err(|error| anyhow!("KMS frame submission failed: {error}"))?;
            surface.frame_pending = true;
        }

        // Frame callbacks allow clients to prepare their next buffer. They are sent after Anvil
        // has sampled the current surfaces, matching the presentation boundary used by Winit.
        data.state.space.elements().for_each(|window| {
            window.send_frame(
                &surface.output,
                data.state.start_time.elapsed(),
                Some(Duration::ZERO),
                |_, _| Some(surface.output.clone()),
            )
        });
        Ok(())
    }

    fn frame_submitted(&mut self, crtc: crtc::Handle) {
        if crtc != self.surface.crtc {
            return;
        }
        match self.surface.drm_output.frame_submitted() {
            Ok(_) => self.surface.frame_pending = false,
            Err(error) => tracing::warn!(%error, "failed to finish DRM frame"),
        }
    }

    fn pause(&mut self) {
        self.active = false;
        self.output_manager.pause();
        self.surface.frame_pending = false;
    }

    fn activate(&mut self) {
        if let Err(error) = self.output_manager.activate(false) {
            tracing::error!(%error, "failed to reactivate DRM device");
            return;
        }
        self.active = true;
        self.surface.frame_pending = false;
    }
}

pub fn init(event_loop: &mut EventLoop<CalloopData>, data: &mut CalloopData) -> Result<()> {
    let (mut session, session_notifier) = LibSeatSession::new()
        .map_err(|error| anyhow!("cannot acquire a libseat session: {error}"))?;
    let seat_name = session.seat();
    let udev = UdevBackend::new(&seat_name)
        .map_err(|error| anyhow!("cannot enumerate DRM devices on {seat_name}: {error}"))?;

    let primary_render_node = choose_primary_gpu(&seat_name)?;
    let primary_card = udev
        .device_list()
        .find(|(device_id, _)| {
            primary_render_node
                .node_with_type(NodeType::Primary)
                .and_then(Result::ok)
                .is_some_and(|node| node.dev_id() == *device_id)
                || primary_render_node.dev_id() == *device_id
        })
        .or_else(|| udev.device_list().next())
        .context("no DRM card found for the active seat")?;

    tracing::info!(path = %primary_card.1.display(), "opening DRM device");
    let (mut backend, drm_notifier) =
        create_backend(&mut session, primary_card.1, primary_render_node, data)?;

    // Advertise all software-buffer formats the actual renderer can import. Without this update,
    // wl_shm clients could choose a format that the GBM/EGL renderer cannot consume.
    let shm_formats = backend
        .gpus
        .single_renderer(&backend.render_node)
        .map_err(|error| anyhow!("cannot query renderer formats: {error}"))?
        .shm_formats();
    data.state.shm_state.update_formats(shm_formats);

    let backend = Rc::new(RefCell::new(backend));
    let drm_backend = backend.clone();
    event_loop
        .handle()
        .insert_source(drm_notifier, move |event, _, _data| match event {
            DrmEvent::VBlank(crtc) => drm_backend.borrow_mut().frame_submitted(crtc),
            DrmEvent::Error(error) => tracing::error!(%error, "DRM event error"),
        })
        .context("cannot register DRM event source")?;

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

    let session_backend = backend.clone();
    event_loop
        .handle()
        .insert_source(session_notifier, move |event, _, _data| match event {
            SessionEvent::PauseSession => {
                tracing::info!("pausing DRM/libinput session");
                libinput_context.suspend();
                session_backend.borrow_mut().pause();
            }
            SessionEvent::ActivateSession => {
                tracing::info!("resuming DRM/libinput session");
                if libinput_context.resume().is_err() {
                    tracing::error!("failed to resume libinput");
                }
                session_backend.borrow_mut().activate();
            }
        })
        .map_err(|_| anyhow!("cannot register libseat session source"))?;

    let repaint_backend = backend.clone();
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
            if let Err(error) = repaint_backend.borrow_mut().render(data) {
                tracing::error!(%error, "direct-backend repaint failed");
                data.state.request_repaint();
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

fn choose_primary_gpu(seat_name: &str) -> Result<DrmNode> {
    if let Some(node) = primary_gpu(seat_name)
        .map_err(|error| anyhow!("cannot query primary GPU: {error}"))?
        .and_then(|path| DrmNode::from_path(path).ok())
        .and_then(|node| node.node_with_type(NodeType::Render))
        .and_then(Result::ok)
    {
        return Ok(node);
    }

    all_gpus(seat_name)
        .map_err(|error| anyhow!("cannot enumerate GPUs: {error}"))?
        .into_iter()
        .find_map(|path| DrmNode::from_path(path).ok())
        .context("no usable GPU found")
}

fn create_backend(
    session: &mut LibSeatSession,
    path: &Path,
    primary_render_node: DrmNode,
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

    let (connector, crtc) = connected_output(output_manager.device())?;
    let drm_mode = select_drm_mode(connector.modes(), &data.state.config.output)?;
    let mode = Mode::from(drm_mode);
    let name = format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    );
    let (physical_width, physical_height) = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        name,
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
        Some((0, 0).into()),
    );
    let global = output.create_global::<crate::Anvil>(&data.display_handle);
    data.state.space.map_output(&output, (0, 0));
    data.state.set_output_size(mode.size.w, mode.size.h);

    let mut renderer = gpus
        .single_renderer(&render_node)
        .context("cannot acquire renderer for KMS output")?;
    let drm_output = output_manager
        .initialize_output::<
            _,
            DirectRenderElement<
                DirectRenderer<'_>,
                WaylandSurfaceRenderElement<DirectRenderer<'_>>,
            >,
        >(
            crtc,
            drm_mode,
            &[connector.handle()],
            &output,
            None,
            &mut renderer,
            &DrmOutputRenderElements::default(),
        )
        .map_err(|error| anyhow!("cannot initialize KMS output: {error}"))?;
    drop(renderer);

    let border_color = parse_hex_color(&data.state.config.appearance.focus_border_color)
        .expect("configuration was validated before backend initialization");
    Ok((
        DirectBackend {
            _session: session.clone(),
            render_node,
            gpus,
            output_manager,
            surface: SurfaceData {
                crtc,
                output,
                _global: global,
                drm_output,
                frame_pending: false,
            },
            border: FocusBorder::new(border_color),
            pointer: PointerMarker::new(),
            #[cfg(feature = "bar")]
            bar: BarRenderer::new(),
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
fn select_drm_mode(modes: &[DrmMode], config: &OutputConfig) -> Result<DrmMode> {
    if let (Some(width), Some(height)) = (config.width, config.height) {
        if let Some(mode) = modes.iter().find(|mode| {
            mode.size() == (width, height)
                && config
                    .refresh_rate
                    .is_none_or(|refresh| mode.vrefresh() == refresh)
        }) {
            return Ok(*mode);
        }

        let requested_refresh = config
            .refresh_rate
            .map(|refresh| format!("@{refresh}"))
            .unwrap_or_default();
        let available = modes
            .iter()
            .map(|mode| {
                let (width, height) = mode.size();
                format!("{width}x{height}@{}", mode.vrefresh())
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "requested output mode {width}x{height}{requested_refresh} is unavailable; available modes: {available}"
        );
    }

    modes
        .iter()
        .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| modes.first())
        .copied()
        .context("connected DRM output exposes no modes")
}

/// Selects the first connected desktop connector and a compatible CRTC.
///
/// Anvil currently exposes one logical output, so it does not need a persistent multi-connector
/// assignment solver. Reusing the firmware/display-manager CRTC is preferred because it avoids an
/// unnecessary modeset; otherwise the encoder's `possible_crtcs` mask supplies a valid fallback.
fn connected_output(
    device: &impl smithay::reexports::drm::control::Device,
) -> Result<(connector::Info, crtc::Handle)> {
    let resources = device
        .resource_handles()
        .context("cannot query DRM resources")?;
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
            .and_then(|encoder| encoder.crtc());
        let fallback = connector.encoders().iter().find_map(|encoder| {
            let encoder = device.get_encoder(*encoder).ok()?;
            resources
                .filter_crtcs(encoder.possible_crtcs())
                .into_iter()
                .next()
        });
        if let Some(crtc) = current.or(fallback) {
            return Ok((connector, crtc));
        }
    }
    bail!("no connected DRM output with a compatible CRTC")
}
