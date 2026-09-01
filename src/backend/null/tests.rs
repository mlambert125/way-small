//! Tests for the headless backend.

use super::{BackendMessage, BackendRequest, DmabufProbe, Frame, run_null_backend};
use tokio::sync::mpsc::channel;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn a_published_frame_is_reported_presented() {
    let (backend_tx, mut backend_rx) = channel(8);
    let (frames_tx, frames_rx) = watch::channel(Frame::new());
    let (_requests_tx, requests_rx) = channel(8);
    let cancel = CancellationToken::new();
    let backend = tokio::spawn(run_null_backend(
        backend_tx,
        frames_rx,
        requests_rx,
        cancel.clone(),
    ));

    drop(frames_tx.send_replace(Frame::new()));

    // Even with nothing to draw on, the frame has to be acknowledged:
    // frame callbacks and buffer releases both hang off this.
    let message = backend_rx.recv().await.expect("backend went quiet");
    assert!(matches!(message, BackendMessage::FramePresented(_)));

    cancel.cancel();
    backend.await.unwrap().unwrap();
}

#[tokio::test]
async fn there_is_no_dmabuf_import_without_a_gpu() {
    let (backend_tx, mut backend_rx) = channel(8);
    let (_frames_tx, frames_rx) = watch::channel(Frame::new());
    let (requests_tx, requests_rx) = channel(8);
    let cancel = CancellationToken::new();
    let backend = tokio::spawn(run_null_backend(
        backend_tx,
        frames_rx,
        requests_rx,
        cancel.clone(),
    ));

    requests_tx.send(BackendRequest::ProbeDmabuf).await.unwrap();

    // Answered rather than ignored: the compositor decides what to advertise
    // on the strength of this, and would wait forever for a backend that
    // stayed quiet because it had nothing to say.
    let message = backend_rx.recv().await.expect("backend went quiet");
    let BackendMessage::DmabufSupport { formats, probe } = message else {
        panic!("expected a dma-buf answer, got {message:?}");
    };
    assert!(formats.is_empty());
    assert!(matches!(probe, DmabufProbe::Unsupported(_)));

    cancel.cancel();
    backend.await.unwrap().unwrap();
}
