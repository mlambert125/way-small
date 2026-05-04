use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::backend::BackendMessage;

pub async fn run_null_backend(
    backend_sender: Sender<BackendMessage>,
    cancel_token: CancellationToken,
) -> anyhow::Result<()> {
    info!("Null backend running (no display output)");
    cancel_token.cancelled().await;
    info!("Null backend shutting down");
    drop(backend_sender);
    Ok(())
}
