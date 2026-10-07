//! Optional external panels use Smithay's per-output layer maps for geometry and stacking.
use std::time::Duration;

use crate::Anvil;
use anvil::layout::Rect;
use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    delegate_layer_shell,
    desktop::{LayerSurface as DesktopLayer, PopupKind, WindowSurfaceType, layer_map_for_output},
    output::Output,
    reexports::wayland_server::{
        Resource,
        protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    },
    utils::{Logical, Point, SERIAL_COUNTER, Serial},
    wayland::{
        compositor::with_states,
        shell::{
            wlr_layer::{
                KeyboardInteractivity, Layer, LayerSurface, LayerSurfaceData, WlrLayerShellHandler,
                WlrLayerShellState,
            },
            xdg::PopupSurface,
        },
    },
};

impl WlrLayerShellHandler for Anvil {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: LayerSurface,
        requested: Option<WlOutput>,
        _: Layer,
        namespace: String,
    ) {
        let output = match requested {
            Some(resource) => Output::from_resource(&resource)
                .filter(|output| self.space.outputs().any(|known| known == output)),
            None => self
                .space
                .outputs()
                .find(|output| Some(output.name().as_str()) == self.focused_output.as_deref())
                .cloned()
                .or_else(|| self.space.outputs().next().cloned()),
        };
        if let Some(output) = output {
            self.layer_surfaces
                .push((DesktopLayer::new(surface, namespace), output));
        } else {
            surface.send_close();
        }
    }

    fn new_popup(&mut self, _: LayerSurface, popup: PopupSurface) {
        if let Err(error) = self.popups.track_popup(PopupKind::Xdg(popup)) {
            tracing::warn!(%error, "cannot track layer popup");
        }
    }

    fn layer_destroyed(&mut self, surface: LayerSurface) {
        let entries: Vec<_> = self
            .layer_surfaces
            .iter()
            .filter(|(layer, _)| layer.layer_surface() == &surface)
            .cloned()
            .collect();
        for (layer, output) in entries {
            layer_map_for_output(&output).unmap_layer(&layer);
        }
        self.layer_surfaces
            .retain(|(layer, _)| layer.layer_surface() != &surface);
        self.arrange_layers();
        self.refresh_layer_focus();
    }
}
delegate_layer_shell!(Anvil);

impl Anvil {
    pub(crate) fn layer_commit(&mut self, surface: &WlSurface) {
        let Some((layer, output)) = self
            .layer_surfaces
            .iter()
            .find(|(layer, _)| layer.wl_surface() == surface)
            .cloned()
        else {
            return;
        };
        let initial = with_states(surface, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });
        let buffered =
            with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false);
        if !initial || buffered {
            if let Err(error) = layer_map_for_output(&output).map_layer(&layer) {
                tracing::warn!(%error, "cannot map layer surface");
            }
            layer_map_for_output(&output).arrange();
            if !initial {
                layer.layer_surface().send_configure();
            }
        } else {
            layer_map_for_output(&output).unmap_layer(&layer);
        }
        self.arrange_layers();
        self.refresh_layer_focus();
    }

    pub(crate) fn arrange_layers(&mut self) {
        for output in self.space.outputs().cloned().collect::<Vec<_>>() {
            let mut map = layer_map_for_output(&output);
            map.cleanup();
            map.arrange();
            let zone = map.non_exclusive_zone();
            let Some(workspace) = self
                .outputs
                .iter_mut()
                .find(|workspace| workspace.name == output.name())
            else {
                continue;
            };
            let screen = workspace.screen_area;
            #[cfg(feature = "bar")]
            let bar_height = self
                .config
                .bar
                .height
                .min(screen.height.saturating_sub(1))
                .max(0);
            #[cfg(not(feature = "bar"))]
            let bar_height = 0;
            // Intersect the external exclusive zone with the independently reserved built-in bar.
            let left = zone.loc.x.max(0);
            let top = zone.loc.y.max(bar_height);
            let right = (zone.loc.x + zone.size.w).min(screen.width);
            let bottom = (zone.loc.y + zone.size.h).min(screen.height);
            workspace.output_area = Rect::new(
                screen.x + left,
                screen.y + top,
                (right - left).max(1),
                (bottom - top).max(1),
            );
        }
        self.arrange();
        self.request_repaint();
    }

    pub(crate) fn close_output_layers(&mut self, name: &str) {
        for (layer, output) in &self.layer_surfaces {
            if output.name() == name {
                layer.layer_surface().send_close();
                layer_map_for_output(output).unmap_layer(layer);
            }
        }
        self.layer_surfaces
            .retain(|(_, output)| output.name() != name);
    }

    pub(crate) fn layer_surface_under(
        &self,
        pos: Point<f64, Logical>,
        upper: bool,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        for output in self.space.outputs() {
            let origin = self.space.output_geometry(output)?.loc;
            let local = pos - origin.to_f64();
            let map = layer_map_for_output(output);
            let layers = if upper {
                [Layer::Overlay, Layer::Top]
            } else {
                [Layer::Bottom, Layer::Background]
            };
            for kind in layers {
                if let Some(layer) = map.layer_under(kind, local) {
                    let location = map.layer_geometry(layer)?.loc;
                    if let Some((surface, offset)) =
                        layer.surface_under(local - location.to_f64(), WindowSurfaceType::ALL)
                    {
                        return Some((surface, (origin + location + offset).to_f64()));
                    }
                }
            }
        }
        None
    }

    pub(crate) fn focus_layer_at(&mut self, pos: Point<f64, Logical>, serial: Serial) -> bool {
        let Some((surface, _)) = self.layer_surface_under(pos, true).or_else(|| {
            if self.space.element_under(pos).is_none() {
                self.layer_surface_under(pos, false)
            } else {
                None
            }
        }) else {
            return false;
        };
        let accepts = self.layer_surfaces.iter().any(|(layer, _)| {
            layer.can_receive_keyboard_focus()
                && (layer.wl_surface() == &surface || self.popups.find_popup(&surface).is_some())
        });
        if accepts {
            self.seat
                .get_keyboard()
                .unwrap()
                .set_focus(self, Some(surface), serial);
        }
        true
    }

    pub(crate) fn refresh_layer_focus(&mut self) {
        if self.session_locked() {
            return;
        }
        self.layer_surfaces
            .retain(|(layer, _)| layer.layer_surface().alive());
        let exclusive = self
            .layer_surfaces
            .iter()
            .rev()
            .find(|(layer, output)| {
                matches!(layer.layer(), Layer::Top | Layer::Overlay)
                    && layer.cached_state().keyboard_interactivity
                        == KeyboardInteractivity::Exclusive
                    && layer_map_for_output(output).layer_geometry(layer).is_some()
                    && with_renderer_surface_state(layer.wl_surface(), |state| {
                        state.buffer().is_some()
                    })
                    .unwrap_or(false)
            })
            .map(|(layer, _)| layer.wl_surface().clone());
        let keyboard = self.seat.get_keyboard().unwrap();
        if let Some(surface) = exclusive.clone() {
            if keyboard.current_focus().as_ref() != Some(&surface) {
                keyboard.set_focus(self, Some(surface), SERIAL_COUNTER.next_serial());
            }
        } else if self.exclusive_layer_focus.take().is_some()
            || keyboard
                .current_focus()
                .is_some_and(|surface| !surface.is_alive())
        {
            self.focus_index(0);
        }
        self.exclusive_layer_focus = exclusive;
    }

    pub(crate) fn send_layer_frames(&self, output: &Output) {
        for layer in layer_map_for_output(output).layers() {
            layer.send_frame(
                output,
                self.start_time.elapsed(),
                Some(Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        }
    }
}
