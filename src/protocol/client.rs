//! Per-client connection state.
//!
//! Each connected client gets a ClientState tracking its object map (id ->
//! ObjectType) and a channel sender for pushing events back to the client.
//! The Clients struct manages the collection of all active client states.

use std::collections::HashMap;

use tokio::sync::mpsc::Sender;

use super::wire::{ArgWriter, message};
use super::{ObjectType, wl_display};
use crate::wayland_socket::WaylandProtocolMessage;

pub struct ClientState {
    /// Maps object id -> object type/state for every object this client has created.
    pub objects: HashMap<u32, ObjectType>,
    /// Maps object id -> bound interface version for version-gated events.
    pub object_versions: HashMap<u32, u32>,
    /// Sender for writing messages back to this client's socket.
    pub sender: Option<Sender<WaylandProtocolMessage>>,
}

impl ClientState {
    pub fn new() -> Self {
        let mut objects = HashMap::new();
        objects.insert(wl_display::OBJECT_ID, ObjectType::WlDisplay);
        Self {
            objects,
            object_versions: HashMap::new(),
            sender: None,
        }
    }

    pub fn register(&mut self, id: u32, object_type: ObjectType) {
        self.objects.insert(id, object_type);
    }

    pub fn register_with_version(&mut self, id: u32, object_type: ObjectType, version: u32) {
        self.objects.insert(id, object_type);
        self.object_versions.insert(id, version);
    }

    pub fn version(&self, id: u32) -> u32 {
        self.object_versions.get(&id).copied().unwrap_or(1)
    }

    pub async fn unregister(&mut self, id: u32) {
        self.objects.remove(&id);
        self.object_versions.remove(&id);
        // Notify the client so it can recycle this object id.
        let args = ArgWriter::new().u32(id).build();
        let _ = self
            .send(message(wl_display::OBJECT_ID, wl_display::DELETE_ID, args))
            .await;
    }

    /// Send a message to this client. Returns Ok(()) or logs a warning on failure.
    pub async fn send(&self, msg: WaylandProtocolMessage) -> Result<(), ()> {
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

    pub fn iter(&self) -> impl Iterator<Item = (&u32, &ClientState)> {
        self.states.iter()
    }
}
