//! Tests for the headless backend.

use super::{BackendMessage, Frame, run_null_backend};
use tokio::sync::mpsc::channel;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn a_published_frame_is_reported_presented() {
    let (backend_tx, mut backend_rx) = channel(8);
    let (frames_tx, frames_rx) = watch::channel(Frame::new());
    let cancel = CancellationToken::new();
    let backend = tokio::spawn(run_null_backend(backend_tx, frames_rx, cancel.clone()));

    drop(frames_tx.send_replace(Frame::new()));

    // Even with nothing to draw on, the frame has to be acknowledged:
    // frame callbacks and buffer releases both hang off this.
    let message = backend_rx.recv().await.expect("backend went quiet");
    assert!(matches!(message, BackendMessage::FramePresented(_)));

    cancel.cancel();
    backend.await.unwrap().unwrap();
}
