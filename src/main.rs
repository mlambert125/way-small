//! Entry point for way-small.
//!
//! Parses CLI args and config, selects a backend, wires up channels between
//! the Wayland socket listener, compositor loop, and display backend, then
//! waits for shutdown.

use clap::{Parser, ValueEnum};
use tokio::sync::mpsc::channel;
use tracing::info;

use std::sync::Arc;

use crate::backend::{BackendMessage, RenderFrame};

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

    // Channels
    let (wayland_message_tx, wayland_message_rx) =
        channel::<wayland_socket::WaylandSocketMessage>(10000);
    let (backend_message_tx, backend_message_rx) = channel::<BackendMessage>(10000);
    let (frame_tx, frame_rx) = channel::<Arc<RenderFrame>>(2); // double-buffer: only 2 in flight
    let cancel_token = tokio_util::sync::CancellationToken::new();

    let backend_handle = match backend {
        Backend::Winit => {
            // Winit needs the real WAYLAND_DISPLAY to connect to the host compositor.
            // We set our override only after winit signals it has built its event loop.
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
            // SAFETY: winit has connected to the host compositor; override for child processes
            unsafe { std::env::set_var("WAYLAND_DISPLAY", &socket_name) };
            Some(handle)
        }
        Backend::None => {
            // SAFETY: no windowing backend needs the real WAYLAND_DISPLAY
            unsafe { std::env::set_var("WAYLAND_DISPLAY", &socket_name) };
            // Keep frame_rx alive so the compositor's frame_sender doesn't error.
            // Frames are just dropped (no display).
            let cancel = cancel_token.clone();
            tokio::spawn(async move {
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

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
            cancel_token.cancel();
        }
        _ = cancel_token.cancelled() => {
            info!("Shutdown triggered by a subsystem");
        }
    }

    let (_, _) = tokio::join!(socket_handle, compositor_handle);
    if let Some(handle) = backend_handle {
        let _ = handle.await;
    }
    Ok(())
}
