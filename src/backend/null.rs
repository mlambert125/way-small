//! Null backend (headless).
//!
//! Displays nothing and captures no input. Useful for testing protocol logic
//! without a display server or GPU.
//!
//! It does still consume frames and report them presented. Discarding them
//! silently would leave clients waiting forever on `wl_surface.frame`, and
//! would pin the buffers of the last frame so they were never released.

use crate::shared::{BackendMessage, Frame, PresentedAt};
use tokio::sync::mpsc::Sender;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[cfg(test)]
mod tests;

/// Run the null backend in a loop until stopped
pub async fn run_null_backend(
    backend_sender: Sender<BackendMessage>,
    mut frames: watch::Receiver<Frame>,
    cancel_token: CancellationToken,
) -> anyhow::Result<()> {
    info!("Null backend running (no display output)");
    loop {
        tokio::select! {
            () = cancel_token.cancelled() => break,
            changed = frames.changed() => {
                if changed.is_err() {
                    break;
                }
                // Take the frame and drop it at once: nothing draws it, but it
                // must not stay borrowed.
                drop(frames.borrow_and_update());
                if backend_sender
                    .send(BackendMessage::FramePresented(PresentedAt::now()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
    info!("Null backend shutting down");
    drop(backend_sender);
    Ok(())
}
