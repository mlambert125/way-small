//! Tests for the headless backend.

use super::{BackendMessage, BackendRequest, DmabufProbe, Frame, run_null_backend};
use tokio::sync::mpsc::channel;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn a_backend_with_no_outputs_presents_nothing() {
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

    // A presentation names the output it happened on, and this backend has
    // none — it never asks for a frame, so nothing is ever composed for it.
    // The clients' frame callbacks are the compositor's job here, fired
    // against the surfaces no output is showing. Claiming a presentation
    // would fire them against an output that does not exist.
    let quiet = tokio::time::timeout(std::time::Duration::from_millis(50), backend_rx.recv()).await;
    assert!(quiet.is_err(), "the backend should have nothing to report");

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
