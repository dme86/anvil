//! Shared UI fonts and Fontconfig fallback, independent of any particular font family.
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::SystemTime,
};

use anyhow::{Context, Result, bail};
#[path = "ui_font/face.rs"]
mod face;
use face::Font;
use fontdue::{FontSettings, LineMetrics, Metrics};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct FaceKey {
    path: PathBuf,
    index: u32,
    length: u64,
    modified: Option<SystemTime>,
}

type FaceCache = HashMap<FaceKey, Weak<Font>>;
static FACES: OnceLock<Mutex<FaceCache>> = OnceLock::new();
static UI_FONTS: OnceLock<Mutex<HashMap<String, Weak<UiFont>>>> = OnceLock::new();

pub struct UiFont {
    primary: Arc<Font>,
    pattern: String,
    fallbacks: Mutex<HashMap<char, Option<Arc<Font>>>>,
    candidates: OnceLock<Vec<FontCandidate>>,
    glyphs: Mutex<GlyphCache>,
}

impl UiFont {
    pub fn load(pattern: &str) -> Result<Arc<Self>> {
        let primary = resolve_face(pattern)?;
        let mut cache = UI_FONTS
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(font) = cache.get(pattern).and_then(Weak::upgrade) {
            if Arc::ptr_eq(&font.primary, &primary) {
                return Ok(font);
            }
        }
        cache.retain(|_, font| font.strong_count() > 0);
        let font = Arc::new(Self {
            primary,
            pattern: pattern.to_owned(),
            fallbacks: Mutex::new(HashMap::new()),
            candidates: OnceLock::new(),
            glyphs: Mutex::new(GlyphCache::default()),
        });
        cache.insert(pattern.to_owned(), Arc::downgrade(&font));
        Ok(font)
    }

    fn face(&self, character: char) -> Option<Arc<Font>> {
        if self.primary.lookup_glyph_index(character) != 0 {
            return Some(self.primary.clone());
        }
        let mut fallbacks = self.fallbacks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(face) = fallbacks.get(&character) {
            return face.clone();
        }
        // Resolve the fallback order/coverage once, not one subprocess per
        // missing character in a title. Parse outlines only for a needed face.
        let candidates = self
            .candidates
            .get_or_init(|| fallback_candidates(&self.pattern).unwrap_or_default());
        let face = candidates
            .iter()
            .filter(|candidate| candidate.covers(character))
            .find_map(|candidate| {
                load_face(candidate.path.clone(), candidate.index)
                    .ok()
                    .filter(|font| font.lookup_glyph_index(character) != 0)
            });
        // Titles and status scripts may contain arbitrary Unicode. Do not retain an
        // unbounded number of fallback faces or failed lookups for their lifetime.
        if fallbacks.len() >= 512 {
            fallbacks.clear();
        }
        fallbacks.insert(character, face.clone());
        face
    }

    pub fn lookup_glyph_index(&self, character: char) -> u32 {
        self.face(character)
            .map_or(0, |font| font.lookup_glyph_index(character))
    }

    pub fn horizontal_line_metrics(&self, size: f32) -> Option<LineMetrics> {
        self.primary.horizontal_line_metrics(size)
    }

    pub fn metrics(&self, character: char, size: f32) -> Metrics {
        self.rasterize(character, size).0
    }

    pub fn rasterize(&self, character: char, size: f32) -> Arc<(Metrics, Vec<u8>)> {
        let key = (character, size.to_bits());
        let mut cache = self.glyphs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(glyph) = cache.entries.get(&key) {
            return glyph.clone();
        }
        let glyph = Arc::new(if invisible(character) {
            (Metrics::default(), Vec::new())
        } else {
            self.face(character)
                .unwrap_or_else(|| self.primary.clone())
                .rasterize(character, size)
        });
        cache.insert(key, glyph.clone());
        glyph
    }
}

type Glyph = Arc<(Metrics, Vec<u8>)>;
const MAX_GLYPH_BYTES: usize = 4 * 1024 * 1024;
const MAX_GLYPHS: usize = 1024;

#[derive(Default)]
struct GlyphCache {
    entries: HashMap<(char, u32), Glyph>,
    bytes: usize,
}

impl GlyphCache {
    fn insert(&mut self, key: (char, u32), glyph: Glyph) {
        let bytes = glyph.1.len();
        // A single oversized glyph must not evict the normal UI working set.
        if bytes > MAX_GLYPH_BYTES {
            return;
        }
        if self.entries.len() >= MAX_GLYPHS || self.bytes + bytes > MAX_GLYPH_BYTES {
            self.entries.clear();
            self.bytes = 0;
        }
        if let Some(previous) = self.entries.insert(key, glyph) {
            self.bytes -= previous.1.len();
        }
        self.bytes += bytes;
    }
}

// Fontdue renders individual characters, not emoji sequences. Presentation
// selectors and joiners must not turn into visible missing-glyph boxes or spaces.
fn invisible(c: char) -> bool {
    matches!(c, '\u{200c}' | '\u{200d}' | '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}')
}

struct FontCandidate {
    path: PathBuf,
    index: u32,
    ranges: Vec<(u32, u32)>,
}

impl FontCandidate {
    fn covers(&self, ch: char) -> bool {
        let code = ch as u32;
        self.ranges
            .iter()
            .any(|&(start, end)| start <= code && code <= end)
    }
}

fn fallback_candidates(pattern: &str) -> Result<Vec<FontCandidate>> {
    let output = Command::new("fc-match")
        .args([
            "--sort",
            "--format=%{file}\n%{index}\n%{charset}\n",
            pattern,
        ])
        .output()
        .context("cannot resolve fallback fonts")?;
    if !output.status.success() {
        bail!("cannot resolve fallback fonts");
    }
    let text = String::from_utf8(output.stdout)?;
    let lines: Vec<_> = text.lines().collect();
    let mut candidates = Vec::new();
    for face in lines.chunks_exact(3) {
        let index = face[1].parse::<u32>()? & 0xffff;
        let mut ranges = Vec::new();
        for range in face[2].split_whitespace() {
            let (start, end) = range.split_once('-').unwrap_or((range, range));
            ranges.push((
                u32::from_str_radix(start, 16)?,
                u32::from_str_radix(end, 16)?,
            ));
        }
        candidates.push(FontCandidate {
            path: face[0].into(),
            index,
            ranges,
        });
    }
    Ok(candidates)
}

fn resolve_face(pattern: &str) -> Result<Arc<Font>> {
    let output = Command::new("fc-match")
        .args(["--format=%{file}\n%{index}\n", pattern])
        .output()
        .context("cannot run fc-match; install fontconfig to use the UI")?;
    if !output.status.success() {
        bail!("fc-match could not resolve font {pattern:?}");
    }
    let result = String::from_utf8(output.stdout).context("non-UTF-8 font path")?;
    let mut lines = result.lines();
    let path = PathBuf::from(
        lines
            .next()
            .filter(|s| !s.is_empty())
            .context("no matching font")?,
    );
    // Fontconfig encodes variable-font named instances in the upper bits. Fontdue
    // selects the collection face; named variation coordinates are not supported.
    let index = lines
        .next()
        .unwrap_or("0")
        .parse::<u32>()
        .context("invalid font face index")?
        & 0xffff;
    load_face(path, index)
}

fn load_face(path: PathBuf, index: u32) -> Result<Arc<Font>> {
    let path = path
        .canonicalize()
        .with_context(|| format!("cannot resolve font {}", path.display()))?;
    let metadata = fs::metadata(&path)?;
    let key = FaceKey {
        path,
        index,
        length: metadata.len(),
        modified: metadata.modified().ok(),
    };
    let mut cache = FACES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(font) = cache.get(&key).and_then(Weak::upgrade) {
        return Ok(font);
    }
    cache.retain(|_, font| font.strong_count() > 0);
    let font = Arc::new(
        Font::from_bytes(
            fs::read(&key.path)?,
            FontSettings {
                collection_index: index,
                ..FontSettings::default()
            },
        )
        .map_err(|error| {
            anyhow::anyhow!(
                "cannot parse font {} face {index}: {error}",
                key.path.display()
            )
        })?,
    );
    cache.insert(key, Arc::downgrade(&font));
    Ok(font)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_consumers_share_a_parsed_face_and_fallbacks() {
        let a = UiFont::load("monospace").unwrap();
        let b = UiFont::load("monospace").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(Arc::ptr_eq(&a.primary, &b.primary));
        assert!(a.lookup_glyph_index('A') != 0);
        assert_eq!(
            a.metrics('A', 16.0).advance_width,
            b.metrics('A', 16.0).advance_width
        );
        let c = resolve_face("monospace").unwrap();
        assert!(Arc::ptr_eq(&a.primary, &c));
    }

    #[test]
    fn collection_faces_are_distinct_and_updated_files_are_reloaded() {
        fn bytes(pattern: &str) -> Vec<u8> {
            let output = Command::new("fc-match")
                .args(["--format=%{file}", pattern])
                .output()
                .unwrap();
            fs::read(String::from_utf8(output.stdout).unwrap()).unwrap()
        }
        let mono = bytes("monospace");
        let serif = bytes("serif");
        // Generate a TTC from the CI system fonts; no third-party font fixture
        // or font editor is needed. Existing collections are tested by Fontconfig.
        if &mono[..4] == b"ttcf" || &serif[..4] == b"ttcf" {
            return;
        }
        let mut collection = b"ttcf\x00\x01\x00\x00\x00\x00\x00\x02".to_vec();
        let second = 20 + mono.len().next_multiple_of(4);
        collection.extend_from_slice(&20u32.to_be_bytes());
        collection.extend_from_slice(&(second as u32).to_be_bytes());
        for (source, offset) in [(&mono, 20), (&serif, second)] {
            collection.resize(offset, 0);
            let mut face = source.clone();
            let tables = u16::from_be_bytes(face[4..6].try_into().unwrap()) as usize;
            for table in 0..tables {
                let at = 12 + table * 16 + 8;
                let original = u32::from_be_bytes(face[at..at + 4].try_into().unwrap());
                face[at..at + 4].copy_from_slice(&(original + offset as u32).to_be_bytes());
            }
            collection.extend_from_slice(&face);
        }
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("anvil-font-{}-{stamp}.ttc", std::process::id()));
        fs::write(&path, &collection).unwrap();
        let first = load_face(path.clone(), 0).unwrap();
        let second = load_face(path.clone(), 1).unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        let expected = Font::from_bytes(serif, FontSettings::default()).unwrap();
        assert_eq!(
            second.metrics('W', 20.0).advance_width,
            expected.metrics('W', 20.0).advance_width
        );
        assert!(load_face(path.clone(), 2).is_err());
        collection.push(0);
        fs::write(&path, &collection).unwrap();
        let updated = load_face(path.clone(), 1).unwrap();
        assert!(!Arc::ptr_eq(&second, &updated));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn shared_metadata_can_rasterize_on_another_thread() {
        let font = UiFont::load("monospace").unwrap();
        let expected = font.primary.rasterize('M', 37.125);
        let worker = std::thread::spawn(move || font.rasterize('M', 37.125));
        let glyph = worker.join().unwrap();
        assert_eq!(glyph.1, expected.1);
        assert_eq!(glyph.0.advance_width, expected.0.advance_width);
    }

    #[test]
    fn repeated_glyphs_reuse_identical_pixels_at_the_same_size() {
        let font = UiFont::load("monospace").unwrap();
        let first = font.rasterize('W', 16.0);
        let second = font.rasterize('W', 16.0);
        assert!(Arc::ptr_eq(&first, &second));
        let direct = font.primary.rasterize('W', 16.0);
        assert_eq!(first.1, direct.1);
        assert_eq!(first.0.advance_width, direct.0.advance_width);
        let larger = font.rasterize('W', 24.0);
        assert!(!Arc::ptr_eq(&first, &larger));
        assert_ne!(first.0.height, larger.0.height);
    }

    #[test]
    fn glyph_cache_bounds_bitmap_memory_and_entry_count() {
        let mut cache = GlyphCache::default();
        let full = Arc::new((Metrics::default(), vec![0; MAX_GLYPH_BYTES]));
        cache.insert(('a', 1), full);
        cache.insert(('b', 1), Arc::new((Metrics::default(), vec![0; 1])));
        assert_eq!(cache.bytes, 1);
        assert_eq!(cache.entries.len(), 1);
        let oversized = Arc::new((Metrics::default(), vec![0; MAX_GLYPH_BYTES + 1]));
        cache.insert(('c', 1), oversized);
        assert_eq!(cache.entries.len(), 1);
        for index in 0..MAX_GLYPHS + 1 {
            cache.insert(
                ('x', index as u32),
                Arc::new((Metrics::default(), Vec::new())),
            );
        }
        assert!(cache.entries.len() <= MAX_GLYPHS);
        assert!(cache.bytes <= MAX_GLYPH_BYTES);
    }

    #[test]
    fn missing_glyphs_are_cached_and_render_without_panicking() {
        let font = UiFont::load("monospace").unwrap();
        let ch = '\u{10ffff}';
        assert_eq!(font.lookup_glyph_index(ch), 0);
        assert_eq!(
            font.metrics(ch, 16.0).advance_width,
            font.rasterize(ch, 16.0).0.advance_width
        );
        assert!(font.fallbacks.lock().unwrap().contains_key(&ch));
    }

    #[test]
    fn presentation_selectors_have_no_ink_or_advance() {
        let font = UiFont::load("monospace").unwrap();
        for ch in ['\u{fe0f}', '\u{200d}', '\u{e0100}'] {
            assert_eq!(font.metrics(ch, 16.0).advance_width, 0.0);
            assert!(font.rasterize(ch, 16.0).1.is_empty());
        }
    }

    #[test]
    fn fontconfig_fallback_uses_the_same_face_for_measurement_and_ink() {
        let font = UiFont::load("monospace").unwrap();
        for ch in ['A', 'é', 'λ', 'Ж', '☀', '\u{f0594}'] {
            if let Some(face) = font.face(ch) {
                assert_ne!(face.lookup_glyph_index(ch), 0);
                assert_eq!(
                    font.metrics(ch, 16.0).advance_width,
                    face.metrics(ch, 16.0).advance_width
                );
                assert_eq!(
                    font.rasterize(ch, 16.0).0.advance_width,
                    face.metrics(ch, 16.0).advance_width
                );
            }
        }
    }
}
