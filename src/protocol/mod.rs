pub mod wl_display;
pub mod wl_registry;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::sync::mpsc::Sender;

use crate::wayland_socket::{ClientMessage, WaylandMessage};

static NEXT_SERIAL: AtomicU32 = AtomicU32::new(1);

/// Get the next monotonically increasing serial number.
pub fn next_serial() -> u32 {
    NEXT_SERIAL.fetch_add(1, Ordering::Relaxed)
}

/// The type and state of a Wayland protocol object.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    WlDisplay,
    WlRegistry,
    WlCallback,
}

/// A global interface that clients can bind via wl_registry.
pub struct Global {
    pub interface: &'static str,
    pub version: u32,
}

pub static GLOBALS: &[Global] = &[
    Global {
        interface: "wl_compositor",
        version: 5,
    },
    Global {
        interface: "wl_shm",
        version: 1,
    },
    Global {
        interface: "wl_seat",
        version: 8,
    },
    Global {
        interface: "wl_output",
        version: 4,
    },
    Global {
        interface: "xdg_wm_base",
        version: 5,
    },
];

// --- Wire format helpers ---

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
/// Returns (string, bytes_consumed including padding).
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
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn u32(mut self, val: u32) -> Self {
        self.buf.extend_from_slice(&val.to_le_bytes());
        self
    }

    pub fn i32(mut self, val: i32) -> Self {
        self.buf.extend_from_slice(&val.to_le_bytes());
        self
    }

    /// Write a Wayland string: u32 length (including null) + chars + null + padding.
    pub fn string(mut self, val: &str) -> Self {
        let len = val.len() as u32 + 1; // include null terminator
        self.buf.extend_from_slice(&len.to_le_bytes());
        self.buf.extend_from_slice(val.as_bytes());
        self.buf.push(0); // null terminator
        // pad to 4-byte boundary
        let padded = ((len as usize) + 3) & !3;
        let padding = padded - len as usize;
        self.buf.extend(std::iter::repeat_n(0u8, padding));
        self
    }

    /// Write a Wayland fixed-point value (24.8 format).
    pub fn fixed(self, val: f64) -> Self {
        let fixed = (val * 256.0) as i32;
        self.i32(fixed)
    }

    pub fn build(self) -> Vec<u8> {
        self.buf
    }
}

/// Cursor-based reader for parsing Wayland message arguments.
pub struct ArgReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

#[allow(dead_code)]
impl<'a> ArgReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn u32(&mut self) -> Option<u32> {
        let val = read_u32(self.buf, self.pos)?;
        self.pos += 4;
        Some(val)
    }

    pub fn i32(&mut self) -> Option<i32> {
        let val = read_i32(self.buf, self.pos)?;
        self.pos += 4;
        Some(val)
    }

    pub fn string(&mut self) -> Option<String> {
        let (s, consumed) = read_string(self.buf, self.pos)?;
        self.pos += consumed;
        Some(s)
    }

    pub fn fixed(&mut self) -> Option<f64> {
        let raw = self.i32()?;
        Some(raw as f64 / 256.0)
    }

    /// Alias for u32 — reads a new_id argument.
    pub fn new_id(&mut self) -> Option<u32> {
        self.u32()
    }
}

/// Build a WaylandMessage with no file descriptors.
pub fn message(object_id: u32, op_code: u16, args: Vec<u8>) -> WaylandMessage {
    WaylandMessage {
        object_id,
        op_code,
        args,
        fds: VecDeque::new(),
    }
}

// --- Client state ---

pub struct ClientState {
    /// Maps object id -> object type/state for every object this client has created.
    pub objects: HashMap<u32, ObjectType>,
    /// Sender for writing messages back to this client's socket.
    pub sender: Option<Sender<WaylandMessage>>,
}

impl ClientState {
    pub fn new() -> Self {
        let mut objects = HashMap::new();
        objects.insert(wl_display::OBJECT_ID, ObjectType::WlDisplay);
        Self {
            objects,
            sender: None,
        }
    }

    pub fn register(&mut self, id: u32, object_type: ObjectType) {
        self.objects.insert(id, object_type);
    }

    pub fn unregister(&mut self, id: u32) {
        self.objects.remove(&id);
    }

    /// Send a message to this client. Returns Ok(()) or logs a warning on failure.
    pub async fn send(&self, msg: WaylandMessage) -> Result<(), ()> {
        if let Some(sender) = &self.sender {
            if let Err(e) = sender.send(msg).await {
                tracing::warn!("Failed to send message to client: {}", e);
                return Err(());
            }
            Ok(())
        } else {
            tracing::warn!("No sender for client");
            Err(())
        }
    }

    /// Send a wl_display.error to this client.
    pub async fn send_error(&self, object_id: u32, code: u32, msg: &str) {
        let args = ArgWriter::new()
            .u32(object_id)
            .u32(code)
            .string(msg)
            .build();
        let _ = self
            .send(message(wl_display::OBJECT_ID, wl_display::ERROR, args))
            .await;
    }
}

pub struct Clients {
    states: HashMap<u32, ClientState>,
}

impl Clients {
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
        }
    }

    pub fn get_or_create(&mut self, client_id: u32) -> &mut ClientState {
        self.states
            .entry(client_id)
            .or_insert_with(ClientState::new)
    }

    pub fn remove(&mut self, client_id: u32) {
        self.states.remove(&client_id);
    }
}

pub async fn handle_message(clients: &mut Clients, message: &ClientMessage) {
    let object_id = message.message.object_id;

    // Ensure the client state has a sender (set on first message)
    let state = clients.get_or_create(message.client_id);
    if state.sender.is_none() {
        state.sender = Some(message.socket_sender.clone());
    }

    let obj_type = state.objects.get(&object_id).copied();
    let state = clients.get_or_create(message.client_id);

    match obj_type {
        Some(ObjectType::WlDisplay) => {
            wl_display::handle(state, message).await;
        }
        Some(ObjectType::WlRegistry) => {
            wl_registry::handle(state, message).await;
        }
        Some(ObjectType::WlCallback) => {
            tracing::warn!(
                "client {}: unexpected request on wl_callback id={}",
                message.client_id,
                object_id,
            );
        }
        None => {
            tracing::warn!(
                "client {}: unknown object_id={}, op_code={}",
                message.client_id,
                object_id,
                message.message.op_code,
            );
        }
    }
}
