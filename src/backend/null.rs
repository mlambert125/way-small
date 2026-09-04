//! Null backend (headless).
//!
//! Displays nothing and captures no input. Useful for testing protocol logic
//! without a display server or GPU.
//!
//! It reports no outputs, and so never asks for a frame and never presents
//! one: rendering is paced per output by the backend that owns it, and this
//! backend owns none. Clients still get their `wl_surface.frame` callbacks —
//! the compositor paces the surfaces no output is showing itself, which under
//! this backend is all of them.
//!
//! The frame slot is still drained. Nothing should arrive in it with no
//! outputs to compose for, but holding a frame borrowed would pin the client
//! buffers it references and keep them from being released.

use crate::shared::{BackendMessage, BackendRequest, DmabufProbe, Frame};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Run the null backend in a loop until stopped
pub async fn run_null_backend(
    backend_sender: Sender<BackendMessage>,
    mut frames: watch::Receiver<Frame>,
    mut requests: Receiver<BackendRequest>,
    cancel_token: CancellationToken,
) -> anyhow::Result<()> {
    info!("Null backend running (no display output)");
    loop {
        tokio::select! {
            () = cancel_token.cancelled() => break,
            request = requests.recv() => {
                match request {
                    // No GPU, so nothing to import onto and no formats to
                    // offer. Answered rather than ignored: the compositor is
                    // waiting to hear before it decides what to advertise.
                    Some(BackendRequest::ProbeDmabuf) => {
                        if backend_sender
                            .send(BackendMessage::DmabufSupport {
                                formats: Vec::new(),
                                probe: DmabufProbe::Unsupported(
                                    "the null backend has no GPU to import onto".into(),
                                ),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    // No GPU to import onto. Answered rather than dropped: a
                    // client is blocked on this one.
                    Some(BackendRequest::ImportDmabuf { token, .. }) => {
                        if backend_sender
                            .send(BackendMessage::DmabufImportResult {
                                token,
                                imported: false,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    None => break,
                }
            }
            changed = frames.changed() => {
                if changed.is_err() {
                    break;
                }
                // Take the frame and drop it at once: nothing draws it, but it
                // must not stay borrowed. No presentation is reported, because
                // none happened — there is no output here that could have made
                // one, and a presentation names the output it was made on.
                drop(frames.borrow_and_update());
            }
        }
    }
    info!("Null backend shutting down");
    drop(backend_sender);
    Ok(())
}
