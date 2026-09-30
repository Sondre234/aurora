//! Triple-buffered `wl_shm` pool with per-buffer damage debt.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use smithay_client_toolkit::reexports::client::protocol::{wl_buffer::WlBuffer, wl_shm};
use smithay_client_toolkit::reexports::client::{Dispatch, QueueHandle};
use smithay_client_toolkit::shm::{Shm, raw::RawPool};

use crate::damage::Damage;
use crate::geom::Rect;

const BUFFERS: usize = 3;

/// User data of every pool buffer: set by the compositor's `release`.
pub(crate) struct BufferData(pub Arc<AtomicBool>);

struct Slot {
    buffer: WlBuffer,
    offset: usize,
    busy: Arc<AtomicBool>,
}

pub(crate) struct BufferPool {
    pool: RawPool,
    w: u32,
    h: u32,
    slots: Vec<Slot>,
    /// Damage each buffer still owes (device pixels); see `convert::finish_frame`.
    pub owed: Vec<Damage>,
}

impl BufferPool {
    pub fn new<S>(shm: &Shm, qh: &QueueHandle<S>, w: u32, h: u32) -> io::Result<Self>
    where
        S: Dispatch<WlBuffer, BufferData> + 'static,
    {
        let stride = w as usize * 4;
        let size = stride * h as usize;
        let mut pool = RawPool::new(size * BUFFERS, shm).map_err(io::Error::other)?;
        let mut slots = Vec::with_capacity(BUFFERS);
        for i in 0..BUFFERS {
            let busy = Arc::new(AtomicBool::new(false));
            let buffer = pool.create_buffer(
                (i * size) as i32,
                w as i32,
                h as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
                BufferData(busy.clone()),
                qh,
            );
            slots.push(Slot {
                buffer,
                offset: i * size,
                busy,
            });
        }
        let full = Rect::new(0.0, 0.0, w as f32, h as f32);
        let owed = (0..BUFFERS)
            .map(|_| {
                let mut d = Damage::new();
                d.add(full);
                d
            })
            .collect();
        Ok(Self {
            pool,
            w,
            h,
            slots,
            owed,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    /// Index of a buffer the compositor has released, if any.
    pub fn acquire(&self) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| !s.busy.load(Ordering::Acquire))
    }

    pub fn canvas(&mut self, i: usize) -> &mut [u8] {
        let len = self.w as usize * self.h as usize * 4;
        let off = self.slots[i].offset;
        &mut self.pool.mmap()[off..off + len]
    }

    pub fn buffer(&self, i: usize) -> &WlBuffer {
        &self.slots[i].buffer
    }

    /// Mark a buffer as handed to the compositor.
    pub fn mark_busy(&self, i: usize) {
        self.slots[i].busy.store(true, Ordering::Release);
    }
}

impl Drop for BufferPool {
    fn drop(&mut self) {
        for s in &self.slots {
            s.buffer.destroy();
        }
    }
}
