//! Nested window and renderer backend used by the current development milestone.
//!
//! Smithay's Winit backend places the compositor inside one host window and translates host input
//! into Smithay events. That makes protocol and window-management work testable without taking
//! over a TTY. The state and handler layers stay reusable when a DRM/libinput backend is added.

use std::time::Duration;

use smithay::{
    backend::{
        renderer::{
            ImportMem,
            damage::OutputDamageTracker,
            element::{memory::MemoryRenderBufferRenderElement, solid::SolidColorRenderElement},
            gles::GlesRenderer,
        },
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::EventLoop,
    utils::{Rectangle, Transform},
};

use anvil::config::parse_hex_color;

#[cfg(any(feature = "bar", feature = "launcher"))]
use crate::bar::BarRenderer;
use crate::{Anvil, CalloopData, render::FocusBorder};

smithay::backend::renderer::element::render_elements! {
    /// Compositor-owned overlays used by the nested development backend.
    WinitOverlay<R> where R: ImportMem;
    Solid=SolidColorRenderElement,
    Texture=MemoryRenderBufferRenderElement<R>,
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
                    let solid_elements = focus_border.elements(
                        state.focused_window_geometry(),
                        state.config.appearance.focus_border_width,
                    );
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
                        {
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
                        smithay::desktop::space::render_output::<_, WinitOverlay<GlesRenderer>, _, _>(
                            &output,
                            renderer,
                            &mut framebuffer,
                            1.0,
                            0,
                            [&state.space],
                            &overlay_elements,
                            &mut damage_tracker,
                            state.config.appearance.background,
                        )
                        .unwrap();
                    }
                    backend.submit(Some(&[damage])).unwrap();
                    // A frame callback tells each client it may produce its next buffer. Without
                    // these callbacks animated or newly exposed clients would eventually stall.
                    state.space.elements().for_each(|window| {
                        window.send_frame(
                            &output,
                            state.start_time.elapsed(),
                            Some(Duration::ZERO),
                            |_, _| Some(output.clone()),
                        )
                    });
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
