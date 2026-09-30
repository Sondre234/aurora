//! A client's outbound queue: bounded, with replies that must arrive and state updates that
//! may be merged or dropped. Pure (it only writes into a `Write`), so the policy is tested
//! without a socket.
//!
//! Two kinds of frames:
//! - reliable: replies, errors, the handshake. Never dropped, never reordered.
//! - latest: events about some piece of state, identified by a [`Key`]. A newer event for the
//!   same key replaces an unsent older one in place, and when the queue grows past
//!   [`SOFT_LIMIT`] every unsent one is dropped and the caller queues a fresh snapshot
//!   instead (a slow subscriber resyncs rather than growing without bound). Past
//!   [`HARD_LIMIT`] even that is not enough (the client does not read its replies) and the
//!   caller closes the connection.
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Write},
};

/// Unsent bytes above which droppable frames are discarded.
pub const SOFT_LIMIT: usize = 256 * 1024;
/// Unsent bytes above which, dropping done, the client is cut off.
pub const HARD_LIMIT: usize = 2 * 1024 * 1024;

/// What a "latest" event is about.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Snapshot,
    Output(String),
    Workspace(String, u32),
    Window(u64),
    Focus,
    Theme,
    Config,
}

/// What a push did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Push {
    Queued,
    /// Replaced an older unsent frame with the same key.
    Coalesced,
    /// The queue was over the soft limit: every droppable frame is gone and the caller
    /// should queue a full snapshot.
    Resync,
    /// Over the hard limit: close the connection.
    Close,
}

struct Item {
    seq: u64,
    data: Vec<u8>,
    key: Option<Key>,
}

pub struct OutQueue {
    items: VecDeque<Item>,
    /// Bytes of the front item already written.
    head_off: usize,
    /// Unsent bytes in all items.
    bytes: usize,
    /// The queued item for each key, by `seq`.
    latest: HashMap<Key, u64>,
    next_seq: u64,
    soft: usize,
    hard: usize,
}

impl Default for OutQueue {
    fn default() -> Self {
        Self::with_limits(SOFT_LIMIT, HARD_LIMIT)
    }
}

impl OutQueue {
    pub fn with_limits(soft: usize, hard: usize) -> Self {
        Self {
            items: VecDeque::new(),
            head_off: 0,
            bytes: 0,
            latest: HashMap::new(),
            next_seq: 0,
            soft,
            hard,
        }
    }

    /// Unsent bytes.
    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    /// Queues a frame that must be delivered in order.
    pub fn push_reliable(&mut self, data: Vec<u8>) -> Push {
        let seq = self.seq();
        self.bytes += data.len();
        self.items.push_back(Item {
            seq,
            data,
            key: None,
        });
        self.enforce(Push::Queued)
    }

    /// Queues an event about `key`, replacing an unsent earlier one about the same key.
    pub fn push_latest(&mut self, key: Key, data: Vec<u8>) -> Push {
        if let Some(seq) = self.latest.get(&key).copied()
            && let Ok(idx) = self.items.binary_search_by_key(&seq, |i| i.seq)
            // A frame that is partly on the wire cannot change.
            && !(idx == 0 && self.head_off > 0)
        {
            let item = &mut self.items[idx];
            self.bytes = self.bytes - item.data.len() + data.len();
            item.data = data;
            return self.enforce(Push::Coalesced);
        }
        let seq = self.seq();
        self.bytes += data.len();
        self.latest.insert(key.clone(), seq);
        self.items.push_back(Item {
            seq,
            data,
            key: Some(key),
        });
        self.enforce(Push::Queued)
    }

    /// Queues a full snapshot: it supersedes every unsent event, which are dropped first,
    /// so nothing older can land after it. The soft limit does not apply to it (a large
    /// state must still get through), only the hard one.
    pub fn push_snapshot(&mut self, data: Vec<u8>) -> Push {
        self.drop_droppable();
        let seq = self.seq();
        self.bytes += data.len();
        self.latest.insert(Key::Snapshot, seq);
        self.items.push_back(Item {
            seq,
            data,
            key: Some(Key::Snapshot),
        });
        if self.bytes > self.hard {
            Push::Close
        } else {
            Push::Queued
        }
    }

    fn enforce(&mut self, ok: Push) -> Push {
        if self.bytes <= self.soft {
            return ok;
        }
        let dropped = self.drop_droppable();
        if self.bytes > self.hard {
            Push::Close
        } else if dropped {
            Push::Resync
        } else {
            ok
        }
    }

    /// Removes every unsent "latest" frame. Returns whether there was any.
    fn drop_droppable(&mut self) -> bool {
        let head_started = self.head_off > 0;
        let mut dropped = false;
        let mut first = true;
        let mut freed = 0;
        self.items.retain(|item| {
            let keep = item.key.is_none() || (first && head_started);
            first = false;
            if !keep {
                freed += item.data.len();
                dropped = true;
            }
            keep
        });
        self.bytes -= freed;
        // Only a started head may still carry a key.
        let live: Vec<u64> = self.items.iter().map(|i| i.seq).collect();
        self.latest.retain(|_, seq| live.contains(seq));
        dropped
    }

    /// Writes as much as `w` takes. `Ok(true)` means everything was written, `Ok(false)`
    /// that `w` would block.
    pub fn flush<W: Write>(&mut self, w: &mut W) -> io::Result<bool> {
        while let Some(item) = self.items.front() {
            match w.write(&item.data[self.head_off..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.head_off += n;
                    self.bytes -= n;
                    if self.head_off == item.data.len() {
                        if let Some(done) = self.items.pop_front()
                            && let Some(key) = done.key
                            && self.latest.get(&key) == Some(&done.seq)
                        {
                            self.latest.remove(&key);
                        }
                        self.head_off = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that takes `cap` bytes in total, then blocks.
    struct Sink {
        got: Vec<u8>,
        cap: usize,
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let room = self.cap.saturating_sub(self.got.len());
            if room == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = room.min(buf.len());
            self.got.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn drain(q: &mut OutQueue) -> Vec<u8> {
        let mut sink = Sink {
            got: Vec::new(),
            cap: usize::MAX,
        };
        assert!(q.flush(&mut sink).unwrap());
        sink.got
    }

    #[test]
    fn frames_come_out_in_order() {
        let mut q = OutQueue::default();
        q.push_reliable(b"aa".to_vec());
        q.push_latest(Key::Focus, b"bb".to_vec());
        q.push_reliable(b"cc".to_vec());
        assert_eq!(q.bytes(), 6);
        assert_eq!(drain(&mut q), b"aabbcc");
        assert!(q.is_empty());
        assert_eq!(q.bytes(), 0);
    }

    #[test]
    fn a_newer_event_replaces_an_unsent_one_in_place() {
        let mut q = OutQueue::default();
        q.push_latest(Key::Window(1), b"w1-old".to_vec());
        q.push_latest(Key::Window(2), b"w2".to_vec());
        assert_eq!(
            q.push_latest(Key::Window(1), b"w1-new!".to_vec()),
            Push::Coalesced
        );
        assert_eq!(q.bytes(), 7 + 2);
        assert_eq!(drain(&mut q), b"w1-new!w2");
    }

    #[test]
    fn distinct_keys_are_not_merged() {
        let mut q = OutQueue::default();
        q.push_latest(Key::Window(1), b"a".to_vec());
        q.push_latest(Key::Window(2), b"b".to_vec());
        q.push_latest(Key::Workspace("A".into(), 1), b"c".to_vec());
        q.push_latest(Key::Workspace("B".into(), 1), b"d".to_vec());
        assert_eq!(drain(&mut q), b"abcd");
    }

    #[test]
    fn a_partly_written_frame_is_never_replaced() {
        let mut q = OutQueue::default();
        q.push_latest(Key::Focus, b"0123456789".to_vec());
        let mut sink = Sink {
            got: Vec::new(),
            cap: 4,
        };
        assert!(!q.flush(&mut sink).unwrap());
        assert_eq!(q.bytes(), 6);
        assert_eq!(q.push_latest(Key::Focus, b"NEW".to_vec()), Push::Queued);
        let mut rest = Sink {
            got: Vec::new(),
            cap: usize::MAX,
        };
        assert!(q.flush(&mut rest).unwrap());
        assert_eq!([sink.got, rest.got].concat(), b"0123456789NEW");
    }

    #[test]
    fn a_blocked_writer_keeps_the_rest_queued() {
        let mut q = OutQueue::default();
        q.push_reliable(b"abcdef".to_vec());
        q.push_reliable(b"gh".to_vec());
        let mut sink = Sink {
            got: Vec::new(),
            cap: 7,
        };
        assert!(!q.flush(&mut sink).unwrap());
        assert_eq!(sink.got, b"abcdefg");
        assert_eq!(q.bytes(), 1);
        sink.cap = 100;
        assert!(q.flush(&mut sink).unwrap());
        assert_eq!(sink.got, b"abcdefgh");
    }

    #[test]
    fn overflow_drops_events_but_keeps_replies_then_asks_for_a_resync() {
        let mut q = OutQueue::with_limits(100, 1000);
        q.push_reliable(b"REPLY".to_vec());
        for i in 0..5 {
            assert_ne!(q.push_latest(Key::Window(i), vec![b'x'; 15]), Push::Close);
        }
        // 5 + 75 bytes queued: the next one crosses the soft limit.
        assert_eq!(q.push_latest(Key::Window(99), vec![b'y'; 30]), Push::Resync);
        assert_eq!(q.bytes(), 5);
        assert_eq!(drain(&mut q), b"REPLY");
        // The queue is usable again, and forgot the dropped keys.
        assert_eq!(q.push_latest(Key::Window(1), b"z".to_vec()), Push::Queued);
        assert_eq!(drain(&mut q), b"z");
    }

    #[test]
    fn a_started_frame_survives_a_drop() {
        let mut q = OutQueue::with_limits(40, 1000);
        q.push_latest(Key::Focus, vec![b'f'; 20]);
        let mut sink = Sink {
            got: Vec::new(),
            cap: 5,
        };
        assert!(!q.flush(&mut sink).unwrap());
        assert_eq!(q.push_latest(Key::Window(1), vec![b'w'; 40]), Push::Resync);
        // Only the started frame's remainder is left, so the stream stays parseable.
        assert_eq!(q.bytes(), 15);
        let mut rest = Sink {
            got: Vec::new(),
            cap: usize::MAX,
        };
        assert!(q.flush(&mut rest).unwrap());
        assert_eq!(rest.got, vec![b'f'; 15]);
    }

    #[test]
    fn a_snapshot_supersedes_pending_events() {
        let mut q = OutQueue::default();
        q.push_latest(Key::Window(1), b"old1".to_vec());
        q.push_reliable(b"R".to_vec());
        q.push_latest(Key::Focus, b"oldf".to_vec());
        q.push_snapshot(b"SNAP".to_vec());
        assert_eq!(drain(&mut q), b"RSNAP");
        // A second snapshot replaces an unsent first one.
        q.push_snapshot(b"one".to_vec());
        q.push_snapshot(b"two".to_vec());
        assert_eq!(drain(&mut q), b"two");
    }

    #[test]
    fn a_snapshot_larger_than_the_soft_limit_still_queues() {
        let mut q = OutQueue::with_limits(10, 1000);
        assert_eq!(q.push_snapshot(vec![1; 100]), Push::Queued);
        assert_eq!(q.bytes(), 100);
        assert_eq!(q.push_snapshot(vec![1; 2000]), Push::Close);
    }

    #[test]
    fn unread_replies_end_in_a_close() {
        let mut q = OutQueue::with_limits(100, 300);
        let mut last = Push::Queued;
        for _ in 0..10 {
            last = q.push_reliable(vec![0; 50]);
            if last == Push::Close {
                break;
            }
        }
        assert_eq!(last, Push::Close);
    }

    #[test]
    fn write_errors_surface() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut q = OutQueue::default();
        q.push_reliable(b"x".to_vec());
        assert!(q.flush(&mut Broken).is_err());
    }
}
