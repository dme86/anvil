//! Output capture shares the display scene and renderer; SHM readback also includes DMA-BUF clients.
//! No readback work is performed unless a client has requested a frame.
use crate::Anvil;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ExportMem, Offscreen, TextureMapping,
            damage::OutputDamageTracker,
            element::{Element, Kind, RenderElement},
            gles::GlesRenderbuffer,
        },
    },
    output::Output,
    reexports::{
        wayland_protocols::ext::{
            image_capture_source::v1::server::{
                ext_image_capture_source_v1::{self, ExtImageCaptureSourceV1},
                ext_output_image_capture_source_manager_v1::{
                    self, ExtOutputImageCaptureSourceManagerV1,
                },
            },
            image_copy_capture::v1::server::{
                ext_image_copy_capture_cursor_session_v1::{
                    self, ExtImageCopyCaptureCursorSessionV1,
                },
                ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
                ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
                ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
            },
        },
        wayland_protocols_wlr::screencopy::v1::server::{
            zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
            protocol::{wl_buffer::WlBuffer, wl_output, wl_shm},
        },
    },
    utils::{Buffer, Clock, Monotonic, Physical, Rectangle, Size, Transform},
    wayland::shm::{BufferData, with_buffer_contents, with_buffer_contents_mut},
};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub(crate) struct CaptureSource {
    output: Option<Output>,
}
#[derive(Debug)]
pub(crate) struct CaptureSession {
    source: CaptureSource,
    cursors: bool,
    size: Mutex<Option<Size<i32, Buffer>>>,
    stopped: Mutex<bool>,
    frame: Mutex<Option<ExtImageCopyCaptureFrameV1>>,
}
#[derive(Debug)]
pub(crate) struct CaptureFrame {
    session: Arc<CaptureSession>,
    buffer: Mutex<Option<WlBuffer>>,
    captured: Mutex<bool>,
}
#[derive(Debug)]
pub(crate) struct LegacyFrame {
    output: Option<Output>,
    region: Option<Rectangle<i32, Buffer>>,
    cursors: bool,
    copied: Mutex<bool>,
}
#[derive(Debug, Default)]
pub(crate) struct CursorSession {
    requested: Mutex<bool>,
}

pub(crate) struct CaptureState {
    sessions: Vec<ExtImageCopyCaptureSessionV1>,
    pending: Vec<PendingCapture>,
    pub(crate) readbacks: u64,
    pub(crate) frames: u64,
}
pub(crate) struct PendingCapture {
    output: Output,
    buffer: WlBuffer,
    region: Rectangle<i32, Buffer>,
    cursors: bool,
    frame: FrameResource,
}
enum FrameResource {
    Modern(ExtImageCopyCaptureFrameV1),
    Legacy(ZwlrScreencopyFrameV1),
}

fn output_size(output: &Output) -> Option<Size<i32, Buffer>> {
    output.current_mode().map(|mode| {
        let size = output.current_transform().transform_size(mode.size);
        Size::from((size.w, size.h))
    })
}
fn buffer_matches(buffer: &WlBuffer, size: Size<i32, Buffer>, exact_stride: bool) -> bool {
    with_buffer_contents(buffer, |_, len, data| {
        buffer_layout(data, len, size, exact_stride).is_some()
    })
    .unwrap_or(false)
}
fn buffer_layout(
    data: BufferData,
    len: usize,
    size: Size<i32, Buffer>,
    exact_stride: bool,
) -> Option<(usize, usize)> {
    if data.format != wl_shm::Format::Argb8888
        || data.width != size.w
        || data.height != size.h
        || size.w <= 0
        || size.h <= 0
    {
        return None;
    }
    let row = usize::try_from(size.w).ok()?.checked_mul(4)?;
    let stride = usize::try_from(data.stride).ok()?;
    let offset = usize::try_from(data.offset).ok()?;
    if stride < row || (exact_stride && stride != row) {
        return None;
    }
    let end = offset
        .checked_add(stride.checked_mul(size.h as usize - 1)?)?
        .checked_add(row)?;
    (end <= len).then_some((offset, stride))
}
impl FrameResource {
    fn alive(&self) -> bool {
        match self {
            Self::Modern(frame) => frame.is_alive(),
            Self::Legacy(frame) => frame.is_alive(),
        }
    }
    fn failed(&self, stopped: bool) {
        match self {
            Self::Modern(frame) => frame.failed(if stopped {
                ext_image_copy_capture_frame_v1::FailureReason::Stopped
            } else {
                ext_image_copy_capture_frame_v1::FailureReason::Unknown
            }),
            Self::Legacy(frame) => frame.failed(),
        }
    }
    fn ready(&self, size: Size<i32, Buffer>) {
        let now: std::time::Duration = Clock::<Monotonic>::new().now().into();
        let secs = now.as_secs();
        let nanos = now.subsec_nanos();
        match self {
            Self::Modern(frame) => {
                frame.transform(wl_output::Transform::Normal);
                frame.damage(0, 0, size.w, size.h);
                frame.presentation_time((secs >> 32) as u32, secs as u32, nanos);
                frame.ready();
            }
            Self::Legacy(frame) => {
                frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
                if frame.version() >= 2 {
                    frame.damage(0, 0, size.w as u32, size.h as u32);
                }
                frame.ready((secs >> 32) as u32, secs as u32, nanos);
            }
        }
    }
}
impl CaptureState {
    pub(crate) fn new(dh: &DisplayHandle) -> Self {
        dh.create_global::<Anvil, ExtOutputImageCaptureSourceManagerV1, _>(1, ());
        dh.create_global::<Anvil, ExtImageCopyCaptureManagerV1, _>(1, ());
        dh.create_global::<Anvil, ZwlrScreencopyManagerV1, _>(3, ());
        Self {
            sessions: Vec::new(),
            pending: Vec::new(),
            readbacks: 0,
            frames: 0,
        }
    }
    pub(crate) fn refresh(&mut self, outputs: &[Output], locked: bool) {
        self.sessions.retain(Resource::is_alive);
        for session in &self.sessions {
            let data = session.data::<Arc<CaptureSession>>().unwrap();
            if *data.stopped.lock().unwrap() {
                continue;
            }
            let size = data
                .source
                .output
                .as_ref()
                .filter(|output| outputs.contains(output))
                .and_then(output_size);
            if locked || size.is_none() {
                *data.stopped.lock().unwrap() = true;
                session.stopped();
                continue;
            }
            let mut known = data.size.lock().unwrap();
            if *known != size {
                *known = size;
                let size = size.unwrap();
                session.buffer_size(size.w as u32, size.h as u32);
                session.shm_format(wl_shm::Format::Argb8888);
                session.done();
            }
        }
        self.pending.retain(|request| {
            if !request.frame.alive() {
                return false;
            }
            if locked || !outputs.contains(&request.output) {
                request.frame.failed(true);
                return false;
            }
            true
        });
    }
    pub(crate) fn needs_output(&self, output: &Output) -> bool {
        self.pending.iter().any(|request| &request.output == output)
    }
    pub(crate) fn paint_cursors(&self, output: &Output) -> bool {
        self.pending
            .iter()
            .any(|request| &request.output == output && request.cursors)
    }
    pub(crate) fn render<R, E>(
        &mut self,
        renderer: &mut R,
        output: &Output,
        elements: &[E],
        background: [f32; 4],
    ) where
        R: ExportMem + Offscreen<GlesRenderbuffer>,
        E: RenderElement<R>,
    {
        // Share only this output's current scene. Different cursor variants are processed
        // sequentially, so another capture never retains a second full-output pixel copy.
        let has_cursor = elements
            .iter()
            .any(|element| element.kind() == Kind::Cursor);
        let mut batches = [Vec::new(), Vec::new()];
        let mut index = 0;
        while index < self.pending.len() {
            if &self.pending[index].output == output {
                let request = self.pending.swap_remove(index);
                if request.frame.alive() {
                    batches[usize::from(request.cursors && has_cursor)].push(request);
                }
            } else {
                index += 1;
            }
        }
        for (cursors, requests) in batches.into_iter().enumerate() {
            if requests.is_empty() {
                continue;
            }
            let mut completed = 0;
            let result = render_pixels(
                renderer,
                output,
                elements,
                background,
                cursors != 0,
                |size, pixels, flipped| {
                    for request in &requests {
                        if request.complete(size, pixels, flipped) {
                            completed += 1;
                        }
                    }
                },
            );
            if let Err(error) = result {
                tracing::warn!(%error, "output capture failed");
                for request in &requests {
                    request.frame.failed(false);
                }
            } else {
                self.readbacks = self.readbacks.saturating_add(1);
                self.frames = self.frames.saturating_add(completed);
            }
        }
    }
}

impl PendingCapture {
    fn complete(&self, size: Size<i32, Buffer>, pixels: &[u8], flipped: bool) -> bool {
        if !self.frame.alive() {
            return false;
        }
        let region = self.region;
        if let FrameResource::Modern(frame) = &self.frame {
            if region.size != size {
                frame.failed(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints);
                return false;
            }
        }
        let Some(rows) = PixelRows::new(size, region, pixels.len(), flipped) else {
            self.frame.failed(false);
            return false;
        };
        let copied = with_buffer_contents_mut(&self.buffer, |ptr, len, data| {
            let Some((offset, stride)) = buffer_layout(
                data,
                len,
                region.size,
                matches!(self.frame, FrameResource::Legacy(_)),
            ) else {
                return false;
            };
            for (row, src) in rows.enumerate() {
                // SAFETY: PixelRows validates source ranges, buffer_layout validates SHM
                // ranges, and pointers remain local to the guarded SHM callback.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        pixels.as_ptr().add(src.start),
                        ptr.add(offset + row * stride),
                        src.len(),
                    );
                }
            }
            true
        })
        .unwrap_or(false);
        if copied {
            self.frame.ready(region.size);
        } else {
            self.frame.failed(false);
        }
        copied
    }
}

/// Validate once, then read cropped rows directly from the mapped renderer memory.
/// Vertical normalization needs no intermediate full-output allocation or flip copy.
struct PixelRows {
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    row_bytes: usize,
    rows: usize,
    flipped: bool,
}
impl PixelRows {
    fn new(
        size: Size<i32, Buffer>,
        region: Rectangle<i32, Buffer>,
        len: usize,
        flipped: bool,
    ) -> Option<Self> {
        if size.w <= 0 || size.h <= 0 || region.size.w <= 0 || region.size.h <= 0 {
            return None;
        }
        let width = usize::try_from(size.w).ok()?;
        let height = usize::try_from(size.h).ok()?;
        let x = usize::try_from(region.loc.x).ok()?;
        let y = usize::try_from(region.loc.y).ok()?;
        let region_width = region.size.w as usize;
        let rows = region.size.h as usize;
        if x.checked_add(region_width)? > width
            || y.checked_add(rows)? > height
            || width.checked_mul(height)?.checked_mul(4)? > len
        {
            return None;
        }
        Some(Self {
            width,
            height,
            x,
            y,
            row_bytes: region_width * 4,
            rows,
            flipped,
        })
    }
}
impl Iterator for PixelRows {
    type Item = std::ops::Range<usize>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.rows == 0 {
            return None;
        }
        let y = if self.flipped {
            self.height - 1 - self.y
        } else {
            self.y
        };
        let start = (y * self.width + self.x) * 4;
        self.y += 1;
        self.rows -= 1;
        Some(start..start + self.row_bytes)
    }
}

fn render_pixels<R, E>(
    renderer: &mut R,
    output: &Output,
    elements: &[E],
    background: [f32; 4],
    cursors: bool,
    consume: impl FnOnce(Size<i32, Buffer>, &[u8], bool),
) -> Result<(), String>
where
    R: ExportMem + Offscreen<GlesRenderbuffer>,
    E: RenderElement<R>,
{
    let size = output_size(output).ok_or("capture output has no mode")?;
    let mut target = renderer
        .create_buffer(Fourcc::Argb8888, size)
        .map_err(|error| error.to_string())?;
    let mut framebuffer = renderer
        .bind(&mut target)
        .map_err(|error| error.to_string())?;
    let mut damage = OutputDamageTracker::new(
        Size::<i32, Physical>::from((size.w, size.h)),
        output.current_scale().fractional_scale(),
        Transform::Normal,
    );
    let selected: Vec<_> = elements
        .iter()
        .filter(|element| cursors || element.kind() != Kind::Cursor)
        .collect();
    let result = damage
        .render_output(renderer, &mut framebuffer, 0, &selected, background)
        .map_err(|error| error.to_string())?;
    result.sync.wait().map_err(|error| error.to_string())?;
    let mapping = renderer
        .copy_framebuffer(&framebuffer, Rectangle::from_size(size), Fourcc::Argb8888)
        .map_err(|error| error.to_string())?;
    let flipped = mapping.flipped();
    let bytes = renderer
        .map_texture(&mapping)
        .map_err(|error| error.to_string())?;
    consume(size, bytes, flipped);
    Ok(())
}

macro_rules! manager_global {
    ($manager:ty) => {
        impl GlobalDispatch<$manager, ()> for Anvil {
            fn bind(
                _: &mut Self,
                _: &DisplayHandle,
                _: &Client,
                resource: New<$manager>,
                _: &(),
                init: &mut DataInit<'_, Self>,
            ) {
                init.init(resource, ());
            }
        }
    };
}
manager_global!(ExtOutputImageCaptureSourceManagerV1);
manager_global!(ExtImageCopyCaptureManagerV1);
manager_global!(ZwlrScreencopyManagerV1);

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for Anvil {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtOutputImageCaptureSourceManagerV1,
        request: ext_output_image_capture_source_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let ext_output_image_capture_source_manager_v1::Request::CreateSource {
            source,
            output,
        } = request
        {
            let output = Output::from_resource(&output)
                .filter(|output| state.space.outputs().any(|known| known == output));
            init.init(source, CaptureSource { output });
        }
    }
}
impl Dispatch<ExtImageCaptureSourceV1, CaptureSource> for Anvil {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtImageCaptureSourceV1,
        _: ext_image_capture_source_v1::Request,
        _: &CaptureSource,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
fn create_session(
    state: &mut Anvil,
    resource: New<ExtImageCopyCaptureSessionV1>,
    source: CaptureSource,
    cursors: bool,
    init: &mut DataInit<'_, Anvil>,
) {
    let data = Arc::new(CaptureSession {
        source,
        cursors,
        size: Mutex::new(None),
        stopped: Mutex::new(false),
        frame: Mutex::new(None),
    });
    let session = init.init(resource, data);
    state.capture_state.sessions.push(session);
    state.refresh_capture();
}
impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for Anvil {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &ExtImageCopyCaptureManagerV1,
        request: ext_image_copy_capture_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_image_copy_capture_manager_v1::Request::CreateSession {
                session,
                source,
                options,
            } => {
                let options = match options {
                    WEnum::Value(options) => options,
                    _ => {
                        resource.post_error(
                            ext_image_copy_capture_manager_v1::Error::InvalidOption,
                            "unknown capture options",
                        );
                        return;
                    }
                };
                create_session(
                    state,
                    session,
                    source.data::<CaptureSource>().unwrap().clone(),
                    options.contains(ext_image_copy_capture_manager_v1::Options::PaintCursors),
                    init,
                );
            }
            ext_image_copy_capture_manager_v1::Request::CreatePointerCursorSession {
                session,
                ..
            } => {
                init.init(session, CursorSession::default());
            }
            _ => {}
        }
    }
}
impl Dispatch<ExtImageCopyCaptureCursorSessionV1, CursorSession> for Anvil {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &ExtImageCopyCaptureCursorSessionV1,
        request: ext_image_copy_capture_cursor_session_v1::Request,
        data: &CursorSession,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let ext_image_copy_capture_cursor_session_v1::Request::GetCaptureSession { session } =
            request
        {
            let mut requested = data.requested.lock().unwrap();
            if *requested {
                resource.post_error(
                    ext_image_copy_capture_cursor_session_v1::Error::DuplicateSession,
                    "cursor session already created",
                );
                return;
            }
            *requested = true;
            // Independent cursor-stream capture is unavailable; advertise a stopped session.
            create_session(state, session, CaptureSource { output: None }, false, init);
        }
    }
}
impl Dispatch<ExtImageCopyCaptureSessionV1, Arc<CaptureSession>> for Anvil {
    fn request(
        _: &mut Self,
        _: &Client,
        resource: &ExtImageCopyCaptureSessionV1,
        request: ext_image_copy_capture_session_v1::Request,
        data: &Arc<CaptureSession>,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let ext_image_copy_capture_session_v1::Request::CreateFrame { frame } = request {
            let mut current = data.frame.lock().unwrap();
            if current.as_ref().is_some_and(Resource::is_alive) {
                resource.post_error(
                    ext_image_copy_capture_session_v1::Error::DuplicateFrame,
                    "destroy previous frame first",
                );
                return;
            }
            let frame = init.init(
                frame,
                CaptureFrame {
                    session: data.clone(),
                    buffer: Mutex::new(None),
                    captured: Mutex::new(false),
                },
            );
            *current = Some(frame);
        }
    }
}
impl Dispatch<ExtImageCopyCaptureFrameV1, CaptureFrame> for Anvil {
    fn destroyed(
        _: &mut Self,
        _: smithay::reexports::wayland_server::backend::ClientId,
        frame: &ExtImageCopyCaptureFrameV1,
        data: &CaptureFrame,
    ) {
        let mut current = data.session.frame.lock().unwrap();
        if current.as_ref() == Some(frame) {
            *current = None;
        }
    }
    fn request(
        state: &mut Self,
        _: &Client,
        frame: &ExtImageCopyCaptureFrameV1,
        request: ext_image_copy_capture_frame_v1::Request,
        data: &CaptureFrame,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use ext_image_copy_capture_frame_v1::{Error, FailureReason, Request};
        if matches!(request, Request::Destroy) {
            return;
        }
        let mut captured = data.captured.lock().unwrap();
        if *captured {
            frame.post_error(Error::AlreadyCaptured, "frame already submitted");
            return;
        }
        match request {
            Request::AttachBuffer { buffer } => *data.buffer.lock().unwrap() = Some(buffer),
            Request::DamageBuffer {
                x,
                y,
                width,
                height,
            } => {
                if x < 0 || y < 0 || width <= 0 || height <= 0 {
                    frame.post_error(Error::InvalidBufferDamage, "invalid buffer damage");
                }
            }
            Request::Capture => {
                *captured = true;
                let Some(buffer) = data.buffer.lock().unwrap().clone() else {
                    frame.post_error(Error::NoBuffer, "attach a buffer first");
                    return;
                };
                state.refresh_capture();
                if *data.session.stopped.lock().unwrap() {
                    frame.failed(FailureReason::Stopped);
                    return;
                }
                let output = data.session.source.output.clone().unwrap();
                let size = data.session.size.lock().unwrap().unwrap();
                if !buffer_matches(&buffer, size, false) {
                    frame.failed(FailureReason::BufferConstraints);
                    return;
                }
                state.capture_state.pending.push(PendingCapture {
                    output,
                    buffer,
                    region: Rectangle::from_size(size),
                    cursors: data.session.cursors,
                    frame: FrameResource::Modern(frame.clone()),
                });
                state.request_capture_repaint();
            }
            _ => {}
        }
    }
}
impl Dispatch<ZwlrScreencopyManagerV1, ()> for Anvil {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_screencopy_manager_v1::Request;
        let (resource, requested, cursors, requested_region) = match request {
            Request::CaptureOutput {
                frame,
                output,
                overlay_cursor,
            } => (frame, output, overlay_cursor != 0, None),
            Request::CaptureOutputRegion {
                frame,
                output,
                overlay_cursor,
                x,
                y,
                width,
                height,
            } => (
                frame,
                output,
                overlay_cursor != 0,
                Some((x, y, width, height)),
            ),
            _ => return,
        };
        let output = Output::from_resource(&requested)
            .filter(|output| state.space.outputs().any(|known| known == output));
        let region = output.as_ref().and_then(|output| {
            let full = Rectangle::from_size(output_size(output)?);
            match requested_region {
                None => Some(full),
                Some((x, y, w, h)) if w > 0 && h > 0 => {
                    let scale = output.current_scale().fractional_scale();
                    let left = (f64::from(x) * scale)
                        .round()
                        .clamp(0.0, f64::from(full.size.w)) as i32;
                    let top = (f64::from(y) * scale)
                        .round()
                        .clamp(0.0, f64::from(full.size.h)) as i32;
                    let right = ((f64::from(x) + f64::from(w)) * scale)
                        .round()
                        .clamp(0.0, f64::from(full.size.w)) as i32;
                    let bottom = ((f64::from(y) + f64::from(h)) * scale)
                        .round()
                        .clamp(0.0, f64::from(full.size.h)) as i32;
                    (right > left && bottom > top).then(|| {
                        Rectangle::new((left, top).into(), (right - left, bottom - top).into())
                    })
                }
                _ => None,
            }
        });
        let frame = init.init(
            resource,
            LegacyFrame {
                output,
                region,
                cursors,
                copied: Mutex::new(false),
            },
        );
        if state.session_locked() || region.is_none() {
            frame.failed();
            return;
        }
        let size = region.unwrap().size;
        frame.buffer(
            wl_shm::Format::Argb8888,
            size.w as u32,
            size.h as u32,
            size.w as u32 * 4,
        );
        if frame.version() >= 3 {
            frame.buffer_done();
        }
    }
}
impl Dispatch<ZwlrScreencopyFrameV1, LegacyFrame> for Anvil {
    fn request(
        state: &mut Self,
        _: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &LegacyFrame,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use zwlr_screencopy_frame_v1::{Error, Request};
        let buffer = match request {
            Request::Copy { buffer } | Request::CopyWithDamage { buffer } => buffer,
            _ => return,
        };
        let mut copied = data.copied.lock().unwrap();
        if *copied {
            frame.post_error(Error::AlreadyUsed, "frame already submitted");
            return;
        }
        *copied = true;
        let (Some(output), Some(region)) = (data.output.clone(), data.region) else {
            frame.failed();
            return;
        };
        if state.session_locked() || !state.space.outputs().any(|known| known == &output) {
            frame.failed();
            return;
        }
        if !buffer_matches(&buffer, region.size, true) {
            frame.post_error(
                Error::InvalidBuffer,
                "buffer must match advertised ARGB8888 dimensions and stride",
            );
            return;
        }
        state.capture_state.pending.push(PendingCapture {
            output,
            buffer,
            region,
            cursors: data.cursors,
            frame: FrameResource::Legacy(frame.clone()),
        });
        state.request_capture_repaint();
    }
}
impl Anvil {
    pub(crate) fn refresh_capture(&mut self) {
        self.capture_state.refresh(
            &self.space.outputs().cloned().collect::<Vec<_>>(),
            self.session_locked(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cropped_rows_normalize_orientation_without_an_intermediate_image() {
        let size = (4, 3).into();
        let region = Rectangle::new((1, 1).into(), (2, 2).into());
        assert_eq!(
            PixelRows::new(size, region, 48, false)
                .unwrap()
                .collect::<Vec<_>>(),
            vec![20..28, 36..44]
        );
        assert_eq!(
            PixelRows::new(size, region, 48, true)
                .unwrap()
                .collect::<Vec<_>>(),
            vec![20..28, 4..12]
        );
        assert!(PixelRows::new(size, region, 47, false).is_none());
        for invalid in [
            Rectangle::new((-1, 0).into(), (2, 2).into()),
            Rectangle::new((0, -1).into(), (2, 2).into()),
            Rectangle::new((3, 0).into(), (2, 2).into()),
            Rectangle::new((0, 2).into(), (2, 2).into()),
            Rectangle::new((0, 0).into(), (0, 2).into()),
            Rectangle::new((i32::MAX, 0).into(), (i32::MAX, 1).into()),
        ] {
            assert!(PixelRows::new(size, invalid, 48, false).is_none());
        }
    }

    #[test]
    fn validates_stride_offset_and_last_row_bounds() {
        let data = BufferData {
            offset: 8,
            width: 2,
            height: 2,
            stride: 12,
            format: wl_shm::Format::Argb8888,
        };
        assert_eq!(buffer_layout(data, 28, (2, 2).into(), false), Some((8, 12)));
        assert!(buffer_layout(data, 27, (2, 2).into(), false).is_none());
        assert!(buffer_layout(data, 28, (2, 2).into(), true).is_none());
        assert!(
            buffer_layout(
                BufferData { stride: -1, ..data },
                usize::MAX,
                (2, 2).into(),
                false
            )
            .is_none()
        );
        assert!(
            buffer_layout(BufferData { width: 3, ..data }, 100, (2, 2).into(), false).is_none()
        );
    }
}
