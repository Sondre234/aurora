//! The password buffer: fixed capacity (never reallocates, so no stray copies are left
//! behind), zeroized on clear and on drop, and never printed.

use std::fmt;

use zeroize::Zeroize;

/// Longest password accepted, in bytes. PAM itself caps at 512 (`PAM_MAX_RESP_SIZE`).
pub const MAX_BYTES: usize = 512;

pub struct Secret {
    text: String,
}

impl Secret {
    pub fn new() -> Self {
        Self {
            text: String::with_capacity(MAX_BYTES),
        }
    }

    /// Appends `c` unless the buffer is full. Control characters are ignored.
    pub fn push(&mut self, c: char) -> bool {
        if c.is_control() || self.text.len() + c.len_utf8() > MAX_BYTES {
            return false;
        }
        self.text.push(c);
        true
    }

    /// Removes the last character and wipes its bytes.
    pub fn pop(&mut self) -> bool {
        let Some(c) = self.text.pop() else {
            return false;
        };
        // `pop` only shrinks the length: the removed bytes are still in the allocation.
        let len = self.text.len();
        let n = c.len_utf8();
        // SAFETY: the bytes past `len` are spare capacity; writing zeros to them cannot
        // break the UTF-8 invariant of the live part.
        unsafe {
            std::ptr::write_bytes(self.text.as_mut_vec().as_mut_ptr().add(len), 0, n);
        }
        true
    }

    pub fn clear(&mut self) {
        self.text.zeroize();
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Number of characters (what the prompt shows as dots).
    pub fn chars(&self) -> usize {
        self.text.chars().count()
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }

    /// Moves the content out into a new buffer, leaving this one empty and wiped.
    pub fn take(&mut self) -> Secret {
        let mut out = Secret::new();
        out.text.push_str(&self.text);
        self.clear();
        out
    }

    /// The whole allocation, for tests that check the wiping.
    #[cfg(test)]
    fn raw(&self) -> &[u8] {
        // SAFETY: reads initialized-or-spare capacity of a live allocation in a test; the
        // spare part was written by `with_capacity`'s callers only through zeroize.
        unsafe { std::slice::from_raw_parts(self.text.as_ptr(), self.text.capacity()) }
    }
}

impl Default for Secret {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.text.zeroize();
    }
}

/// Only for tests that assert on what was handed over.
#[cfg(test)]
impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(<{} chars>)", self.chars())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Secret {
        let mut secret = Secret::new();
        for c in s.chars() {
            secret.push(c);
        }
        secret
    }

    #[test]
    fn push_pop_and_count() {
        let mut s = typed("héllo");
        assert_eq!(s.chars(), 5);
        assert!(s.pop());
        assert_eq!(s.as_bytes(), "héll".as_bytes());
        while s.pop() {}
        assert!(s.is_empty());
        assert!(!s.pop());
    }

    #[test]
    fn caps_length_without_reallocating() {
        let mut s = Secret::new();
        let before = s.text.as_ptr();
        for _ in 0..MAX_BYTES + 50 {
            s.push('a');
        }
        assert_eq!(s.as_bytes().len(), MAX_BYTES);
        assert_eq!(s.text.as_ptr(), before);
        // A multibyte char that does not fit is refused whole.
        s.pop();
        s.pop();
        s.text.push('a'); // one byte free after two pops and a push
        assert!(!s.push('€'));
    }

    #[test]
    fn ignores_control_chars() {
        let mut s = Secret::new();
        assert!(!s.push('\n'));
        assert!(!s.push('\u{1b}'));
        assert!(s.is_empty());
    }

    #[test]
    fn clear_and_pop_wipe_the_allocation() {
        let mut s = typed("hunter2");
        s.pop();
        assert!(!s.raw().windows(7).any(|w| w == b"hunter2"));
        assert_eq!(s.raw()[6], 0, "popped byte wiped");
        s.clear();
        assert!(s.raw().iter().all(|b| *b == 0));
        assert!(s.is_empty());
    }

    #[test]
    fn take_moves_and_wipes_the_source() {
        let mut s = typed("secret");
        let t = s.take();
        assert_eq!(t.as_bytes(), b"secret");
        assert!(s.is_empty());
        assert!(s.raw().iter().all(|b| *b == 0));
    }

    #[test]
    fn debug_never_shows_content() {
        let s = typed("swordfish");
        let shown = format!("{s:?}");
        assert!(!shown.contains("sword"));
    }
}
