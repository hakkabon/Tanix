//! Tiny allocation-free string builder for server log lines.

/// Fixed-capacity string builder (no_std, no allocation).
pub struct StrBuf {
    pub buf: [u8; 128],
    pub len: usize,
}

impl Default for StrBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl StrBuf {
    pub fn new() -> Self {
        Self { buf: [0u8; 128], len: 0 }
    }

    pub fn push_str(&mut self, s: &str) {
        for &b in s.as_bytes() {
            if self.len < self.buf.len() {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
    }

    /// Append `0x` + 8 lowercase hex digits.
    pub fn push_hex32(&mut self, v: u32) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        self.push_str("0x");
        for shift in (0..32).step_by(4).rev() {
            if self.len < self.buf.len() {
                self.buf[self.len] = HEX[((v >> shift) & 0xf) as usize];
                self.len += 1;
            }
        }
    }

    /// Append decimal digits.
    pub fn push_dec32(&mut self, v: u32) {
        let mut tmp = [0u8; 10];
        let mut n = v;
        let mut i = tmp.len();
        if n == 0 {
            tmp[i - 1] = b'0';
            i -= 1;
        } else {
            while n > 0 {
                i -= 1;
                tmp[i] = b'0' + (n % 10) as u8;
                n /= 10;
            }
        }
        self.push_str(core::str::from_utf8(&tmp[i..]).unwrap_or("?"));
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("?")
    }

    /// Empty the buffer so it can be reused.
    pub fn reset(&mut self) {
        self.len = 0;
    }
}
