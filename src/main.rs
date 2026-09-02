#![warn(clippy::pedantic)]

//! Entry point for way-small.
use crate::{
    shared::{BackendMessage, BackendRequest, Frame},
    wayland_socket::WaylandSocketMessage,
};
use clap::{Parser, ValueEnum};
use tokio::sync::{mpsc::channel, watch};
use tracing::{debug, info};

mod backend;
mod compositor;
mod config;
mod shared;
mod wayland_socket;

/// Backend choices for compositor I/O
#[derive(Debug, Clone, ValueEnum)]
enum Backend {
    // Winit backend for testing / running as a child compositor
    Winit,
    // Null backend (logging only)
    None,
}

/// Command-line arguments
/// Most of these are config file overrides
#[derive(Parser, Debug)]
#[command(name = "way-small", about = "A small Wayland compositor")]
struct Args {
    /// Which backend to use for compositor I/O
    #[arg(short, long, value_enum)]
    backend: Option<Backend>,
    /// The unix socket name/path to use for the Wayland wire protocol
    #[arg(short, long)]
    socket_path: Option<String>,
}

/// Start the chosen backend, and hand back the blocking task to join on if it
/// runs on a thread of its own.
///
/// The winit backend has to own a thread: its event loop and GL context are
/// bound to one, and both must be up before the socket is advertised — hence
/// waiting on `ready_rx` here rather than letting the caller race it.
async fn spawn_backend(
    backend: Backend,
    backend_message_tx: tokio::sync::mpsc::Sender<BackendMessage>,
    backend_request_rx: tokio::sync::mpsc::Receiver<BackendRequest>,
    frame_rx: watch::Receiver<Frame>,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Option<tokio::task::JoinHandle<anyhow::Result<()>>> {
    match backend {
        Backend::Winit => {
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
            let handle = tokio::task::spawn_blocking(move || {
                backend::winit::run_winit_backend(
                    backend_message_tx,
                    &cancel_token,
                    ready_tx,
                    frame_rx,
                    backend_request_rx,
                )
            });
            let _ = ready_rx.await;
            Some(handle)
        }
        Backend::None => {
            tokio::spawn(async move {
                backend::null::run_null_backend(
                    backend_message_tx,
                    frame_rx,
                    backend_request_rx,
                    cancel_token,
                )
                .await
            });
            None
        }
    }
}

/// Main method (entry-point)
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
        format!("{runtime_dir}/{socket_name}")
    };

    info!("Using backend: {:?}", backend);
    info!(
        "Wayland socket: {} (WAYLAND_DISPLAY={})",
        socket_path, socket_name
    );

    debug!("Creating channels");

    // Socket -> Compositor
    let (wayland_message_tx, wayland_message_rx) = channel::<WaylandSocketMessage>(1000);
    // Backend -> Compositor
    let (backend_message_tx, backend_message_rx) = channel::<BackendMessage>(1000);
    // Compositor -> Backend
    let (backend_request_tx, backend_request_rx) = channel::<BackendRequest>(64);
    // Compositor -> Backend (Latest only - watch::channel)
    let (frame_tx, frame_rx) = watch::channel::<Frame>(Frame::new());

    // Top-level cancellation token
    let cancel_token = tokio_util::sync::CancellationToken::new();

    debug!("Spawning subsystem tasks");

    let backend_handle = spawn_backend(
        backend,
        backend_message_tx,
        backend_request_rx,
        frame_rx,
        cancel_token.clone(),
    )
    .await;

    unsafe { std::env::set_var("WAYLAND_DISPLAY", &socket_name) };

    let compositor_handle = tokio::spawn(compositor::run_compositor(
        wayland_message_rx,
        backend_message_rx,
        backend_request_tx,
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
        () = cancel_token.cancelled() => {
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
