use crate::{backend::BackendMessage, protocol, wayland_socket::WaylandSocketMessage};
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

pub async fn run_compositor(
    mut wayland_message_receiver: Receiver<WaylandSocketMessage>,
    mut backend_message_receiver: Receiver<BackendMessage>,
    cancel_token: CancellationToken,
) -> anyhow::Result<()> {
    info!("Running compositor...");

    let mut clients = protocol::Clients::new();

    loop {
        tokio::select! {
            Some(message) = wayland_message_receiver.recv() => {
                match message {
                    WaylandSocketMessage::Message(msg) => {
                        debug!(
                            "client {}: object_id={} op_code={}",
                            msg.client_id, msg.message.object_id, msg.message.op_code
                        );
                        protocol::handle_message(&mut clients, &msg).await;
                    }
                    WaylandSocketMessage::ClientDisconnected { client_id } => {
                        info!("Client {} disconnected", client_id);
                        clients.remove(client_id);
                    }
                }
            }
            Some(message) = backend_message_receiver.recv() => {
                debug!("Received event from backend: {:?}", message);
                match message {
                    BackendMessage::Closed => {
                        info!("Backend requested shutdown");
                        cancel_token.cancel();
                        break;
                    }
                    BackendMessage::Resized(w, h) => {
                        info!("Backend resized to {}x{}", w, h);
                    }
                    BackendMessage::KeyInput { keycode, keysym, state } => {
                        debug!("Key input: keycode={} keysym={:#x} state={:?}", keycode, keysym, state);
                    }
                    BackendMessage::MouseMove { x, y } => {
                        debug!("Mouse move: ({}, {})", x, y);
                    }
                    BackendMessage::MouseButton { button, state } => {
                        debug!("Mouse button: {:?} {:?}", button, state);
                    }
                    BackendMessage::MouseScroll { dx, dy } => {
                        debug!("Mouse scroll: ({}, {})", dx, dy);
                    }
                    BackendMessage::FocusIn => {
                        debug!("Focus in");
                    }
                    BackendMessage::FocusOut => {
                        debug!("Focus out");
                    }
                }
            }
            _ = cancel_token.cancelled() => {
                info!("Compositor received shutdown signal");
                break;
            }
        }
    }

    Ok(())
}
