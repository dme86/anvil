//! Minimal dwm-style status bar compiled only with the default `bar` Cargo feature.
//!
//! Hack Nerd Font is embedded and rasterized into Smithay solid-color elements. The direct DRM
//! session therefore gets the configured font without depending on another desktop environment.

use std::{
    process::Command,
    time::{Duration, Instant},
};

use anvil::config::{Bar as BarConfig, parse_named_hex_color};
use fontdue::{Font, FontSettings};
use smithay::backend::renderer::element::{
    Kind,
    solid::{SolidColorBuffer, SolidColorRenderElement},
};

const HACK_NERD_FONT: &[u8] = include_bytes!("../assets/HackNerdFont-Regular.ttf");

pub struct BarSnapshot {
    pub selected_tags: u16,
    pub occupied_tags: u16,
    pub tag_count: usize,
    /// One entry per visible toplevel. The renderer gives every entry an equal part of the title
    /// area so the bar's window count always agrees with the tiled/floating clients below it.
    pub windows: Vec<BarWindow>,
    pub status: String,
}

pub struct BarWindow {
    pub title: String,
    pub focused: bool,
}

#[derive(Default)]
pub struct BarState {
    last_refresh: Option<Instant>,
    status: String,
}

impl BarState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Refreshes stdout-backed status blocks without hard-coding clock, battery or network APIs.
    pub fn refresh(&mut self, config: &BarConfig) {
        let interval = Duration::from_millis(config.refresh_interval_ms);
        if self
            .last_refresh
            .is_some_and(|last| last.elapsed() < interval)
        {
            return;
        }
        self.last_refresh = Some(Instant::now());
        self.status = config
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

/// Persistent font and solid buffers shared by the nested and direct render paths.
pub struct BarRenderer {
    font: Font,
    buffers: Vec<SolidColorBuffer>,
    previous: Vec<RectangleSpec>,
}

impl Default for BarRenderer {
    fn default() -> Self {
        Self {
            font: Font::from_bytes(HACK_NERD_FONT, FontSettings::default())
                .expect("bundled Hack Nerd Font is invalid"),
            buffers: Vec::new(),
            previous: Vec::new(),
        }
    }
}

impl BarRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn elements(
        &mut self,
        width: i32,
        config: &BarConfig,
        snapshot: &BarSnapshot,
    ) -> Vec<SolidColorRenderElement> {
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

        let tag_width = self.text_width("9", size).ceil() as i32 + 12;
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
            left += tag_width;
        }

        let status_width = self.text_width(&snapshot.status, size).ceil() as i32 + 14;
        let status_x = (width - status_width).max(left);
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

        self.update_buffers(&specs);
        specs
            .iter()
            .zip(&self.buffers)
            // Smithay consumes custom elements front-to-back; our paint list is background-first.
            .rev()
            .map(|(spec, buffer)| {
                SolidColorRenderElement::from_buffer(
                    buffer,
                    (spec.x, spec.y),
                    1.0,
                    1.0,
                    Kind::Unspecified,
                )
            })
            .collect()
    }

    fn text_width(&self, text: &str, size: f32) -> f32 {
        text.chars()
            .map(|character| self.font.metrics(character, size).advance_width)
            .sum()
    }

    /// Rasterizes Hack glyphs and coalesces equal-alpha horizontal pixels into rectangles.
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
            .expect("Hack font has no line metrics");
        let baseline = ((bar_height as f32 + line.ascent + line.descent) / 2.0).round() as i32;
        let mut cursor = x as f32;

        for character in text.chars() {
            let (metrics, bitmap) = self.font.rasterize(character, size);
            let glyph_x = cursor.round() as i32 + metrics.xmin;
            let glyph_y = baseline - metrics.height as i32 - metrics.ymin;
            for row in 0..metrics.height {
                let mut column = 0;
                while column < metrics.width {
                    let alpha = alpha_bucket(bitmap[row * metrics.width + column]);
                    if alpha == 0 {
                        column += 1;
                        continue;
                    }
                    let start = column;
                    while column < metrics.width
                        && alpha_bucket(bitmap[row * metrics.width + column]) == alpha
                    {
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

    fn update_buffers(&mut self, specs: &[RectangleSpec]) {
        while self.buffers.len() < specs.len() {
            self.buffers.push(SolidColorBuffer::new((1, 1), [0.0; 4]));
        }
        for (index, spec) in specs.iter().enumerate() {
            if self.previous.get(index) != Some(spec) {
                self.buffers[index].update((spec.width, spec.height), spec.color);
            }
        }
        self.previous = specs.to_vec();
    }
}

fn alpha_bucket(alpha: u8) -> u8 {
    match alpha {
        0..=31 => 0,
        32..=95 => 64,
        96..=191 => 160,
        _ => 255,
    }
}

fn color(name: &str, value: &str) -> [f32; 4] {
    parse_named_hex_color(name, value).expect("bar color was validated during startup")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_refresh_combines_successful_commands() {
        let mut state = BarState::new();
        let config = BarConfig {
            status_commands: vec!["printf left".into(), "printf right".into()],
            ..BarConfig::default()
        };
        state.refresh(&config);
        assert_eq!(state.text(), "left | right");
    }

    #[test]
    fn bundled_hack_font_contains_nerd_font_symbols() {
        let renderer = BarRenderer::new();
        assert_ne!(renderer.font.lookup_glyph_index('󰍛'), 0);
    }
}
