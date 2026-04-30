use tokio::sync::mpsc::channel;
use tracing::info;

mod compositor;
mod wayland_socket;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let socket_path = String::from("/tmp/way-small.sock");
    let (wayland_message_tx, wayland_message_rx) =
        channel::<wayland_socket::WaylandSocketMessage>(10000);
    let cancel_token = tokio_util::sync::CancellationToken::new();

    let socket_handle = tokio::spawn(wayland_socket::run_wayland_socket(
        socket_path,
        wayland_message_tx,
        cancel_token.clone(),
    ));
    let compositor_handle = tokio::spawn(compositor::run_compositor(
        wayland_message_rx,
        cancel_token.clone(),
    ));

    tokio::signal::ctrl_c().await?;
    info!("Received Ctrl+C, shutting down...");
    cancel_token.cancel();

    let _ = tokio::join!(socket_handle, compositor_handle);
    Ok(())
}
