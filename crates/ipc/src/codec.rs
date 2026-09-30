//! Length-prefixed framing: `u32` LE length, then postcard of a [`Frame`].

use std::io::{self, Read, Write};

use crate::{Frame, MAX_FRAME};

#[derive(Debug)]
pub enum FrameError {
    /// Declared or encoded length exceeds [`MAX_FRAME`]. On the decode side the stream is
    /// desynchronized and must be closed; the decoder refuses further frames.
    TooLarge(usize),
    /// The body did not decode (garbage, or a variant from a newer protocol). The length
    /// prefix was honored, so the stream is still in sync and decoding may continue.
    Decode(postcard::Error),
    /// Encoding failed.
    Encode(postcard::Error),
    Io(io::Error),
    /// The peer closed the stream in the middle of a frame.
    Truncated,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(n) => write!(f, "frame of {n} bytes exceeds the {MAX_FRAME} byte cap"),
            Self::Decode(e) => write!(f, "undecodable frame: {e}"),
            Self::Encode(e) => write!(f, "cannot encode frame: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Truncated => write!(f, "stream ended mid-frame"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Appends the framed encoding of `frame` to `out`. On error `out` is unchanged.
pub fn encode_into(frame: &Frame, out: &mut Vec<u8>) -> Result<(), FrameError> {
    let body = postcard::to_stdvec(frame).map_err(FrameError::Encode)?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(())
}

/// The framed encoding of `frame`, ready to write to a socket.
pub fn encode(frame: &Frame) -> Result<Vec<u8>, FrameError> {
    let mut out = Vec::new();
    encode_into(frame, &mut out)?;
    Ok(out)
}

/// Incremental decoder for nonblocking sockets: [`feed`](Decoder::feed) whatever bytes
/// arrived, then call [`next_frame`](Decoder::next_frame) until it returns `Ok(None)`.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// Start of unconsumed data in `buf`.
    pos: usize,
    poisoned: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes buffered but not yet returned as frames.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// True after an oversize header: the stream cannot be resynchronized.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        if self.poisoned {
            return;
        }
        if self.pos > 0 && self.pos >= self.buf.len() / 2 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Pops the next complete frame. `Ok(None)` means more bytes are needed.
    /// `Err(TooLarge)` poisons the decoder (close the connection); `Err(Decode)` consumed
    /// the bad frame, so the caller may keep going or drop the client.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        if self.poisoned {
            return Err(FrameError::TooLarge(0));
        }
        let avail = &self.buf[self.pos..];
        let Some(header) = avail.first_chunk::<4>() else {
            return Ok(None);
        };
        let len = u32::from_le_bytes(*header) as usize;
        if len > MAX_FRAME {
            self.poisoned = true;
            self.buf = Vec::new();
            self.pos = 0;
            return Err(FrameError::TooLarge(len));
        }
        let Some(body) = avail.get(4..4 + len) else {
            return Ok(None);
        };
        let result = postcard::from_bytes::<Frame>(body);
        self.pos += 4 + len;
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
        match result {
            Ok(frame) => Ok(Some(frame)),
            Err(e) => Err(FrameError::Decode(e)),
        }
    }
}

/// Blocking: writes one frame and flushes.
pub fn write_frame<W: Write>(w: &mut W, frame: &Frame) -> Result<(), FrameError> {
    w.write_all(&encode(frame)?)?;
    w.flush()?;
    Ok(())
}

/// Blocking: reads exactly one frame. `Ok(None)` is a clean EOF between frames.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Option<Frame>, FrameError> {
    let mut header = [0u8; 4];
    match r.read(&mut header[..1])? {
        0 => return Ok(None),
        _ => read_full(r, &mut header[1..])?,
    }
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    read_full(r, &mut body)?;
    postcard::from_bytes(&body)
        .map(Some)
        .map_err(FrameError::Decode)
}

fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), FrameError> {
    r.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => FrameError::Truncated,
        _ => FrameError::Io(e),
    })
}
