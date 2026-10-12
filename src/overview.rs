//! Window overview layout and surface rendering, independent of tag visibility.
//!
//! Inactive clients are not mapped into Smithay's Space. Instead, Anvil renders their
//! existing Window buffers directly. The layout does not change tags or configure clients.

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

/// Each managed window has exactly one card even when it belongs to multiple dwm tags.
/// Its lowest numbered tag determines the visual section; the card lists all its tags.
#[derive(Clone)]
pub struct OverviewTile {
    pub window: Window,
    pub title: String,
    pub app_id: String,
    pub output: String,
    pub tags: u16,
    pub selected: bool,
    pub page: usize,
    pub card: Rect,
    pub preview: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeadingKind {
    MonitorTag,
    Application,
}

pub struct OverviewHeading {
    pub title: String,
    pub area: Rect,
    pub kind: HeadingKind,
    pub page: usize,
}

pub struct OverviewSnapshot {
    /// Stable render/navigation order: output, tag, application, then original window order.
    pub tiles: Vec<OverviewTile>,
    pub headings: Vec<OverviewHeading>,
    pub page: usize,
    pub page_count: usize,
}

struct Entry<'a> {
    output: &'a str,
    app_id: &'a str,
    tags: u16,
}

struct Layout {
    order: Vec<usize>,
    cards: Vec<(usize, Rect)>,
    headings: Vec<OverviewHeading>,
    page_count: usize,
}

fn primary_tag(mask: u16) -> Option<usize> {
    (mask != 0).then(|| mask.trailing_zeros() as usize)
}

fn application(app: &str) -> &str {
    let app = app.trim();
    if app.is_empty() { "Unknown application" } else { app }
}

/// Compute a pure-data layout so grouping, pagination and bounds can be regression tested
/// without a renderer, an EGL context or real Wayland client surfaces.
fn arrange(width: i32, height: i32, entries: &[Entry<'_>], outputs: &[String]) -> Layout {
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by(|&a, &b| {
        let left = &entries[a];
        let right = &entries[b];
        let output_rank = |name: &str| outputs.iter().position(|output| output == name).unwrap_or(usize::MAX);
        output_rank(left.output).cmp(&output_rank(right.output))
            .then_with(|| left.output.cmp(right.output))
            .then_with(|| primary_tag(left.tags).cmp(&primary_tag(right.tags)))
            .then_with(|| application(left.app_id).to_lowercase().cmp(
                &application(right.app_id).to_lowercase()))
            .then_with(|| a.cmp(&b))
    });

    let width = width.max(1);
    let height = height.max(1);
    let margin = if width < 600 { 10 } else { 24 };
    let gap = 12;
    let top = (height / 10).clamp(24, 50);
    let columns = ((width - margin * 2 + gap) / (240 + gap)).clamp(1, 6);
    let card_width = ((width - margin * 2 - (columns - 1) * gap) / columns).max(1);
    let card_height = ((card_width * 3 / 5) + 44).clamp(110, 240)
        .min((height - top - margin - 30 - 24 - gap).max(1));
    let limit = (height - margin).max(1);
    let mut page = 0;
    let mut y = top;
    let mut column = 0;
    let mut previous_group: Option<(String, Option<usize>)> = None;
    let mut previous_app: Option<String> = None;
    let mut cards = Vec::with_capacity(order.len());
    let mut headings = Vec::new();

    for &idx in &order {
        let entry = &entries[idx];
        let group = (entry.output.to_owned(), primary_tag(entry.tags));
        let app = application(entry.app_id).to_owned();
        let different_group = previous_group.as_ref() != Some(&group);
        let different_app = different_group || previous_app.as_ref() != Some(&app);

        // Complete the previous row before changing groups or starting another row.
        if column > 0 && (column == columns || different_app) {
            y += card_height + gap;
            column = 0;
        }
        let mut group_header = different_group;
        let mut app_header = different_app;
        let required = card_height
            + if group_header { 32 } else { 0 }
            + if app_header { 24 } else { 0 };
        // Full-size cards win over endless shrinking. Additional pages retain every card and
        // repeat the relevant headings, while the selected window chooses the visible page.
        if y + required > limit && y > top {
            page += 1;
            y = top;
            column = 0;
            group_header = true;
            app_header = true;
        }
        if group_header {
            let tag = group.1.map_or_else(|| "Unassigned".to_owned(), |tag| format!("Tag {}", tag + 1));
            headings.push(OverviewHeading {
                title: format!("{}  ·  {}", group.0, tag),
                area: Rect::new(margin, y, (width - 2 * margin).max(1), 28),
                kind: HeadingKind::MonitorTag,
                page,
            });
            y += 32;
        }
        if app_header {
            headings.push(OverviewHeading {
                title: app.clone(),
                area: Rect::new(margin, y, (width - 2 * margin).max(1), 20),
                kind: HeadingKind::Application,
                page,
            });
            y += 24;
        }
        cards.push((
            page,
            Rect::new(margin + column * (card_width + gap), y, card_width, card_height),
        ));
        column += 1;
        previous_group = Some(group);
        previous_app = Some(app);
    }
    Layout {
        order, cards, headings,
        page_count: if entries.is_empty() { 1 } else { page + 1 },
    }
}

impl OverviewSnapshot {
    pub fn new(width: i32, height: i32, tiles: Vec<OverviewTile>, outputs: &[String]) -> Self {
        let entries = tiles.iter().map(|tile| Entry {
            output: &tile.output, app_id: &tile.app_id, tags: tile.tags,
        }).collect::<Vec<_>>();
        let layout = arrange(width, height, &entries, outputs);
        let mut pending = tiles.into_iter().map(Some).collect::<Vec<_>>();
        let mut ordered = Vec::with_capacity(pending.len());
        for (&index, &(page, card)) in layout.order.iter().zip(&layout.cards) {
            let mut tile = pending[index].take().expect("overview index is unique");
            tile.page = page;
            tile.card = card;
            let header = 46.min((card.height / 3).max(0));
            tile.preview = Rect::new(
                card.x + 2, card.y + header,
                (card.width - 4).max(1), (card.height - header - 2).max(1),
            );
            ordered.push(tile);
        }
        let page = ordered.iter().find(|tile| tile.selected).map_or(0, |tile| tile.page);
        Self {
            tiles: ordered,
            headings: layout.headings,
            page,
            page_count: layout.page_count,
        }
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
    for tile in snapshot.tiles.iter().filter(|tile| tile.page == snapshot.page) {
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
            ).into(),
        );
        elements.extend(constrain_as_render_elements(
            &tile.window, renderer, loc, 1.0, frame, reference,
            ConstrainScaleBehavior::Fit, ConstrainAlign::CENTER, scale,
        ));
    }
    elements
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry<'a>(output: &'a str, app_id: &'a str, tags: u16) -> Entry<'a> {
        Entry { output, app_id, tags }
    }

    #[test]
    fn grouping_is_monitor_tag_app_and_stable_within_application() {
        let entries = [
            entry("HDMI-A-1", "org.firefox", 2),
            entry("DP-1", "foot", 1),
            entry("DP-1", "firefox", 1),
            entry("DP-1", "firefox", 1),
            entry("DP-1", "foot", 4),
            entry("DP-1", "firefox", 0b11),
        ];
        let layout = arrange(1920, 1080, &entries, &["DP-1".into(), "HDMI-A-1".into()]);
        assert_eq!(layout.order, vec![2, 3, 5, 1, 4, 0]);
        assert_eq!(layout.cards.len(), entries.len());
        assert_eq!(layout.headings.iter().filter(|header|
            header.kind == HeadingKind::MonitorTag).count(), 3);
        assert!(layout.headings.iter().any(|header|
            header.title == "HDMI-A-1  ·  Tag 2"));
    }

    #[test]
    fn crowded_and_narrow_displays_paginate_without_overlapping_cards() {
        let entries = (0..65).map(|_| entry("DP-1", "firefox", 1)).collect::<Vec<_>>();
        for (w, h) in [(1920, 1080), (1024, 768), (500, 400)] {
            let layout = arrange(w, h, &entries, &["DP-1".into()]);
            assert!(layout.page_count > 1);
            assert_eq!(layout.cards.len(), entries.len());
            for (i, (page, rect)) in layout.cards.iter().enumerate() {
                assert!(rect.x >= 0 && rect.y >= 0 && rect.width > 0 && rect.height > 0);
                assert!(rect.x + rect.width <= w);
                assert!(rect.y + rect.height <= h);
                for (other_page, other) in layout.cards.iter().skip(i + 1) {
                    if other_page == page {
                        assert!(rect.x + rect.width <= other.x
                            || other.x + other.width <= rect.x
                            || rect.y + rect.height <= other.y
                            || other.y + other.height <= rect.y);
                    }
                }
            }
        }
    }

    #[test]
    fn unassigned_tags_and_missing_application_have_readable_labels() {
        let layout = arrange(1200, 800, &[entry("DP-2", "", 0)], &[]);
        assert!(layout.headings.iter().any(|header| header.title.contains("Unassigned")));
        assert!(layout.headings.iter().any(|header| header.title == "Unknown application"));
    }
}
