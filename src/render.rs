//! Renderer-independent elements shared by nested and direct display backends.
//!
//! Both Winit and DRM ultimately feed Smithay render elements into a renderer. Keeping Anvil's
//! focus-border construction here prevents the two backends from acquiring subtly different
//! window-manager visuals while still allowing each backend to own its presentation lifecycle.

use std::{env, fs};

use anyhow::{Context, Result, anyhow};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportMem, Renderer,
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                solid::{SolidColorBuffer, SolidColorRenderElement},
            },
        },
    },
    utils::{Logical, Point, Rectangle, Transform},
};
use xcursor::{CursorTheme, parser::parse_xcursor};

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

/// A system-theme cursor used by the direct DRM backend.
///
/// Winit supplies a host cursor, but a compositor that owns KMS has no window system underneath it.
/// XCursor is the standard Linux mechanism for resolving `left_ptr`, including theme inheritance,
/// image size and the exact hotspot. Keeping the decoded pixels in one persistent memory buffer
/// gives a smooth antialiased cursor without requiring a hardware cursor plane.
pub struct PointerMarker {
    buffer: MemoryRenderBuffer,
    hotspot: (i32, i32),
}

impl PointerMarker {
    pub fn new() -> Result<Self> {
        let requested_size = env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(24);
        let configured_theme = env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        // `xcursor-themes` commonly provides whiteglass without making it the global default.
        // Trying it after the configured/default themes gives minimal systems a conventional arrow
        // while still respecting every explicit user choice.
        let path = [
            configured_theme.as_str(),
            "default",
            "Adwaita",
            "whiteglass",
        ]
        .into_iter()
        .find_map(|theme| CursorTheme::load(theme).load_icon("left_ptr"))
        .ok_or_else(|| {
            anyhow!("no XCursor 'left_ptr' found; install a cursor theme or set XCURSOR_THEME")
        })?;
        let bytes = fs::read(&path)
            .with_context(|| format!("cannot read cursor image {}", path.display()))?;
        let images = parse_xcursor(&bytes)
            .ok_or_else(|| anyhow!("cannot parse XCursor image {}", path.display()))?;
        let image = images
            .into_iter()
            .min_by_key(|image| image.size.abs_diff(requested_size))
            .ok_or_else(|| anyhow!("XCursor image {} has no frames", path.display()))?;
        let buffer = MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Abgr8888,
            (image.width as i32, image.height as i32),
            1,
            Transform::Normal,
            None,
        );
        Ok(Self {
            buffer,
            hotspot: (image.xhot as i32, image.yhot as i32),
        })
    }

    pub fn element<R>(
        &self,
        renderer: &mut R,
        location: Point<f64, Logical>,
    ) -> std::result::Result<MemoryRenderBufferRenderElement<R>, R::Error>
    where
        R: Renderer + ImportMem,
        R::TextureId: Send + Clone + 'static,
    {
        let position = (
            location.x.round() - f64::from(self.hotspot.0),
            location.y.round() - f64::from(self.hotspot.1),
        );
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            position,
            &self.buffer,
            None,
            None,
            None,
            Kind::Cursor,
        )
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
