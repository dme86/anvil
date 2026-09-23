//! Pure geometry calculation for Anvil's dynamic tiling layout.
//!
//! This module intentionally knows nothing about Smithay windows. It receives only an output
//! rectangle, a client count, and layout settings, then returns one rectangle per client. Keeping
//! policy separate from Wayland mechanics makes edge cases deterministic and directly testable.

use crate::config::Layout;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
/// A small backend-independent logical-pixel rectangle.
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Computes dwm's classic master/stack layout in logical pixels.
///
/// Clients retain their order: the first `master_count` clients occupy the left column and all
/// remaining clients share the stack on the right. If there is no stack, masters use the complete
/// width. Returning geometry instead of mutating windows keeps this function easy to reason about.
pub fn tile(area: Rect, count: usize, config: &Layout) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }

    // Remove the outer gap once before splitting the output. Saturating/clamped arithmetic keeps
    // even absurdly small virtual outputs from generating zero or negative Wayland sizes.
    let outer = config.outer_gap.max(0);
    let gap = config.gap.max(0);
    let usable = Rect::new(
        area.x + outer,
        area.y + outer,
        (area.width - outer * 2).max(1),
        (area.height - outer * 2).max(1),
    );
    let masters = config.master_count.min(count);
    if count <= masters {
        // Without stack clients there is no reason to reserve a right-hand column.
        return vertical_column(usable, count, gap);
    }

    // The inter-column gap is removed before applying the factor. Consequently master + stack +
    // gap always reconstruct the usable width without overlap or an off-by-one sliver.
    let available_width = (usable.width - gap).max(2);
    let master_width = ((available_width as f64 * config.master_factor).round() as i32)
        .clamp(1, available_width - 1);
    let stack_width = available_width - master_width;

    let mut result = vertical_column(
        Rect::new(usable.x, usable.y, master_width, usable.height),
        masters,
        gap,
    );
    result.extend(vertical_column(
        Rect::new(
            usable.x + master_width + gap,
            usable.y,
            stack_width,
            usable.height,
        ),
        count - masters,
        gap,
    ));
    result
}

fn vertical_column(area: Rect, count: usize, gap: i32) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let total_gap = gap.saturating_mul(count.saturating_sub(1) as i32);
    let available = (area.height - total_gap).max(count as i32);
    let base = available / count as i32;
    let remainder = available % count as i32;
    let mut y = area.y;

    (0..count)
        .map(|index| {
            // Integer division can leave a few pixels. Assigning one remainder pixel to each of
            // the first clients fills the column exactly and avoids a visible strip at the bottom.
            let height = base + i32::from((index as i32) < remainder);
            let rect = Rect::new(area.x, y, area.width.max(1), height.max(1));
            y += height + gap;
            rect
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_and_stack_are_deterministic() {
        let config = Layout {
            gap: 10,
            outer_gap: 10,
            master_factor: 0.6,
            master_count: 1,
        };
        assert_eq!(
            tile(Rect::new(0, 0, 1000, 800), 3, &config),
            vec![
                Rect::new(10, 10, 582, 780),
                Rect::new(602, 10, 388, 385),
                Rect::new(602, 405, 388, 385),
            ]
        );
    }

    #[test]
    fn no_clients_produces_no_tiles() {
        assert!(tile(Rect::new(0, 0, 100, 100), 0, &Layout::default()).is_empty());
    }

    #[test]
    fn tiny_outputs_never_produce_negative_sizes() {
        let tiles = tile(Rect::new(0, 0, 4, 4), 8, &Layout::default());
        assert!(tiles.iter().all(|r| r.width > 0 && r.height > 0));
    }
}
