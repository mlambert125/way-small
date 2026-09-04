//! Per-client connection state.
//!
//! Each connected client gets a `ClientState` tracking its object map (id ->
//! `ObjectType)` and a channel sender for pushing events back to the client.
//! The Clients struct manages the collection of all active client states.

use super::super::protocol::wire_utils::{ArgWriter, build_message};
use super::super::protocol::{ObjectType, wl_display};
use crate::wayland_socket::{CLIENT_SEND_QUEUE_LIMIT, WaylandProtocolMessage};
use std::collections::{HashMap, VecDeque};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{Sender, error::TrySendError};
use tokio_util::sync::CancellationToken;

/// The first object id the compositor may hand out.
///
/// Wayland splits the id space: a client allocates from below this line and the
/// compositor from above it, so neither has to ask the other what is free. The
/// halves are why `wl_display.delete_id` exists only for the client's own ids —
/// the compositor recycles its own without telling anyone.
pub const SERVER_ID_BASE: u32 = 0xff00_0000;

/// Whether an object id belongs to the compositor's half of the id space.
pub fn is_server_id(id: u32) -> bool {
    id >= SERVER_ID_BASE
}

/// State specific to an individual wayland client
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
    /// Next id to hand out from the compositor's half of the id space.
    next_server_id: u32,
}

impl ClientState {
    /// Create a new client state with a provided sender for talking back to
    /// the socket, an fd_queue containing ancillary socket fds, and a cancellation
    /// token for killing the underlying socket
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
            next_server_id: SERVER_ID_BASE,
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
            return Err(());
        }
        self.objects.insert(id, object_type);
        Ok(())
    }

    /// Take the next id from the compositor's half of the id space and
    /// register an object under it.
    ///
    /// Used where the protocol has the compositor name an object rather than
    /// the client — `zwp_linux_buffer_params_v1.created` carries a `wl_buffer`
    /// the compositor allocates. `None` once the half is exhausted, which takes
    /// sixteen million objects on one connection and is a client with a leak.
    pub fn allocate_id(&mut self, object_type: ObjectType) -> Option<u32> {
        let id = self.next_server_id;
        if id == u32::MAX {
            tracing::warn!("client has exhausted the server id space");
            return None;
        }
        self.next_server_id += 1;
        self.objects.insert(id, object_type);
        Some(id)
    }

    /// Take the next id from the compositor's half and record a version for it.
    ///
    /// A compositor-named object still has a version, and it is the version of
    /// whatever created it — a `wl_data_offer` speaks the version its
    /// `wl_data_device` was bound at. Without this the object would default to
    /// version 1 and every version-gated event on it would be silently
    /// suppressed, which is a failure that looks exactly like the feature not
    /// being implemented.
    pub fn allocate_id_with_version(
        &mut self,
        object_type: ObjectType,
        version: u32,
    ) -> Option<u32> {
        let id = self.allocate_id(object_type)?;
        self.object_versions.insert(id, version);
        Some(id)
    }

    /// Registers a new wayland object with wayland protocol version information
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

    /// Gets the version of an object, defaulting to 1 if the object was registered
    /// without a version #
    pub fn version(&self, id: u32) -> u32 {
        self.object_versions.get(&id).copied().unwrap_or(1)
    }

    /// Unregister a wayland client object
    pub fn unregister(&mut self, id: u32) {
        self.objects.remove(&id);
        self.object_versions.remove(&id);
        // Only the client's own ids are announced: it allocates those, so only
        // it needs telling one is free again. An id from the compositor's half
        // is the compositor's to recycle, and announcing it would invite the
        // client to reuse an id it never owned.
        if is_server_id(id) {
            return;
        }
        let args = ArgWriter::new().u32(id).build();
        let _ = self.send(build_message(wl_display::OBJECT_ID, wl_display::DELETE_ID, args));
    }

    /// Queue a message for delivery to this client's wayland socket
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

    /// Send a `wl_display.error` to this client and disconnect it.
    ///
    /// There is no other kind. `wl_display.error` is fatal by definition — the
    /// spec has the client disconnect on receiving one, and libwayland
    /// destroys the connection as it posts it — so the disconnect belongs here
    /// rather than at each call site, where it was previously remembered about
    /// four times in thirty-odd. An error the compositor sent and then carried
    /// on from is the worst of both: the client is entitled to consider the
    /// connection dead, while the compositor keeps state for it and keeps
    /// answering requests it may still have in flight.
    ///
    /// A condition the client can recover from is not this. Those are
    /// interface-specific events — `zwp_linux_buffer_params_v1.failed` is one
    /// — and go out as ordinary messages.
    ///
    /// Cancelling stops this client's socket tasks; the read task then reports
    /// the disconnect, and the client's resources are cleaned up through the
    /// same path any other disconnect takes. Callers only need to stop what
    /// they were doing.
    pub fn send_error(&self, object_id: u32, code: u32, msg: &str) {
        let args = ArgWriter::new()
            .u32(object_id)
            .u32(code)
            .string(msg)
            .build();
        let _ = self.send(build_message(wl_display::OBJECT_ID, wl_display::ERROR, args));
        self.cancel_token.cancel();
    }
}

/// A wrapper for a hashmap of all clients on the compositor keyed by client id
pub struct Clients {
    /// The hashmap
    states: HashMap<u32, ClientState>,
}

impl Clients {
    /// Create a new wrapped client state hashmap
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
        }
    }

    /// Create/add a new client to hashmap
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

    /// Get a mutable client state from this hashmap
    pub fn get(&mut self, client_id: u32) -> Option<&mut ClientState> {
        self.states.get_mut(&client_id)
    }

    /// Remove a client
    pub fn remove(&mut self, client_id: u32) {
        self.states.remove(&client_id);
    }

    /// Iterate over all clients
    pub fn iter(&self) -> impl Iterator<Item = (&u32, &ClientState)> {
        self.states.iter()
    }

    /// The version an object was bound at, without borrowing the collection
    /// mutably.
    ///
    /// [`Self::get`] hands back a `&mut ClientState`, which cannot be held
    /// while the rest of `CompositorState` is read. Version gating needs the
    /// answer in exactly that position — deciding whether to send an event,
    /// with the state that decides *what* to send already borrowed.
    pub fn version_of(&self, client_id: u32, object_id: u32) -> Option<u32> {
        self.states
            .get(&client_id)
            .map(|client| client.version(object_id))
    }
}
