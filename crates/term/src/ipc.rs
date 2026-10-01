//! Nonblocking IPC connection to the compositor, for the live theme only.
//!
//! Same shape as the shell's: [`Conn`] owns the socket, the decoder and an output
//! buffer and knows nothing about the event loop; the app registers a clone of
//! [`Conn::stream`] as a calloop source and calls [`Conn::read`] when it is readable.
//! The terminal works without it: with no socket it reads `theme.toml` once.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use aurora_ipc::{
    Body, Decoder, Frame, FrameError, Request, Topic, check_first_frame, encode_into,
};

/// Unsent bytes beyond which the compositor is considered stuck and we reconnect.
const MAX_OUT: usize = 64 * 1024;

pub struct Conn {
    stream: UnixStream,
    decoder: Decoder,
    out: Vec<u8>,
    greeted: bool,
}

impl Conn {
    /// Connects, then queues the hello, the theme subscription and a theme request.
    pub fn connect(path: &Path) -> io::Result<Self> {
        Self::from_stream(UnixStream::connect(path)?)
    }

    pub fn from_stream(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        let mut c = Self {
            stream,
            decoder: Decoder::new(),
            out: Vec::new(),
            greeted: false,
        };
        c.queue(&Frame::hello("term"))?;
        c.queue(&Frame::new(0, Body::Subscribe(vec![Topic::Theme])))?;
        c.queue(&Frame::request(1, Request::GetTheme))?;
        c.flush()?;
        Ok(c)
    }

    /// A second handle to the same socket, for registering with the event loop.
    pub fn stream(&self) -> io::Result<UnixStream> {
        self.stream.try_clone()
    }

    fn queue(&mut self, frame: &Frame) -> io::Result<()> {
        if self.out.len() > MAX_OUT {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "output backlog"));
        }
        encode_into(frame, &mut self.out).map_err(io::Error::other)
    }

    fn flush(&mut self) -> io::Result<()> {
        while !self.out.is_empty() {
            match self.stream.write(&self.out) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Drains the socket and returns the complete frames. `Err` means the connection is
    /// over (EOF, I/O error, version mismatch, desynchronized stream).
    pub fn read(&mut self) -> io::Result<Vec<Frame>> {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let mut frames = Vec::new();
        loop {
            match self.decoder.next_frame() {
                Ok(Some(f)) => {
                    if !self.greeted {
                        check_first_frame(&f).map_err(io::Error::other)?;
                        self.greeted = true;
                    }
                    frames.push(f);
                }
                Ok(None) => break,
                Err(FrameError::Decode(e)) => {
                    tracing::warn!("term: skipping undecodable ipc frame: {e}");
                }
                Err(e) => return Err(io::Error::other(e)),
            }
        }
        self.flush()?;
        Ok(frames)
    }
}

/// Reconnect delays: 250 ms doubling to a 5 s cap, restarted by [`Backoff::reset`].
#[derive(Debug, Default)]
pub struct Backoff {
    attempts: u32,
}

impl Backoff {
    pub fn next_delay(&mut self) -> Duration {
        let ms = (250u64 << self.attempts.min(5)).min(5000);
        self.attempts = self.attempts.saturating_add(1);
        Duration::from_millis(ms)
    }

    pub fn reset(&mut self) {
        self.attempts = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurora_ipc::{Event, ThemeSnapshot, read_frame, write_frame};

    #[test]
    fn backoff_doubles_to_a_cap_and_resets() {
        let mut b = Backoff::default();
        let ms: Vec<u128> = (0..8).map(|_| b.next_delay().as_millis()).collect();
        assert_eq!(ms, [250, 500, 1000, 2000, 4000, 5000, 5000, 5000]);
        b.reset();
        assert_eq!(b.next_delay().as_millis(), 250);
    }

    /// An anonymous socketpair stands in for the compositor; nothing touches the real
    /// IPC socket.
    #[test]
    fn handshake_subscribes_to_the_theme_and_receives_pushes() {
        let (client, mut server) = UnixStream::pair().expect("pair");
        let mut conn = Conn::from_stream(client).expect("conn");

        let hello = read_frame(&mut server).expect("read").expect("hello");
        assert!(matches!(hello.body, Body::Hello(_)));
        let sub = read_frame(&mut server).expect("read").expect("subscribe");
        assert_eq!(sub.body, Body::Subscribe(vec![Topic::Theme]));
        let get = read_frame(&mut server).expect("read").expect("request");
        assert_eq!((get.id, get.body), (1, Body::Request(Request::GetTheme)));

        assert!(conn.read().expect("empty read").is_empty());
        write_frame(&mut server, &Frame::hello("aurora-comp")).expect("hello");
        let snap = ThemeSnapshot::new(7, Default::default());
        write_frame(&mut server, &Frame::event(Event::Theme(snap))).expect("theme");
        let frames = conn.read().expect("frames");
        assert_eq!(frames.len(), 2);
        assert!(matches!(&frames[1].body, Body::Event(Event::Theme(t)) if t.rev == 7));

        drop(server);
        assert!(conn.read().is_err());
    }

    #[test]
    fn a_first_frame_that_is_not_hello_ends_the_connection() {
        let (client, mut server) = UnixStream::pair().expect("pair");
        let mut conn = Conn::from_stream(client).expect("conn");
        let snap = ThemeSnapshot::new(1, Default::default());
        write_frame(&mut server, &Frame::event(Event::Theme(snap))).expect("frame");
        assert!(conn.read().is_err());
    }
}
