//! Minimal dwm-style status bar compiled only with the default `bar` Cargo feature.
//!
//! The bar deliberately uses an embedded 8×8 bitmap font and Smithay solid-color elements. It
//! therefore needs neither a desktop font service nor a privileged external layer-shell client,
//! which keeps it available during the earliest stages of a bare DRM session.

use std::{
    process::Command,
    time::{Duration, Instant},
};

use anvil::config::{Bar as BarConfig, parse_named_hex_color};
use font8x8::{BASIC_FONTS, UnicodeFonts};
use smithay::backend::renderer::element::{
    Kind,
    solid::{SolidColorBuffer, SolidColorRenderElement},
};

/// Immutable window-manager data needed to draw one frame of the bar.
pub struct BarSnapshot {
    pub selected_tags: u16,
    pub occupied_tags: u16,
    pub windows: Vec<BarWindow>,
    pub status: String,
}

pub struct BarWindow {
    pub title: String,
    pub focused: bool,
}

/// Runtime status command cache.
pub struct BarState {
    last_refresh: Option<Instant>,
    status: String,
}

impl BarState {
    pub fn new() -> Self {
        Self {
            last_refresh: None,
            status: String::new(),
        }
    }

    /// Runs configured status commands only when their common refresh interval has elapsed.
    ///
    /// stdout is intentionally the complete interface: any language or shell script can produce a
    /// block without teaching the compositor about clocks, batteries, networks or audio systems.
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

/// Persistent GPU-independent buffers used by both Winit and DRM renderers.
pub struct BarRenderer {
    buffers: Vec<SolidColorBuffer>,
    previous: Vec<RectangleSpec>,
}

impl BarRenderer {
    pub fn new() -> Self {
        Self {
            buffers: Vec::new(),
            previous: Vec::new(),
        }
    }

    /// Converts tags, window titles and status text into front-to-back solid rectangles.
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
        let mut specs = vec![RectangleSpec {
            x: 0,
            y: 0,
            width,
            height: config.height,
            color: background,
        }];

        let scale = ((config.height - 6) / 8).clamp(1, 3);
        let character_width = 8 * scale;
        let tag_width = character_width + 10;
        let text_y = (config.height - 8 * scale) / 2;
        let mut left = 0;
        for tag in 0..9 {
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
            text_specs(
                &mut specs,
                left + 5,
                text_y,
                &(tag + 1).to_string(),
                scale,
                label_color,
            );
            left += tag_width;
        }

        let status_width = text_width(&snapshot.status, character_width) + 12;
        let status_x = (width - status_width).max(left);
        text_specs(
            &mut specs,
            status_x + 6,
            text_y,
            &snapshot.status,
            scale,
            foreground,
        );

        // The middle follows dwm's title area but displays every visible client. Focus receives the
        // selected palette; other titles remain readable without competing with the active one.
        let mut middle_x = left;
        for window in &snapshot.windows {
            if middle_x >= status_x {
                break;
            }
            let available = status_x - middle_x;
            let natural = text_width(&window.title, character_width) + 16;
            let section_width = natural.min(available);
            if window.focused {
                specs.push(RectangleSpec {
                    x: middle_x,
                    y: 0,
                    width: section_width,
                    height: config.height,
                    color: selected_background,
                });
            }
            let maximum_characters = ((section_width - 12).max(0) / character_width) as usize;
            let title: String = window.title.chars().take(maximum_characters).collect();
            text_specs(
                &mut specs,
                middle_x + 8,
                text_y,
                &title,
                scale,
                if window.focused {
                    selected_foreground
                } else {
                    foreground
                },
            );
            middle_x += section_width;
        }

        self.update_buffers(&specs);
        specs
            .iter()
            .zip(&self.buffers)
            // Smithay consumes custom elements front-to-back. Text and selected-section fills were
            // appended after the base background while constructing the convenient paint list, so
            // reverse it here to keep the opaque base behind every glyph.
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

fn color(name: &str, value: &str) -> [f32; 4] {
    parse_named_hex_color(name, value).expect("bar color was validated during startup")
}

fn text_width(text: &str, character_width: i32) -> i32 {
    text.chars().count() as i32 * character_width
}

/// Emits one rectangle for every horizontal run of lit bitmap pixels.
fn text_specs(
    specs: &mut Vec<RectangleSpec>,
    x: i32,
    y: i32,
    text: &str,
    scale: i32,
    color: [f32; 4],
) {
    let mut cursor = x;
    for character in text.chars() {
        let glyph = BASIC_FONTS
            .get(character)
            .or_else(|| BASIC_FONTS.get('?'))
            .unwrap();
        for (row, bits) in glyph.into_iter().enumerate() {
            let mut column = 0;
            while column < 8 {
                if bits & (1 << column) == 0 {
                    column += 1;
                    continue;
                }
                let start = column;
                while column < 8 && bits & (1 << column) != 0 {
                    column += 1;
                }
                specs.push(RectangleSpec {
                    x: cursor + start * scale,
                    y: y + row as i32 * scale,
                    width: (column - start) * scale,
                    height: scale,
                    color,
                });
            }
        }
        cursor += 8 * scale;
    }
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
}
