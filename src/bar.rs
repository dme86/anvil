//! Minimal dwm-style status bar compiled only with the default `bar` Cargo feature.
//!
//! Fontconfig resolves the configured system font once at startup. Glyphs are rasterized into one
//! compact texture, keeping the direct DRM session independent from a desktop environment.

// The launcher-only build reuses this module's font/texture renderer but intentionally leaves the
// bar snapshot, hit testing and status machinery dormant.
#![cfg_attr(not(feature = "bar"), allow(dead_code))]

use std::{
    fs,
    process::Command,
    time::{Duration, Instant},
};

#[cfg(feature = "launcher")]
use crate::launcher::LauncherSnapshot;
use anvil::config::{Bar as BarConfig, parse_named_hex_color};
use anyhow::{Context, Result, bail};
use fontdue::{Font, FontSettings};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportMem, Renderer,
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            },
        },
    },
    utils::{Rectangle, Transform},
};

pub struct BarSnapshot {
    pub selected_tags: u16,
    pub occupied_tags: u16,
    pub tag_count: usize,
    /// Active runtime layout mode rendered between tags and window titles.
    pub layout_symbol: &'static str,
    /// Window count for every tag, including tags that are currently hidden.
    ///
    /// Keeping counts in the snapshot leaves the renderer independent of compositor window types
    /// and makes the same information available to both nested and direct DRM backends.
    pub window_counts: Vec<usize>,
    /// One entry per visible toplevel. The renderer gives every entry an equal part of the title
    /// area so the bar's window count always agrees with the tiled/floating clients below it.
    pub windows: Vec<BarWindow>,
    pub status: String,
    #[cfg(feature = "launcher")]
    pub launcher: Option<LauncherSnapshot>,
}

pub struct BarWindow {
    pub title: String,
    pub focused: bool,
}

/// The compositor action associated with an interactive part of the bar.
///
/// Window indices refer to the visible order used by both the title strip and `focus_index`, so a
/// click cannot accidentally target a hidden window from another tag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BarHit {
    Tag(usize),
    LayoutMode,
    Window(usize),
}

/// Horizontal regions shared by painting and pointer hit testing.
///
/// Keeping this calculation in one place is important: status text changes its width over time,
/// which also changes every window title's slice. If rendering and input recomputed those bounds
/// differently, the visible label and its clickable area would drift apart.
struct BarLayout {
    tag_width: i32,
    tags_end: i32,
    mode_end: i32,
    status_x: i32,
}

// These are deliberately plain rectangles rather than font glyphs. Their appearance therefore
// remains stable when users select another system font or when a minimal installation has only a
// fallback font available.
const WINDOW_INDICATOR_WIDTH: i32 = 2;
const WINDOW_INDICATOR_HEIGHT: i32 = 2;
const WINDOW_INDICATOR_GAP: i32 = 1;

impl BarLayout {
    fn new(font: &Font, width: i32, config: &BarConfig, snapshot: &BarSnapshot) -> Self {
        let text_width = |text: &str| {
            text.chars()
                .map(|character| font.metrics(character, config.font_size).advance_width)
                .sum::<f32>()
        };
        // Window markers live underneath the number rather than beside it. This preserves the
        // original compact tag width and, importantly, its familiar pointer target.
        let tag_width = text_width("9").ceil() as i32 + 12;
        let tags_end = tag_width * snapshot.tag_count as i32;
        // All three symbols contain three monospace-friendly ASCII cells, but measure every one
        // so custom proportional fonts still receive one stable, non-jumping layout slot.
        let mode_width = ["[]=", "[ ]", "><>"]
            .into_iter()
            .map(&text_width)
            .fold(0.0_f32, f32::max)
            .ceil() as i32
            + 12;
        let mode_end = tags_end + mode_width;
        let status_width = text_width(&snapshot.status).ceil() as i32 + 14;
        let status_x = (width - status_width).max(mode_end);
        Self {
            tag_width,
            tags_end,
            mode_end,
            status_x,
        }
    }

    fn hit(&self, x: i32, snapshot: &BarSnapshot) -> Option<BarHit> {
        if (0..self.tags_end).contains(&x) {
            return Some(BarHit::Tag((x / self.tag_width) as usize));
        }
        if (self.tags_end..self.mode_end).contains(&x) {
            return Some(BarHit::LayoutMode);
        }
        if !(self.mode_end..self.status_x).contains(&x) || snapshot.windows.is_empty() {
            return None;
        }
        let title_width = self.status_x - self.mode_end;
        // Use the renderer's exact integer boundaries. Inverting the division algebraically would
        // be subtly wrong at a rounded boundary (for example pixel 2 of a five-pixel, two-window
        // strip), while the number of visible windows is small enough that this scan is trivial.
        (0..snapshot.windows.len()).find_map(|index| {
            let start = self.mode_end + title_width * index as i32 / snapshot.windows.len() as i32;
            let end =
                self.mode_end + title_width * (index as i32 + 1) / snapshot.windows.len() as i32;
            (x >= start && x < end).then_some(BarHit::Window(index))
        })
    }
}

pub struct BarState {
    last_refresh: Option<Instant>,
    status: String,
    font: Font,
}

impl BarState {
    pub fn new(config: &BarConfig) -> Result<Self> {
        Ok(Self {
            last_refresh: None,
            status: String::new(),
            font: load_system_font(&config.font)?,
        })
    }

    /// Resolves a left-button press using the exact font metrics used by the renderer.
    pub fn hit_test(
        &self,
        width: i32,
        config: &BarConfig,
        snapshot: &BarSnapshot,
        x: i32,
        y: i32,
    ) -> Option<BarHit> {
        if x < 0 || x >= width || y < 0 || y >= config.height {
            return None;
        }
        BarLayout::new(&self.font, width, config, snapshot).hit(x, snapshot)
    }

    /// Refreshes stdout-backed status blocks without hard-coding clock, battery or network APIs.
    pub fn refresh(&mut self, config: &BarConfig) -> bool {
        let interval = Duration::from_millis(config.refresh_interval_ms);
        if self
            .last_refresh
            .is_some_and(|last| last.elapsed() < interval)
        {
            return false;
        }
        self.last_refresh = Some(Instant::now());
        let status = config
            .status_commands
            .iter()
            .filter_map(|command| match Command::new("/bin/sh").arg("-c").arg(command).output() {
                Ok(output) if output.status.success() => {
                    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                    (!text.is_empty()).then_some(text)
                }
                Ok(output) => {
                    tracing::warn!(%command, status = %output.status, "bar status command failed");
                    None
                }
                Err(error) => {
                    tracing::warn!(%command, %error, "cannot execute bar status command");
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" | ");
        if status == self.status {
            return false;
        }
        self.status = status;
        true
    }

    pub fn text(&self) -> &str {
        &self.status
    }
}

#[derive(Clone, PartialEq)]
struct RectangleSpec {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    color: [f32; 4],
}

/// Persistent font and one software-rendered texture shared by both display backends.
///
/// Keeping the whole strip in one buffer avoids retaining thousands of tiny GPU resources for the
/// individual grayscale runs making up the glyphs.
pub struct BarRenderer {
    font: Font,
    buffer: Option<MemoryRenderBuffer>,
    previous: Vec<RectangleSpec>,
}

impl BarRenderer {
    pub fn new(config: &BarConfig) -> Result<Self> {
        Ok(Self {
            font: load_system_font(&config.font)?,
            buffer: None,
            previous: Vec::new(),
        })
    }

    pub fn element<R>(
        &mut self,
        renderer: &mut R,
        width: i32,
        config: &BarConfig,
        snapshot: &BarSnapshot,
    ) -> Result<MemoryRenderBufferRenderElement<R>, R::Error>
    where
        R: Renderer + ImportMem,
        R::TextureId: Send + Clone + 'static,
    {
        let background = color("background", &config.background);
        let foreground = color("foreground", &config.foreground);
        let selected_background = color("selected_background", &config.selected_background);
        let selected_foreground = color("selected_foreground", &config.selected_foreground);
        let occupied = color("occupied", &config.occupied);
        let size = config.font_size;
        let mut specs = vec![RectangleSpec {
            x: 0,
            y: 0,
            width,
            height: config.height,
            color: background,
        }];

        let layout = BarLayout::new(&self.font, width, config, snapshot);
        let tag_width = layout.tag_width;
        let mut left = 0;
        for tag in 0..snapshot.tag_count {
            let mask = 1_u16 << tag;
            let selected = snapshot.selected_tags & mask != 0;
            if selected {
                specs.push(RectangleSpec {
                    x: left,
                    y: 0,
                    width: tag_width,
                    height: config.height,
                    color: selected_background,
                });
            }
            let label_color = if selected {
                selected_foreground
            } else if snapshot.occupied_tags & mask != 0 {
                occupied
            } else {
                foreground
            };
            self.text_specs(
                &mut specs,
                left + 6,
                &(tag + 1).to_string(),
                size,
                config.height,
                left + tag_width,
                label_color,
            );

            // A window may carry multiple dwm-style tag bits. State counts it once for every tag
            // it belongs to, and this cap keeps the display compact even for a very busy desktop.
            let indicator_count = snapshot
                .window_counts
                .get(tag)
                .copied()
                .unwrap_or_default()
                .min(config.max_window_indicators);
            let indicator_strip_width = indicator_count as i32
                * (WINDOW_INDICATOR_WIDTH + WINDOW_INDICATOR_GAP)
                - if indicator_count == 0 {
                    0
                } else {
                    WINDOW_INDICATOR_GAP
                };
            // Centre the tiny markers underneath the tag number. Keeping them on the last two
            // pixels makes them readable without moving the text baseline or widening the bar.
            let indicator_x = left + (tag_width - indicator_strip_width) / 2;
            let indicator_y = config.height - WINDOW_INDICATOR_HEIGHT;
            for index in 0..indicator_count {
                specs.push(RectangleSpec {
                    x: indicator_x + index as i32 * (WINDOW_INDICATOR_WIDTH + WINDOW_INDICATOR_GAP),
                    y: indicator_y,
                    width: WINDOW_INDICATOR_WIDTH,
                    height: WINDOW_INDICATOR_HEIGHT,
                    color: label_color,
                });
            }
            left += tag_width;
        }

        self.text_specs(
            &mut specs,
            left + 6,
            snapshot.layout_symbol,
            size,
            config.height,
            layout.mode_end,
            foreground,
        );
        left = layout.mode_end;

        #[cfg(feature = "launcher")]
        if let Some(launcher) = &snapshot.launcher {
            // Preserve tags/layout on the left and dedicate the remainder of the bar to search.
            let available = (width - left).max(1);
            let query_width = (available / 3).clamp(140, 360).min(available);
            specs.push(RectangleSpec {
                x: left,
                y: 0,
                width: query_width,
                height: config.height,
                color: selected_background,
            });
            self.text_specs(
                &mut specs,
                left + 8,
                &format!("> {}", launcher.query),
                size,
                config.height,
                left + query_width - 6,
                selected_foreground,
            );
            let results_left = left + query_width;
            let results_width = width - results_left;
            let count = launcher.results.len().max(1) as i32;
            for (index, result) in launcher.results.iter().enumerate() {
                let start = results_left + results_width * index as i32 / count;
                let end = results_left + results_width * (index as i32 + 1) / count;
                if index == launcher.selected {
                    specs.push(RectangleSpec {
                        x: start,
                        y: 0,
                        width: end - start,
                        height: config.height,
                        color: selected_background,
                    });
                }
                self.text_specs(
                    &mut specs,
                    start + 7,
                    result,
                    size,
                    config.height,
                    end - 6,
                    if index == launcher.selected {
                        selected_foreground
                    } else {
                        foreground
                    },
                );
            }
            self.update_buffer(width, config.height, &specs);
            return MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                (0.0, 0.0),
                self.buffer.as_ref().expect("bar buffer was initialized"),
                None,
                None,
                None,
                Kind::Unspecified,
            );
        }

        let status_x = layout.status_x;
        self.text_specs(
            &mut specs,
            status_x + 7,
            &snapshot.status,
            size,
            config.height,
            width,
            foreground,
        );

        // Divide the full title strip evenly among all visible clients. Consequently two windows
        // get one half each, three one third each, and so on; a short title never steals space
        // from another open window. The focused client retains the selected palette.
        let title_width = (status_x - left).max(0);
        let count = snapshot.windows.len() as i32;
        for (index, window) in snapshot.windows.iter().enumerate() {
            let start = left + title_width * index as i32 / count;
            let end = left + title_width * (index as i32 + 1) / count;
            if end <= start {
                continue;
            }
            if window.focused {
                specs.push(RectangleSpec {
                    x: start,
                    y: 0,
                    width: end - start,
                    height: config.height,
                    color: selected_background,
                });
            }
            self.text_specs(
                &mut specs,
                start + 8,
                &window.title,
                size,
                config.height,
                end - 8,
                if window.focused {
                    selected_foreground
                } else {
                    foreground
                },
            );
        }

        self.update_buffer(width, config.height, &specs);
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            (0.0, 0.0),
            self.buffer.as_ref().expect("bar buffer was initialized"),
            None,
            None,
            None,
            Kind::Unspecified,
        )
    }

    #[cfg(all(feature = "launcher", not(feature = "bar")))]
    /// Renders the same launcher as a centered list when the bar feature is absent.
    pub fn launcher_element<R>(
        &mut self,
        renderer: &mut R,
        screen_width: i32,
        screen_height: i32,
        config: &BarConfig,
        launcher: &LauncherSnapshot,
    ) -> Result<MemoryRenderBufferRenderElement<R>, R::Error>
    where
        R: Renderer + ImportMem,
        R::TextureId: Send + Clone + 'static,
    {
        let row_height = (config.font_size.ceil() as i32 + 10).max(24);
        let width = 720.min((screen_width - 40).max(240));
        let height = row_height * (launcher.results.len() as i32 + 1);
        let background = color("background", &config.background);
        let foreground = color("foreground", &config.foreground);
        let selected_background = color("selected_background", &config.selected_background);
        let selected_foreground = color("selected_foreground", &config.selected_foreground);
        let mut specs = vec![RectangleSpec {
            x: 0,
            y: 0,
            width,
            height,
            color: background,
        }];
        self.text_specs(
            &mut specs,
            10,
            &format!("> {}", launcher.query),
            config.font_size,
            row_height,
            width - 10,
            foreground,
        );
        for (index, result) in launcher.results.iter().enumerate() {
            let y = row_height * (index as i32 + 1);
            if index == launcher.selected {
                specs.push(RectangleSpec {
                    x: 0,
                    y,
                    width,
                    height: row_height,
                    color: selected_background,
                });
            }
            let before = specs.len();
            self.text_specs(
                &mut specs,
                10,
                result,
                config.font_size,
                row_height,
                width - 10,
                if index == launcher.selected {
                    selected_foreground
                } else {
                    foreground
                },
            );
            for spec in &mut specs[before..] {
                spec.y += y;
            }
        }
        self.update_buffer(width, height, &specs);
        let location = (
            f64::from((screen_width - width) / 2),
            f64::from((screen_height - height) / 2),
        );
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            self.buffer
                .as_ref()
                .expect("launcher buffer was initialized"),
            None,
            None,
            None,
            Kind::Unspecified,
        )
    }

    /// Rasterizes system-font glyphs with their original grayscale coverage.
    ///
    /// Keeping Fontdue's full 8-bit alpha value matters more than minimizing element count here:
    /// the bar is static between title/status changes, while coarse alpha buckets made a normally
    /// smooth glyph look noticeably jagged compared with native text rendering.
    #[allow(clippy::too_many_arguments)]
    fn text_specs(
        &self,
        specs: &mut Vec<RectangleSpec>,
        x: i32,
        text: &str,
        size: f32,
        bar_height: i32,
        clip_x: i32,
        color: [f32; 4],
    ) {
        let line = self
            .font
            .horizontal_line_metrics(size)
            .expect("configured font has no horizontal line metrics");
        let baseline = ((bar_height as f32 + line.ascent + line.descent) / 2.0).round() as i32;
        let mut cursor = x as f32;

        for character in text.chars() {
            let (metrics, bitmap) = self.font.rasterize(character, size);
            let glyph_x = cursor.round() as i32 + metrics.xmin;
            let glyph_y = baseline - metrics.height as i32 - metrics.ymin;
            for row in 0..metrics.height {
                let mut column = 0;
                while column < metrics.width {
                    let alpha = bitmap[row * metrics.width + column];
                    if alpha == 0 {
                        column += 1;
                        continue;
                    }
                    let start = column;
                    while column < metrics.width && bitmap[row * metrics.width + column] == alpha {
                        column += 1;
                    }
                    let run_x = glyph_x + start as i32;
                    if run_x >= clip_x {
                        break;
                    }
                    let mut run_color = color;
                    run_color[3] *= f32::from(alpha) / 255.0;
                    specs.push(RectangleSpec {
                        x: run_x,
                        y: glyph_y + row as i32,
                        width: ((column - start) as i32).min(clip_x - run_x),
                        height: 1,
                        color: run_color,
                    });
                }
            }
            cursor += metrics.advance_width;
            if cursor.round() as i32 >= clip_x {
                break;
            }
        }
    }

    fn update_buffer(&mut self, width: i32, height: i32, specs: &[RectangleSpec]) {
        let resized = self
            .previous
            .first()
            .is_none_or(|background| background.width != width || background.height != height);
        if self.buffer.is_none() || resized {
            self.buffer = Some(MemoryRenderBuffer::new(
                Fourcc::Abgr8888,
                (width, height),
                1,
                Transform::Normal,
                None,
            ));
        }
        if self.previous == specs {
            return;
        }

        let mut context = self.buffer.as_mut().unwrap().render();
        context
            .draw(|pixels| {
                pixels.fill(0);
                for spec in specs {
                    paint_rectangle(pixels, width, height, spec);
                }
                Ok::<_, std::convert::Infallible>(vec![Rectangle::from_size(
                    (width, height).into(),
                )])
            })
            .unwrap();
        self.previous = specs.to_vec();
    }
}

/// Alpha-composites one rectangle into little-endian ABGR8888 (RGBA byte order).
fn paint_rectangle(pixels: &mut [u8], width: i32, height: i32, spec: &RectangleSpec) {
    let start_x = spec.x.clamp(0, width);
    let end_x = (spec.x + spec.width).clamp(0, width);
    let start_y = spec.y.clamp(0, height);
    let end_y = (spec.y + spec.height).clamp(0, height);
    let source_alpha = spec.color[3].clamp(0.0, 1.0);
    for y in start_y..end_y {
        for x in start_x..end_x {
            let offset = ((y * width + x) * 4) as usize;
            let destination_alpha = f32::from(pixels[offset + 3]) / 255.0;
            let output_alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
            for channel in 0..3 {
                let destination = f32::from(pixels[offset + channel]) / 255.0;
                let output = if output_alpha == 0.0 {
                    0.0
                } else {
                    (spec.color[channel] * source_alpha
                        + destination * destination_alpha * (1.0 - source_alpha))
                        / output_alpha
                };
                pixels[offset + channel] = (output * 255.0).round() as u8;
            }
            pixels[offset + 3] = (output_alpha * 255.0).round() as u8;
        }
    }
}

fn color(name: &str, value: &str) -> [f32; 4] {
    parse_named_hex_color(name, value).expect("bar color was validated during startup")
}

/// Resolves a user-facing Fontconfig pattern to a file and loads it for Fontdue rasterization.
fn load_system_font(pattern: &str) -> Result<Font> {
    let output = Command::new("fc-match")
        .args(["--format=%{file}\n", pattern])
        .output()
        .context("cannot run fc-match; install fontconfig to use the bar")?;
    if !output.status.success() {
        bail!("fc-match could not resolve bar font {pattern:?}");
    }
    let stdout = String::from_utf8(output.stdout).context("fc-match returned a non-UTF-8 path")?;
    let path = stdout
        .lines()
        .next()
        .filter(|path| !path.is_empty())
        .with_context(|| format!("no installed font matches {pattern:?}"))?;
    let bytes = fs::read(path).with_context(|| format!("cannot read matched font {path}"))?;
    Font::from_bytes(bytes, FontSettings::default())
        .map_err(|error| anyhow::anyhow!("cannot parse matched font {path}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_refresh_combines_successful_commands() {
        let config = BarConfig {
            status_commands: vec!["printf left".into(), "printf right".into()],
            ..BarConfig::default()
        };
        let mut state = BarState::new(&config).unwrap();
        state.refresh(&config);
        assert_eq!(state.text(), "left | right");
    }

    #[test]
    fn configured_system_font_loads() {
        let renderer = BarRenderer::new(&BarConfig::default()).unwrap();
        assert_ne!(renderer.font.lookup_glyph_index('A'), 0);
    }

    #[test]
    fn hit_test_maps_tags_titles_and_status() {
        let config = BarConfig::default();
        let snapshot = BarSnapshot {
            selected_tags: 1,
            occupied_tags: 1,
            tag_count: 4,
            layout_symbol: "[]=",
            window_counts: vec![2, 0, 0, 0],
            windows: vec![
                BarWindow {
                    title: "one".into(),
                    focused: true,
                },
                BarWindow {
                    title: "two".into(),
                    focused: false,
                },
            ],
            status: "12:34".into(),
            #[cfg(feature = "launcher")]
            launcher: None,
        };
        let state = BarState::new(&config).unwrap();
        let layout = BarLayout::new(&state.font, 1000, &config, &snapshot);

        assert_eq!(
            state.hit_test(1000, &config, &snapshot, 1, 1),
            Some(BarHit::Tag(0))
        );
        assert_eq!(
            state.hit_test(1000, &config, &snapshot, layout.tags_end + 1, 1),
            Some(BarHit::LayoutMode)
        );
        assert_eq!(
            state.hit_test(1000, &config, &snapshot, layout.mode_end + 1, 1),
            Some(BarHit::Window(0))
        );
        assert_eq!(
            state.hit_test(1000, &config, &snapshot, layout.status_x - 1, 1),
            Some(BarHit::Window(1))
        );
        assert_eq!(
            state.hit_test(1000, &config, &snapshot, layout.status_x, 1),
            None
        );
        assert_eq!(
            state.hit_test(1000, &config, &snapshot, 1, config.height),
            None
        );
    }
}
