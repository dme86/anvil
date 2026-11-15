//! Immutable face metadata; FreeType outlines are loaded only for cache misses.
//! No FreeType face/library is shared between threads or retained by the UI cache.
use anyhow::{Context, Result, bail};
use fontdue::{FontSettings, LineMetrics, Metrics};
use freetype::{Face, Library, RenderMode, bitmap::PixelMode, face::LoadFlag};
use std::{collections::HashMap, sync::Arc};

thread_local! {
    static LIBRARY: std::result::Result<Library, freetype::Error> = Library::init();
}

pub(super) struct Font {
    bytes: Arc<[u8]>,
    index: u32,
    characters: HashMap<char, u32>,
    ascent: f32,
    descent: f32,
    line_gap: f32,
}

impl Font {
    pub fn from_bytes(bytes: impl AsRef<[u8]>, settings: FontSettings) -> Result<Self> {
        let mut font = Self {
            bytes: Arc::from(bytes.as_ref()),
            index: settings.collection_index,
            characters: HashMap::new(),
            ascent: 0.8,
            descent: -0.2,
            line_gap: 0.0,
        };
        let (characters, ascent, descent, gap) = font.with_face(|face| {
            let characters = face
                .chars()
                .filter_map(|(code, index)| {
                    u32::try_from(code)
                        .ok()
                        .and_then(char::from_u32)
                        .map(|c| (c, index.get()))
                })
                .collect();
            let em = f32::from(face.em_size());
            let (ascent, descent, gap) = if em > 0.0 {
                let ascent = f32::from(face.ascender()) / em;
                let descent = f32::from(face.descender()) / em;
                (
                    ascent,
                    descent,
                    f32::from(face.height()) / em - ascent + descent,
                )
            } else {
                (0.8, -0.2, 0.0)
            };
            Ok((characters, ascent, descent, gap))
        })?;
        font.characters = characters;
        font.ascent = ascent;
        font.descent = descent;
        font.line_gap = gap;
        Ok(font)
    }

    fn with_face<T>(&self, f: impl FnOnce(&Face<Arc<[u8]>>) -> Result<T>) -> Result<T> {
        LIBRARY.with(|library| {
            let library = library
                .as_ref()
                .map_err(|error| anyhow::anyhow!("FreeType: {error}"))?;
            let face = library
                .new_memory_face2(self.bytes.clone(), self.index as isize)
                .context("cannot open font face")?;
            f(&face)
        })
    }

    pub fn lookup_glyph_index(&self, character: char) -> u32 {
        self.characters.get(&character).copied().unwrap_or(0)
    }

    pub fn horizontal_line_metrics(&self, size: f32) -> Option<LineMetrics> {
        Some(LineMetrics {
            ascent: self.ascent * size,
            descent: self.descent * size,
            line_gap: self.line_gap * size,
            new_line_size: (self.ascent - self.descent + self.line_gap) * size,
        })
    }

    pub fn rasterize(&self, character: char, size: f32) -> (Metrics, Vec<u8>) {
        match self.with_face(|face| {
            face.set_char_size(0, (size * 64.0).round() as isize, 72, 72)?;
            face.load_glyph(self.lookup_glyph_index(character), LoadFlag::NO_HINTING)?;
            let glyph = face.glyph();
            glyph.render_glyph(RenderMode::Normal)?;
            let bitmap = glyph.bitmap();
            let width = bitmap.width().max(0) as usize;
            let height = bitmap.rows().max(0) as usize;
            let pixels = convert_bitmap(
                bitmap.buffer(),
                width,
                height,
                bitmap.pitch(),
                bitmap.pixel_mode()?,
                bitmap.raw().num_grays.max(0) as u16,
            )?;
            let metrics = Metrics {
                xmin: glyph.bitmap_left(),
                ymin: glyph.bitmap_top() - height as i32,
                width,
                height,
                advance_width: glyph.advance().x as f32 / 64.0,
                advance_height: glyph.advance().y as f32 / 64.0,
                ..Metrics::default()
            };
            Ok((metrics, pixels))
        }) {
            Ok(glyph) => glyph,
            Err(error) => {
                tracing::warn!(%error, ?character, "cannot rasterize UI glyph");
                (
                    Metrics {
                        advance_width: size * 0.5,
                        ..Metrics::default()
                    },
                    Vec::new(),
                )
            }
        }
    }

    #[cfg(test)]
    pub fn metrics(&self, character: char, size: f32) -> Metrics {
        self.rasterize(character, size).0
    }
}

fn convert_bitmap(
    source: &[u8],
    width: usize,
    height: usize,
    pitch: i32,
    mode: PixelMode,
    grays: u16,
) -> Result<Vec<u8>> {
    let bytes = width.checked_mul(height).context("glyph size overflow")?;
    if bytes == 0 {
        return Ok(Vec::new());
    }
    let row_bytes = match mode {
        PixelMode::Gray => width,
        PixelMode::Mono => width.div_ceil(8),
        PixelMode::Gray2 => width.div_ceil(4),
        PixelMode::Gray4 => width.div_ceil(2),
        PixelMode::Bgra => width.checked_mul(4).context("glyph row overflow")?,
        _ => bail!("unsupported glyph pixel mode {mode:?}"),
    };
    let stride = pitch.unsigned_abs() as usize;
    if stride < row_bytes
        || source.len() < stride.checked_mul(height).context("glyph pitch overflow")?
    {
        bail!("invalid glyph bitmap stride");
    }
    let mut pixels = vec![0; bytes];
    for y in 0..height {
        let row = if pitch < 0 { height - 1 - y } else { y } * stride;
        for x in 0..width {
            pixels[y * width + x] = match mode {
                PixelMode::Gray => (u32::from(source[row + x]) * 255
                    / u32::from(grays.saturating_sub(1).max(1)))
                .min(255) as u8,
                PixelMode::Mono => {
                    if source[row + x / 8] & (0x80 >> (x % 8)) == 0 {
                        0
                    } else {
                        255
                    }
                }
                PixelMode::Gray2 => ((source[row + x / 4] >> (6 - 2 * (x % 4))) & 3) * 85,
                PixelMode::Gray4 => ((source[row + x / 2] >> (4 - 4 * (x % 2))) & 15) * 17,
                PixelMode::Bgra => source[row + 4 * x + 3],
                _ => unreachable!(),
            };
        }
    }
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bitmap_conversion_handles_padding_orientation_and_monochrome() {
        assert_eq!(
            convert_bitmap(&[1, 2, 0, 3, 4, 0], 2, 2, 3, PixelMode::Gray, 256).unwrap(),
            [1, 2, 3, 4]
        );
        assert_eq!(
            convert_bitmap(&[1, 2, 0, 3, 4, 0], 2, 2, -3, PixelMode::Gray, 256).unwrap(),
            [3, 4, 1, 2]
        );
        assert_eq!(
            convert_bitmap(&[0b10100000], 3, 1, 1, PixelMode::Mono, 2).unwrap(),
            [255, 0, 255]
        );
        assert_eq!(
            convert_bitmap(&[10, 20, 30, 40], 1, 1, 4, PixelMode::Bgra, 256).unwrap(),
            [40]
        );
        assert!(convert_bitmap(&[1], 2, 2, 2, PixelMode::Gray, 256).is_err());
    }
}
