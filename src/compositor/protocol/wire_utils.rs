//! Wayland wire format serialization and deserialization.
//!
//! Provides low-level helpers for reading and writing the Wayland binary
//! protocol: `ArgWriter` (builder for outgoing message args), `ArgReader`
//! (cursor-based parser for incoming args), and the `message()` constructor.

use crate::wayland_socket::WaylandProtocolMessage;

/// Read a u32 from a byte slice at the given offset.
#[allow(dead_code)]
pub fn read_u32(args: &[u8], offset: usize) -> Option<u32> {
    args.get(offset..offset + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Read an i32 from a byte slice at the given offset.
#[allow(dead_code)]
pub fn read_i32(args: &[u8], offset: usize) -> Option<i32> {
    args.get(offset..offset + 4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Read a Wayland string from a byte slice at the given offset.
/// Returns (`String`, `bytes_consumed` including padding).
/// Wire format: u32 length (including null), then chars + null, padded to 4 bytes.
pub fn read_string(args: &[u8], offset: usize) -> Option<(String, usize)> {
    let len = read_u32(args, offset)? as usize;
    if len == 0 {
        return Some((String::new(), 4));
    }
    let padded = (len + 3) & !3;
    let start = offset + 4;
    let end = start + len - 1; // exclude null terminator
    if args.len() < start + padded {
        return None;
    }
    let s = String::from_utf8_lossy(&args[start..end]).into_owned();
    Some((s, 4 + padded))
}

/// Writer for building Wayland message argument buffers.
pub struct ArgWriter {
    buf: Vec<u8>,
}

#[allow(dead_code)]
impl ArgWriter {
    /// Create a new `ArgWriter` with an emptargument buffer
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Adds a `u32` to the argument buffer
    pub fn u32(mut self, val: u32) -> Self {
        self.buf.extend_from_slice(&val.to_le_bytes());
        self
    }

    /// Adds an `i32` to the argument buffer
    pub fn i32(mut self, val: i32) -> Self {
        self.buf.extend_from_slice(&val.to_le_bytes());
        self
    }

    /// Adds a wayland string to the argument buffer
    pub fn string(mut self, val: &str) -> Self {
        assert!(
            val.len() < u32::MAX as usize,
            "String too long for Wayland protocol"
        );
        let len = u32::try_from(val.len()).expect("String too long for Wayland protocol") + 1;
        self.buf.extend_from_slice(&len.to_le_bytes());
        self.buf.extend_from_slice(val.as_bytes());
        self.buf.push(0); // null terminator
        // pad to 4-byte boundary
        let padded = ((len as usize) + 3) & !3;
        let padding = padded - len as usize;
        self.buf.extend(std::iter::repeat_n(0u8, padding));
        self
    }

    /// Adds a 64-bit float as a 24.8 fixed point decimal to the buffer
    pub fn fixed(self, val: f64) -> Self {
        self.i32(f64_to_24_8_fixed(val))
    }

    pub fn build(self) -> Vec<u8> {
        self.buf
    }
}

/// Convert a f64 to Wayland's 24.8 fixed-point format (i32 with 8 fractional bits).
pub fn f64_to_24_8_fixed(val: f64) -> i32 {
    f64_to_i32(val * 256.0)
}

// Convert a f64 to i32 without any scaling
pub fn f64_to_i32(val: f64) -> i32 {
    unsafe { f64::to_int_unchecked(val) }
}

/// Cursor-based reader for parsing Wayland message arguments.
pub struct ArgReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

#[allow(dead_code)]
impl<'a> ArgReader<'a> {
    /// Create a new argument reader
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Attempt to read a u32 from the buffer and advance the cursor
    pub fn u32(&mut self) -> Option<u32> {
        let val = read_u32(self.buf, self.pos)?;
        self.pos += 4;
        Some(val)
    }

    /// Attempt to read an i32 from the buffer and advance the cursor
    pub fn i32(&mut self) -> Option<i32> {
        let val = read_i32(self.buf, self.pos)?;
        self.pos += 4;
        Some(val)
    }

    /// Attempt to read a wayland string from the buffer and advance the cursor
    pub fn string(&mut self) -> Option<String> {
        let (s, consumed) = read_string(self.buf, self.pos)?;
        self.pos += consumed;
        Some(s)
    }

    /// Attempt to read a fixed-point decimal from the buffer, convert it to a `f64` and advance the cursor
    pub fn fixed(&mut self) -> Option<f64> {
        let raw = self.i32()?;
        Some(f64::from(raw) / 256.0)
    }

    /// Alias for u32 — reads a `new_id` argument.
    pub fn new_id(&mut self) -> Option<u32> {
        self.u32()
    }
}

/// Build a `WaylandMessage` with no file descriptors.
pub fn message(object_id: u32, op_code: u16, args: Vec<u8>) -> WaylandProtocolMessage {
    WaylandProtocolMessage {
        object_id,
        op_code,
        args,
        fds: Vec::new(),
    }
}
