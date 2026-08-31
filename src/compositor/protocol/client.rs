//! Per-client connection state.
//!
//! Each connected client gets a `ClientState` tracking its object map (id ->
//! `ObjectType)` and a channel sender for pushing events back to the client.
//! The Clients struct manages the collection of all active client states.

use std::collections::{HashMap, VecDeque};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::{Sender, error::TrySendError};
use tokio_util::sync::CancellationToken;

use super::wire_utils::{ArgWriter, message};
use super::{ObjectType, wl_display};
use crate::wayland_socket::{CLIENT_SEND_QUEUE_LIMIT, WaylandProtocolMessage};

pub struct ClientState {
    /// Maps object id -> object type/state for every object this client has created.
    pub objects: HashMap<u32, ObjectType>,
    /// Maps object id -> bound interface version for version-gated events.
    pub object_versions: HashMap<u32, u32>,
    /// Sender for writing messages back to this client's socket.
    pub sender: Sender<WaylandProtocolMessage>,
    // File descriptor queue shared between this client and the socket task
    pub fd_queue: Arc<Mutex<VecDeque<OwnedFd>>>,
    /// Cancels this client's socket tasks. Triggered when the client stops
    /// draining its socket and its outgoing queue overflows.
    pub cancel_token: CancellationToken,
}

impl ClientState {
    pub fn new(
        sender: Sender<WaylandProtocolMessage>,
        fd_queue: Arc<Mutex<VecDeque<OwnedFd>>>,
        cancel_token: CancellationToken,
    ) -> Self {
        let mut objects = HashMap::new();
        objects.insert(wl_display::OBJECT_ID, ObjectType::WlDisplay);
        Self {
            objects,
            object_versions: HashMap::new(),
            sender,
            fd_queue,
            cancel_token,
        }
    }

    /// Register a newly created object.
    ///
    /// Returns `Err` if the client is reusing an id that is still live. Clients
    /// must not reuse an object id until the compositor has acknowledged its
    /// destruction with `wl_display.delete_id`, so this is a protocol error.
    /// Silently replacing the old object — which is what a bare `insert` did —
    /// strands whatever compositor state it owned: an `mmap`ed shm pool, a
    /// surface in the stack, a pointer or keyboard binding. Those are keyed by
    /// object id, so the replacement makes them unreachable and the
    /// disconnect-time cleanup, which walks the client's object map, no longer
    /// knows they exist.
    ///
    /// The client is sent an error and dropped, so callers only need to stop
    /// what they were doing. Returning `Result` rather than handling it
    /// silently is deliberate: `Result` is `#[must_use]`, so a call site that
    /// forgets to bail out is a compiler warning rather than a latent leak.
    pub fn register(&mut self, id: u32, object_type: ObjectType) -> Result<(), ()> {
        if self.objects.contains_key(&id) {
            tracing::warn!("Client reused live object id {}, disconnecting it", id);
            // WL_DISPLAY_ERROR_INVALID_OBJECT = 0
            self.send_error(id, 0, &format!("object id {id} is already in use"));
            self.cancel_token.cancel();
            return Err(());
        }
        self.objects.insert(id, object_type);
        Ok(())
    }

    pub fn register_with_version(
        &mut self,
        id: u32,
        object_type: ObjectType,
        version: u32,
    ) -> Result<(), ()> {
        self.register(id, object_type)?;
        self.object_versions.insert(id, version);
        Ok(())
    }

    pub fn version(&self, id: u32) -> u32 {
        self.object_versions.get(&id).copied().unwrap_or(1)
    }

    pub fn unregister(&mut self, id: u32) {
        self.objects.remove(&id);
        self.object_versions.remove(&id);
        // Notify the client so it can recycle this object id.
        let args = ArgWriter::new().u32(id).build();
        let _ = self.send(message(wl_display::OBJECT_ID, wl_display::DELETE_ID, args));
    }

    /// Queue a message for delivery to this client.
    ///
    /// This never blocks. The compositor loop is single-threaded and owns all
    /// state, so awaiting a client here would let one client that has stopped
    /// reading its socket stall input, rendering, and every other client. If
    /// the outgoing queue is full we assume the client is wedged and drop it,
    /// which is the same policy libwayland applies once a client's output
    /// buffer grows past its threshold.
    ///
    /// On failure the message is dropped, which closes any file descriptors it
    /// carried — callers do not need to clean them up.
    pub fn send(&self, msg: WaylandProtocolMessage) -> Result<(), ()> {
        match self.sender.try_send(msg) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                // Only complain once; later sends land here until the socket
                // tasks notice the cancellation and tear the client down.
                if !self.cancel_token.is_cancelled() {
                    tracing::warn!(
                        "Client is not draining its socket ({CLIENT_SEND_QUEUE_LIMIT} messages queued), disconnecting it"
                    );
                    self.cancel_token.cancel();
                }
                Err(())
            }
            Err(TrySendError::Closed(_)) => {
                tracing::debug!("Client socket closed, dropping outgoing message");
                Err(())
            }
        }
    }

    /// Send a `wl_display.error` to this client.
    pub fn send_error(&self, object_id: u32, code: u32, msg: &str) {
        let args = ArgWriter::new()
            .u32(object_id)
            .u32(code)
            .string(msg)
            .build();
        let _ = self.send(message(wl_display::OBJECT_ID, wl_display::ERROR, args));
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

    pub fn create(
        &mut self,
        client_id: u32,
        sender: Sender<WaylandProtocolMessage>,
        fd_queue: Arc<Mutex<VecDeque<OwnedFd>>>,
        cancel_token: CancellationToken,
    ) {
        let client_state = ClientState::new(sender, fd_queue, cancel_token);

        self.states.insert(client_id, client_state);
    }

    pub fn get(&mut self, client_id: u32) -> Option<&mut ClientState> {
        self.states.get_mut(&client_id)
    }

    pub fn remove(&mut self, client_id: u32) {
        self.states.remove(&client_id);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&u32, &ClientState)> {
        self.states.iter()
    }
}
