use crate::wayland_socket::WaylandSocketMessage;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

pub async fn run_compositor(
    mut wayland_message_receiver: Receiver<WaylandSocketMessage>,
    cancel_token: CancellationToken,
) -> anyhow::Result<()> {
    println!("Running compositor...");

    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(message) = wayland_message_receiver.recv() => {
                    debug!(
                        "Received message for object {} with opcode {} and args {:?}",
                        message.message.object_id, message.message.op_code, message.message.args
                    );
                }
                _ = cancel_token.cancelled() => {
                    info!("Compositor received shutdown signal");
                    break;
                }
            }
        }
    });

    Ok(())
}
