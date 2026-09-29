//! Screen capture (ext-image-copy-capture, what grim uses). Outputs only, shm buffers only.
//!
//! A frame request queues a redraw of its output and waits in `pending`; the render loop of
//! that output calls `serve` with the elements it is about to draw, so the copy always shows
//! what the output shows. The copy goes through the CPU, which suits screenshots; a client
//! streaming video from this path would cost a full-frame readback per frame.
use std::time::Duration;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, Offscreen,
            damage::OutputDamageTracker,
            gles::{GlesRenderer, GlesTexture},
        },
    },
    output::{Output, WeakOutput},
    reexports::wayland_server::protocol::wl_shm,
    utils::{Buffer, Rectangle, Size, Transform},
    wayland::{
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, ImageCaptureSourceState,
            OutputCaptureSourceHandler, OutputCaptureSourceState,
        },
        image_copy_capture::{
            BufferConstraints, CaptureFailureReason, Frame, FrameRef, ImageCopyCaptureHandler,
            ImageCopyCaptureState, Session, SessionRef,
        },
        shm::{BufferData, with_buffer_contents_mut},
    },
};

use crate::{backend::BACKGROUND, scene::OutputElement, state::Aurora};

/// Sessions and the frames waiting for their output's next render.
pub struct Captures {
    _source: ImageCaptureSourceState,
    output_source: OutputCaptureSourceState,
    copy: ImageCopyCaptureState,
    sessions: Vec<Session>,
    pending: Vec<Pending>,
}

struct Pending {
    output: WeakOutput,
    frame: Frame,
    cursor: bool,
}

impl Captures {
    pub fn new(dh: &smithay::reexports::wayland_server::DisplayHandle) -> Self {
        Self {
            _source: ImageCaptureSourceState::new(),
            output_source: OutputCaptureSourceState::new::<Aurora>(dh),
            copy: ImageCopyCaptureState::new::<Aurora>(dh),
            sessions: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Whether some frame waits for `output`.
    pub fn wants(&self, output: &Output) -> bool {
        self.pending
            .iter()
            .any(|p| p.output.upgrade().as_ref() == Some(output))
    }

    /// The output is gone: its sessions end and their waiting frames fail.
    pub fn output_removed(&mut self, output: &Output) {
        self.pending
            .retain(|p| p.output.upgrade().is_some_and(|o| &o != output));
        self.sessions
            .retain(|s| session_output(s).is_none_or(|o| &o != output));
    }

    /// The output's mode changed: sessions on it announce the new buffer size.
    pub fn output_changed(&mut self, output: &Output) {
        for session in &self.sessions {
            if session_output(session).as_ref() == Some(output)
                && let Some(constraints) = constraints_for(output)
                && session
                    .current_constraints()
                    .is_none_or(|c| c.size != constraints.size)
            {
                session.update_constraints(constraints);
            }
        }
    }
}

fn source_output(source: &ImageCaptureSource) -> Option<Output> {
    source.user_data().get::<WeakOutput>()?.upgrade()
}

fn session_output(session: &SessionRef) -> Option<Output> {
    source_output(&session.source())
}

fn constraints_for(output: &Output) -> Option<BufferConstraints> {
    let mode = output.current_mode()?;
    Some(BufferConstraints {
        size: Size::<i32, Buffer>::from((mode.size.w, mode.size.h)),
        shm: vec![wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888],
        dma: None,
    })
}

impl ImageCaptureSourceHandler for Aurora {}

impl OutputCaptureSourceHandler for Aurora {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.captures.output_source
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}

impl ImageCopyCaptureHandler for Aurora {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.captures.copy
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        constraints_for(&source_output(source)?)
    }

    fn new_session(&mut self, session: Session) {
        self.captures.sessions.push(session);
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        let Some(output) = session_output(session) else {
            return frame.fail(CaptureFailureReason::Stopped);
        };
        self.captures.pending.push(Pending {
            output: output.downgrade(),
            frame,
            cursor: session.draw_cursor(),
        });
        // Forces a render even when nothing on the output changed.
        self.queue_redraw_output(&output);
    }

    fn frame_aborted(&mut self, frame: FrameRef) {
        self.captures.pending.retain(|p| p.frame != frame);
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.captures.sessions.retain(|s| *s != session);
    }
}

/// Answers the frames waiting for `output`. `elements` is what the output is about to draw,
/// the first `n_cursor` of them the pointer. Any failure fails the frame, never the render.
pub fn serve(
    captures: &mut Captures,
    renderer: &mut GlesRenderer,
    output: &Output,
    elements: &[OutputElement],
    n_cursor: usize,
    now: Duration,
) {
    if !captures.wants(output) {
        return;
    }
    let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut captures.pending)
        .into_iter()
        .partition(|p| p.output.upgrade().as_ref() == Some(output));
    captures.pending = rest;

    // The image is the same for every client except for the pointer, so render at most twice.
    let mut images: [Option<Result<Vec<u8>, String>>; 2] = [None, None];
    for p in mine {
        let slot = &mut images[usize::from(p.cursor)];
        let image = slot.get_or_insert_with(|| {
            let scene = if p.cursor {
                elements
            } else {
                &elements[n_cursor.min(elements.len())..]
            };
            render(renderer, output, scene).map_err(|err| err.to_string())
        });
        let result = match image {
            Ok(pixels) => copy_into(&p.frame, output, pixels),
            Err(err) => Err(err.clone()),
        };
        match result {
            Ok(size) => {
                let full = Rectangle::from_size(size);
                p.frame.success(Transform::Normal, Some(vec![full]), now);
            }
            Err(err) => {
                tracing::warn!("capture: {err}");
                p.frame.fail(CaptureFailureReason::Unknown);
            }
        }
    }
}

/// The output as tightly packed RGBA rows, top row first.
fn render(
    renderer: &mut GlesRenderer,
    output: &Output,
    elements: &[OutputElement],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mode = output.current_mode().ok_or("output has no mode")?;
    let size = Size::<i32, Buffer>::from((mode.size.w, mode.size.h));
    let mut texture: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, size)?;
    // Fresh tracker: the output's own one carries damage history and the winit output's
    // flipped transform, neither of which belongs in a screenshot.
    let mut tracker = OutputDamageTracker::new(
        mode.size,
        output.current_scale().fractional_scale(),
        Transform::Normal,
    );
    let mut target = renderer.bind(&mut texture)?;
    tracker
        .render_output(renderer, &mut target, 0, elements, BACKGROUND)
        .map_err(|err| format!("{err:?}"))?;
    let mapping =
        renderer.copy_framebuffer(&target, Rectangle::from_size(size), Fourcc::Abgr8888)?;
    Ok(renderer.map_texture(&mapping)?.to_vec())
}

/// Writes `rgba` (the output's size, tightly packed) into the frame's shm buffer as
/// Argb8888/Xrgb8888. Returns the copied size.
fn copy_into(frame: &Frame, output: &Output, rgba: &[u8]) -> Result<Size<i32, Buffer>, String> {
    let mode = output.current_mode().ok_or("output has no mode")?;
    let (w, h) = (mode.size.w as usize, mode.size.h as usize);
    if rgba.len() != w * h * 4 {
        return Err("readback has the wrong size".into());
    }
    let buffer = frame.buffer();
    with_buffer_contents_mut(&buffer, |ptr, len, data: BufferData| {
        let opaque = match data.format {
            wl_shm::Format::Argb8888 => false,
            wl_shm::Format::Xrgb8888 => true,
            other => return Err(format!("unsupported buffer format {other:?}")),
        };
        let (offset, stride) = (data.offset.max(0) as usize, data.stride.max(0) as usize);
        let fits = data.width as usize >= w
            && data.height as usize >= h
            && stride >= w * 4
            && offset + stride * (h - 1) + w * 4 <= len;
        if !fits {
            return Err("buffer does not fit the output".into());
        }
        // Safety: the range was checked against the pool length above.
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        for (y, row) in rgba.chunks_exact(w * 4).enumerate() {
            let out = &mut dst[offset + y * stride..][..w * 4];
            for (o, p) in out.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                // Memory order is B, G, R, A for both formats.
                o[0] = p[2];
                o[1] = p[1];
                o[2] = p[0];
                o[3] = if opaque { 0xff } else { p[3] };
            }
        }
        Ok(())
    })
    .map_err(|err| format!("cannot access the buffer: {err}"))??;
    Ok(Size::from((w as i32, h as i32)))
}
