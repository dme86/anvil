//! Window overview layout and surface rendering, independent of tag visibility.
//!
//! Anvil keeps inactive windows in `Anvil::windows` but unmaps them from Smithay's Space.
//! The overview renders each `Window` directly, so it never needs to switch tags, move
//! clients or alter the compositor's ordinary tiling geometry.

use anvil::layout::Rect;
use smithay::{
    backend::renderer::{
        ImportAll, Renderer,
        element::{
            surface::WaylandSurfaceRenderElement,
            utils::{
                ConstrainAlign, ConstrainScaleBehavior, CropRenderElement, RelocateRenderElement,
                RescaleRenderElement, constrain_as_render_elements,
            },
        },
    },
    desktop::Window,
    utils::{Physical, Point, Rectangle},
};

/// One selectable identity and its local (focused-output) overview layout rectangles.
#[derive(Clone)]
pub struct OverviewTile {
    pub window: Window,
    pub title: String,
    pub app_id: String,
    pub output: String,
    pub tags: u16,
    pub selected: bool,
    pub card: Rect,
    pub preview: Rect,
}

pub struct OverviewSnapshot {
    pub tiles: Vec<OverviewTile>,
}

/// Columns shared by layout and keyboard navigation so movements match the visual grid.
pub fn columns_for(width: i32, height: i32, n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    ((n as f64 * width.max(1) as f64 / height.max(1) as f64)
        .sqrt()
        .ceil() as usize)
        .clamp(1, n)
}

/// The complete session is shown on one output: no window is collapsed by app_id.
impl OverviewSnapshot {
    pub fn new(width: i32, height: i32, mut tiles: Vec<OverviewTile>) -> Self {
        let n = tiles.len();
        if n == 0 {
            return Self { tiles };
        }
        // The row/column choice is aspect-ratio aware and preserves vector/window order.
        let width = width.max(1);
        let height = height.max(1);
        let columns = columns_for(width, height, n);
        let rows = n.div_ceil(columns);
        let gap = 16;
        let top = 48.min(height / 4);
        let available_width = (width - gap * (columns as i32 + 1)).max(columns as i32);
        let available_height = (height - top - gap * (rows as i32 + 1)).max(rows as i32);
        let cell_width = (available_width / columns as i32).max(1);
        let cell_height = (available_height / rows as i32).max(1);
        for (index, tile) in tiles.iter_mut().enumerate() {
            let col = index % columns;
            let row = index / columns;
            let x = gap + col as i32 * (cell_width + gap);
            let y = top + gap + row as i32 * (cell_height + gap);
            let header = 46.min((cell_height / 3).max(0));
            tile.card = Rect::new(x, y, cell_width, cell_height);
            tile.preview = Rect::new(
                x + 2,
                y + header,
                (cell_width - 4).max(1),
                (cell_height - header - 2).max(1),
            );
        }
        Self { tiles }
    }
}

/// A Wayland/XWayland window scaled and clipped into one overview preview rectangle.
pub type OverviewPreview<R> =
    CropRenderElement<RelocateRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<R>>>>;

pub fn render_previews<R>(
    renderer: &mut R,
    snapshot: &OverviewSnapshot,
    scale: f64,
) -> Vec<OverviewPreview<R>>
where
    R: Renderer + ImportAll,
    R::TextureId: Clone + 'static,
{
    let mut elements = Vec::new();
    for tile in &snapshot.tiles {
        let geometry = tile.window.geometry();
        if geometry.size.w <= 0 || geometry.size.h <= 0 {
            continue;
        }
        let loc = Point::<i32, Physical>::from((
            (tile.preview.x as f64 * scale).round() as i32,
            (tile.preview.y as f64 * scale).round() as i32,
        ));
        let size = (
            (tile.preview.width as f64 * scale).round().max(1.0) as i32,
            (tile.preview.height as f64 * scale).round().max(1.0) as i32,
        );
        let frame = Rectangle::new(loc, size.into());
        let reference = Rectangle::from_size(
            (
                (geometry.size.w as f64 * scale).round().max(1.0) as i32,
                (geometry.size.h as f64 * scale).round().max(1.0) as i32,
            )
                .into(),
        );
        elements.extend(constrain_as_render_elements(
            &tile.window,
            renderer,
            loc,
            1.0,
            frame,
            reference,
            ConstrainScaleBehavior::Fit,
            ConstrainAlign::CENTER,
            scale,
        ));
    }
    elements
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grid_columns_match_visual_order() {
        assert_eq!(columns_for(1920, 1080, 0), 1);
        assert_eq!(columns_for(1920, 1080, 2), 2);
        assert_eq!(columns_for(900, 1200, 6), 3);
        assert_eq!(columns_for(1920, 1080, 1), 1);
    }

    #[test]
    fn empty_overview_is_valid() {
        let snap = OverviewSnapshot::new(1920, 1080, Vec::new());
        assert!(snap.tiles.is_empty());
    }
}
