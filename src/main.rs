//! Entry point for way-small.
//!
//! Parses CLI args and config, selects a backend, wires up channels between
//! the Wayland socket listener, compositor loop, and display backend, then
//! waits for shutdown.

use crate::{
    backend::{BackendMessage, RenderFrame},
    wayland_socket::WaylandSocketMessage,
};
use clap::{Parser, ValueEnum};
use std::sync::Arc;
use tokio::sync::mpsc::channel;
use tracing::{debug, info};

mod backend;
mod compositor;
mod config;
mod null_backend;
mod protocol;
mod renderer;
mod wayland_socket;
mod winit_backend;

#[derive(Debug, Clone, ValueEnum)]
enum Backend {
    Winit,
    None,
}

#[derive(Parser, Debug)]
#[command(name = "way-small", about = "A small Wayland compositor")]
struct Args {
    #[arg(short, long, value_enum)]
    backend: Option<Backend>,

    #[arg(short, long)]
    socket_path: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let config = config::ConfigFile::load();
    let args = Args::parse();

    let backend = args.backend.unwrap_or_else(|| {
        config
            .backend
            .as_deref()
            .and_then(|s| match s {
                "winit" => Some(Backend::Winit),
                "none" => Some(Backend::None),
                _ => {
                    tracing::warn!("Unknown backend '{}' in config, using winit", s);
                    None
                }
            })
            .unwrap_or(Backend::Winit)
    });

    let socket_name = args
        .socket_path
        .or(config.socket_path)
        .unwrap_or_else(|| "way-small-0".to_string());

    let socket_path = if std::path::Path::new(&socket_name).is_absolute() {
        socket_name.clone()
    } else {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR is not set");
        format!("{}/{}", runtime_dir, socket_name)
    };

    info!("Using backend: {:?}", backend);
    info!(
        "Wayland socket: {} (WAYLAND_DISPLAY={})",
        socket_path, socket_name
    );

    debug!("Creating channels");

    let (wayland_message_tx, wayland_message_rx) = channel::<WaylandSocketMessage>(1000);
    let (backend_message_tx, backend_message_rx) = channel::<BackendMessage>(1000);
    let (frame_tx, frame_rx) = channel::<Arc<RenderFrame>>(2);
    let cancel_token = tokio_util::sync::CancellationToken::new();

    debug!("Spawning subsystem tasks");

    let backend_handle = match backend {
        Backend::Winit => {
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
            let handle = tokio::task::spawn_blocking({
                let cancel_token = cancel_token.clone();
                move || {
                    winit_backend::run_winit_backend(
                        backend_message_tx,
                        cancel_token,
                        ready_tx,
                        frame_rx,
                    )
                }
            });
            let _ = ready_rx.await;

            unsafe { std::env::set_var("WAYLAND_DISPLAY", &socket_name) };
            Some(handle)
        }
        Backend::None => {
            unsafe { std::env::set_var("WAYLAND_DISPLAY", &socket_name) };
            let cancel = cancel_token.clone();
            tokio::spawn(async move {
                // Keep the frame_rx alive
                let _frame_rx = frame_rx;
                null_backend::run_null_backend(backend_message_tx, cancel).await
            });
            None
        }
    };

    let compositor_handle = tokio::spawn(compositor::run_compositor(
        wayland_message_rx,
        backend_message_rx,
        frame_tx,
        cancel_token.clone(),
    ));

    let socket_handle = tokio::spawn(wayland_socket::run_wayland_socket(
        socket_path,
        wayland_message_tx,
        cancel_token.clone(),
    ));

    debug!("Subsystems running, waiting for shutdown signal");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
            cancel_token.cancel();
        }
        _ = cancel_token.cancelled() => {
            info!("Shutdown triggered by a subsystem");
        }
    }

    debug!("Waiting for subsystem tasks to finish");

    let (_, _) = tokio::join!(socket_handle, compositor_handle);
    if let Some(handle) = backend_handle {
        let _ = handle.await;
    }

    Ok(())
}
