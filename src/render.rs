//! Renderer-independent elements shared by nested and direct display backends.
//!
//! Both Winit and DRM ultimately feed Smithay render elements into a renderer. Keeping Anvil's
//! focus-border construction here prevents the two backends from acquiring subtly different
//! window-manager visuals while still allowing each backend to own its presentation lifecycle.

use smithay::{
    backend::renderer::element::{
        Kind,
        solid::{SolidColorBuffer, SolidColorRenderElement},
    },
    utils::{Logical, Point, Rectangle},
};

/// Four persistent solid-color buffers forming the focus ring.
///
/// Keeping their renderer IDs stable allows Smithay's damage tracking to recognize unchanged
/// borders between frames. Creating new buffers on every repaint would turn an otherwise static
/// ring into permanent damage and waste both GPU work and display bandwidth.
pub struct FocusBorder {
    top: SolidColorBuffer,
    bottom: SolidColorBuffer,
    left: SolidColorBuffer,
    right: SolidColorBuffer,
    color: [f32; 4],
}

/// A tiny compositor-drawn pointer used by the direct DRM backend.
///
/// Winit supplies a host cursor, but a compositor that owns KMS has no window system underneath it.
/// Two persistent strips form a high-contrast L-shaped marker without embedding an image asset or
/// requiring hardware-cursor-plane support from every DRM driver.
pub struct PointerMarker {
    vertical: SolidColorBuffer,
    horizontal: SolidColorBuffer,
}

impl PointerMarker {
    pub fn new() -> Self {
        let color = [0.85, 0.85, 0.85, 1.0];
        Self {
            vertical: SolidColorBuffer::new((2, 14), color),
            horizontal: SolidColorBuffer::new((6, 2), color),
        }
    }

    pub fn elements(&self, location: Point<f64, Logical>) -> [SolidColorRenderElement; 2] {
        let x = location.x.round() as i32;
        let y = location.y.round() as i32;
        [
            SolidColorRenderElement::from_buffer(&self.vertical, (x, y), 1.0, 1.0, Kind::Cursor),
            SolidColorRenderElement::from_buffer(
                &self.horizontal,
                (x + 2, y),
                1.0,
                1.0,
                Kind::Cursor,
            ),
        ]
    }
}

impl FocusBorder {
    pub fn new(color: [f32; 4]) -> Self {
        Self {
            top: SolidColorBuffer::new((1, 1), color),
            bottom: SolidColorBuffer::new((1, 1), color),
            left: SolidColorBuffer::new((1, 1), color),
            right: SolidColorBuffer::new((1, 1), color),
            color,
        }
    }

    /// Builds a border immediately outside the focused client's geometry.
    ///
    /// The client owns every pixel inside `geometry`: terminals commonly place their first glyph
    /// very close to that edge, so an inset border would obscure useful content. Anvil therefore
    /// spends a small part of the configured gap on the focus ring. The horizontal strips include
    /// the corner pixels, while the vertical strips cover only the exact client height; together
    /// they form one continuous rectangle without ever entering the client area.
    pub fn elements(
        &mut self,
        geometry: Option<Rectangle<i32, Logical>>,
        configured_width: i32,
    ) -> Vec<SolidColorRenderElement> {
        let Some(geometry) = geometry else {
            return Vec::new();
        };
        let thickness = configured_width;
        if thickness <= 0 {
            return Vec::new();
        }

        let horizontal_size = (geometry.size.w + thickness * 2, thickness);
        let vertical_size = (thickness, geometry.size.h);
        self.top.update(horizontal_size, self.color);
        self.bottom.update(horizontal_size, self.color);
        self.left.update(vertical_size, self.color);
        self.right.update(vertical_size, self.color);

        let x = geometry.loc.x;
        let y = geometry.loc.y;
        let outer_x = x - thickness;
        let outer_y = y - thickness;
        let right = x + geometry.size.w;
        let bottom = y + geometry.size.h;
        vec![
            SolidColorRenderElement::from_buffer(
                &self.top,
                (outer_x, outer_y),
                1.0,
                1.0,
                Kind::Unspecified,
            ),
            SolidColorRenderElement::from_buffer(
                &self.bottom,
                (outer_x, bottom),
                1.0,
                1.0,
                Kind::Unspecified,
            ),
            SolidColorRenderElement::from_buffer(
                &self.left,
                (outer_x, y),
                1.0,
                1.0,
                Kind::Unspecified,
            ),
            SolidColorRenderElement::from_buffer(
                &self.right,
                (right, y),
                1.0,
                1.0,
                Kind::Unspecified,
            ),
        ]
    }
}
