//! Nested window and renderer backend used by the current development milestone.
//!
//! Smithay's Winit backend places the compositor inside one host window and translates host input
//! into Smithay events. That makes protocol and window-management work testable without taking
//! over a TTY. The state and handler layers stay reusable when a DRM/libinput backend is added.

use std::{cell::RefCell, rc::Rc, time::Duration};

use smithay::{
    backend::{
        egl::EGLDevice,
        renderer::{
            ImportAll, ImportDma, ImportMem,
            damage::OutputDamageTracker,
            element::{
                Kind,
                memory::MemoryRenderBufferRenderElement,
                solid::SolidColorRenderElement,
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
            },
            gles::GlesRenderer,
        },
        winit::{self, WinitEvent},
    },
    desktop::{Space, Window, utils::send_frames_surface_tree},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::EventLoop,
    utils::{Rectangle, Transform},
    wayland::dmabuf::DmabufFeedbackBuilder,
};

use anvil::config::parse_hex_color;

#[cfg(any(feature = "bar", feature = "launcher"))]
use crate::bar::BarRenderer;
use crate::{Anvil, CalloopData, render::FocusBorder};

smithay::backend::renderer::element::render_elements! {
    /// Compositor-owned overlays used by the nested development backend.
    WinitOverlay<R> where R: ImportAll + ImportMem;
    Solid=SolidColorRenderElement,
    Texture=MemoryRenderBufferRenderElement<R>,
    Surface=WaylandSurfaceRenderElement<R>,
}

pub fn init(
    event_loop: &mut EventLoop<CalloopData>,
    data: &mut CalloopData,
) -> Result<(), Box<dyn std::error::Error>> {
    // The backend owns the EGL surface/framebuffer; the event source owns Winit's host event loop.
    // Naming the renderer type explicitly is necessary because the custom border elements no
    // longer mention `GlesRenderer` in `render_output`'s generic arguments.
    let (mut backend, winit) = winit::init::<GlesRenderer>()?;
    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };
    // Wayland clients require an advertised output to choose scale, size and refresh behavior.
    // Physical dimensions are unknown for a nested window, so leave them at zero/unknown.
    let output = Output::new(
        "winit".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "anvil".into(),
            model: "nested".into(),
        },
    );
    // Keep the global alive for the life of this closure/function state; dropping it would remove
    // the output from the registry for newly connecting clients.
    let _global = output.create_global::<Anvil>(&data.display_handle);
    // Winit's framebuffer orientation differs from Wayland's logical orientation in this backend;
    // Flipped180 is the transform used by Smithay's reference nested compositor.
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    data.state.space.map_output(&output, (0, 0));
    data.state.set_output_size(mode.size.w, mode.size.h);

    // Nested mode uses the same protocol path as DRM mode. Query the host EGL renderer instead of
    // assuming the direct backend's formats; laptop hybrid graphics can expose a different set.
    let dmabuf_formats = backend.renderer().dmabuf_formats();
    let render_node = EGLDevice::device_for_display(backend.renderer().egl_context().display())
        .and_then(|device| device.try_get_render_node())
        .ok()
        .flatten();
    if let Some(render_node) = render_node {
        let feedback = DmabufFeedbackBuilder::new(render_node.dev_id(), dmabuf_formats)
            .build()
            .map_err(|error| format!("cannot build nested DMA-BUF feedback: {error}"))?;
        data.state
            .dmabuf_state
            .create_global_with_default_feedback::<Anvil>(&data.display_handle, &feedback);
    } else {
        // EGL implementations without device-query extensions cannot name a main device for v4
        // feedback. Version 3 still advertises every renderer-supported format and modifier.
        data.state
            .dmabuf_state
            .create_global::<Anvil>(&data.display_handle, dmabuf_formats);
    }

    let backend = Rc::new(RefCell::new(backend));
    let importer_backend = Rc::downgrade(&backend);
    data.state.dmabuf_importer = Some(Box::new(move |dmabuf| {
        importer_backend.upgrade().is_some_and(|backend| {
            backend
                .borrow_mut()
                .renderer()
                .import_dmabuf(dmabuf, None)
                .is_ok()
        })
    }));

    // Damage tracking lets the renderer submit only changed regions instead of repainting blindly.
    let mut damage_tracker = OutputDamageTracker::from_output(&output);
    let border_color = parse_hex_color(&data.state.config.appearance.focus_border_color)
        .expect("configuration was validated before backend initialization");
    let mut focus_border = FocusBorder::new(border_color);
    #[cfg(feature = "bar")]
    let mut bar = BarRenderer::new(&data.state.config.bar)?;
    #[cfg(all(feature = "launcher", not(feature = "bar")))]
    let mut launcher = BarRenderer::new(&data.state.config.bar)?;
    // SAFETY: this happens before commands or clients are spawned and the compositor owns the process.
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &data.state.socket_name);
    }

    event_loop
        .handle()
        .insert_source(winit, move |event, _, data| {
            let state = &mut data.state;
            let mut backend = backend.borrow_mut();
            match event {
                WinitEvent::Resized { size, .. } => {
                    // Update both the protocol-visible output mode and our tiling area. Clients then
                    // receive new configure sizes during `set_output_size` -> `arrange`.
                    output.change_current_state(
                        Some(Mode {
                            size,
                            refresh: 60_000,
                        }),
                        None,
                        None,
                        None,
                    );
                    state.set_output_size(size.w, size.h);
                }
                WinitEvent::Input(event) => state.process_input_event(event),
                WinitEvent::Redraw => {
                    // Rendering is output-centric: collect surfaces mapped in `Space`, composite
                    // them into the current framebuffer, then submit the damaged region.
                    let size = backend.window_size();
                    let damage = Rectangle::from_size(size);
                    let locked = state.session_locked();
                    let solid_elements = if locked {
                        Vec::new()
                    } else {
                        focus_border.elements(
                            state.focused_window_geometry(),
                            state.config.appearance.focus_border_width,
                            1.0,
                        )
                    };
                    let lock_surface = state.session_lock_surface(&output).cloned();
                    {
                        let (renderer, mut framebuffer) = backend.bind().unwrap();
                        // The bar feature appends its texture below; without that optional feature
                        // the vector remains immutable after collection.
                        #[allow(unused_mut)]
                        let mut overlay_elements = solid_elements
                            .into_iter()
                            .map(WinitOverlay::Solid)
                            .collect::<Vec<_>>();
                        #[cfg(feature = "bar")]
                        if !locked {
                            let config = state.config.bar.clone();
                            let snapshot = state.bar_snapshot("winit");
                            let area = state
                                .outputs
                                .iter()
                                .find(|candidate| candidate.name == "winit")
                                .expect("nested output state missing")
                                .screen_area;
                            overlay_elements.push(WinitOverlay::Texture(
                                bar.element(
                                    renderer,
                                    area.width,
                                    &config,
                                    &snapshot,
                                )
                                .unwrap(),
                            ));
                        }
                        #[cfg(all(feature = "launcher", not(feature = "bar")))]
                        if !locked {
                            if let Some(snapshot) = state.launcher_snapshot() {
                            let area = state
                                .outputs
                                .iter()
                                .find(|candidate| candidate.name == "winit")
                                .expect("nested output state missing")
                                .screen_area;
                                overlay_elements.push(WinitOverlay::Texture(
                                    launcher
                                        .launcher_element(
                                            renderer,
                                            area.width,
                                            area.height,
                                            &state.config.bar,
                                            &snapshot,
                                        )
                                        .unwrap(),
                                ));
                            }
                        }
                        if let Some(lock_surface) = lock_surface.as_ref() {
                            overlay_elements.extend(render_elements_from_surface_tree(
                                renderer,
                                lock_surface.wl_surface(),
                                (0, 0),
                                1.0,
                                1.0,
                                Kind::Unspecified,
                            ));
                        }
                        let spaces: Vec<&Space<Window>> =
                            if locked { Vec::new() } else { vec![&state.space] };
                        let background = if locked {
                            [0.0, 0.0, 0.0, 1.0]
                        } else {
                            state.config.appearance.background
                        };
                        smithay::desktop::space::render_output::<_, WinitOverlay<GlesRenderer>, _, _>(
                            &output,
                            renderer,
                            &mut framebuffer,
                            1.0,
                            0,
                            spaces,
                            &overlay_elements,
                            &mut damage_tracker,
                            background,
                        )
                        .unwrap();
                    }
                    backend.submit(Some(&[damage])).unwrap();
                    // A frame callback tells each client it may produce its next buffer. Without
                    // these callbacks animated or newly exposed clients would eventually stall.
                    if locked {
                        if let Some(lock_surface) = lock_surface {
                            send_frames_surface_tree(
                                lock_surface.wl_surface(),
                                &output,
                                state.start_time.elapsed(),
                                Some(Duration::ZERO),
                                |_, _| Some(output.clone()),
                            );
                        }
                        state.mark_session_lock_output_secured(&output.name());
                    } else {
                        state.space.elements().for_each(|window| {
                            window.send_frame(
                                &output,
                                state.start_time.elapsed(),
                                Some(Duration::ZERO),
                                |_, _| Some(output.clone()),
                            )
                        });
                    }
                    // Refresh cached surface state, discard dead popups and push queued protocol
                    // events before requesting the next host redraw.
                    state.space.refresh();
                    state.popups.cleanup();
                    let _ = data.display_handle.flush_clients();
                    backend.window().request_redraw();
                }
                // Closing the host window is equivalent to the compositor quit binding.
                WinitEvent::CloseRequested => state.loop_signal.stop(),
                _ => {}
            }
        })?;
    Ok(())
}
