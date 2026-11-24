//! Fixed-capacity text buffer (no heap), used for status strings, URLs and
//! formatting. Writes past the capacity are truncated.

#[derive(Clone, Copy)]
pub struct Text<const N: usize> {
    data: [u8; N],
    len: usize,
}

impl<const N: usize> Text<N> {
    pub const fn new() -> Self {
        Self { data: [0; N], len: 0 }
    }

    pub fn from_str(s: &str) -> Self {
        let mut t = Self::new();
        t.push_str(s);
        t
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
    }

    pub fn push_str(&mut self, s: &str) {
        // Only cut at a char boundary so as_str() stays valid UTF-8.
        let mut n = s.len().min(N - self.len);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        self.data[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
    }

    /// Append bytes as printable ASCII for the display's fonts (and safe
    /// for JSON after escaping): control characters become spaces, each
    /// non-ASCII UTF-8 character (or Latin-1 byte) becomes one '?'.
    pub fn push_sanitized(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.len == N {
                break;
            }
            let c = match b {
                0x20..=0x7E => b,
                0x80..=0xBF => continue, // UTF-8 continuation byte
                0xC0..=0xFF => b'?',
                _ => b' ',
            };
            self.data[self.len] = c;
            self.len += 1;
        }
    }
}

// Compares the text only (bytes past `len` may be left over from before).
impl<const N: usize> PartialEq for Text<N> {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}
impl<const N: usize> Eq for Text<N> {}

impl<const N: usize> core::fmt::Write for Text<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.push_str(s);
        Ok(())
    }
}
