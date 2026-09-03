//! Wayland Unix socket listener and per-client I/O.
//!
//! Accepts client connections on the Wayland socket, spawns read/write tasks
//! per client, frames the wire protocol (8-byte header + args), passes FDs,
//! and forwards parsed messages to the compositor via a channel.

use sendfd::{RecvWithFd, SendWithFd};
use std::{
    collections::VecDeque,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
};
use tokio::{
    net::{UnixSocket, UnixStream},
    select,
    sync::mpsc::{Sender, channel},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

/// The atomic client ID counter, assigned as clients connect
static NEXT_CLIENT_ID: AtomicU32 = AtomicU32::new(1);

/// Maximum number of pending clients on the unix socket
const SOCKET_CLIENT_BACKLOG: u32 = 1024;

/// Size of the ancillary buffer used when receiving file descriptors.
const MAX_FDS_IN: usize = 28;

/// Maximum number of file descriptors a client may have queued but unclaimed.
const MAX_PENDING_FDS: usize = 256;

/// Maximum number of messages that may be queued for a single client before we
/// give up on it.
pub const CLIENT_SEND_QUEUE_LIMIT: usize = 4096;

/// A low-level (untyped) wayland message
#[derive(Debug)]
pub struct WaylandProtocolMessage {
    /// The object being acted upon
    pub object_id: u32,
    /// The op code (method) to call
    pub op_code: u16,
    /// Arguments to the operation
    pub args: Vec<u8>,
    /// File descriptors to attach as ancillary data when sending this message.
    /// Always empty on inbound messages: inbound fds are on the client's `fd_queue`
    pub fds: Vec<OwnedFd>,
}

/// A wayland protocol message with an associated client-id
pub struct WaylandProtocolMessageWithClientInfo {
    /// The client id associated with the message
    pub client_id: u32,
    /// The wayland protocol message
    pub message: WaylandProtocolMessage,
}

/// A message from this thread notifying the compositor of a new client connection
pub struct WaylandNewClientMessage {
    /// The id for the new client
    pub client_id: u32,
    /// A sender for sending wayland protocol messages back to this client
    pub socket_sender: Sender<WaylandProtocolMessage>,
    /// A shared queue of file-descriptors collected/pending on this client
    pub fd_queue: Arc<Mutex<VecDeque<OwnedFd>>>,
    /// Cancels just this client's socket tasks, leaving other clients running.
    /// The compositor triggers it to drop a client that has stopped reading.
    pub client_cancel_token: CancellationToken,
}

/// The top-level type for channel messages sent from this thread to the compositor thread
pub enum WaylandSocketMessage {
    /// A message denoting a new client connection
    NewClient(WaylandNewClientMessage),
    /// A wayland protocol message from some client
    Message(WaylandProtocolMessageWithClientInfo),
    /// A message denoting a client hanging up the socket
    ClientDisconnected { client_id: u32 },
}

/// Run a listening socket/loop until told to shutdown
pub async fn run_wayland_socket(
    socket_path: String,
    compositor_message_sender: Sender<WaylandSocketMessage>,
    cancel_token: CancellationToken,
) -> anyhow::Result<()> {
    if std::path::Path::new(&socket_path).exists() {
        std::fs::remove_file(&socket_path)?;
    }
    let socket = UnixSocket::new_stream()?;
    socket.bind(&socket_path)?;

    let listener = socket.listen(SOCKET_CLIENT_BACKLOG)?;

    debug!("Wayland socket listening on {}", socket_path);

    let mut client_handles: Vec<JoinHandle<()>> = Vec::new();

    loop {
        let res = select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _)) => {
                        let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
                        let stream = Arc::new(stream);
                        let compositor_message_channel = compositor_message_sender.clone();
                        let handle = handle_client(client_id, stream, compositor_message_channel, cancel_token.clone());
                        client_handles.push(handle);
                        Ok(())
                    }
                    Err(e) => {
                        debug!("Error accepting client: {}", e);
                        Err(anyhow::anyhow!("Error accepting client: {e}"))
                    }
                }
            }
            () = cancel_token.cancelled() => {
                Err(anyhow::anyhow!("Wayland socket received shutdown signal"))
            }
        };
        if res.is_err() {
            break;
        }
    }

    debug!("Waiting for client socket threads to terminate");
    for handle in client_handles {
        let _ = handle.await;
    }

    info!("Wayland socket shutting down...");
    if let Err(e) = std::fs::remove_file(&socket_path) {
        debug!("Failed to remove socket file: {}", e);
    }
    Ok(())
}

/// Handle and individual client
#[allow(clippy::too_many_lines)]
fn handle_client(
    client_id: u32,
    stream: Arc<UnixStream>,
    compositor_message_channel: Sender<WaylandSocketMessage>,
    cancel_token: CancellationToken,
) -> JoinHandle<()> {
    let sender_stream = stream.clone();
    tokio::spawn(async move {
        debug!("New client connected");
        let mut data = VecDeque::<u8>::new();
        let pending_fds_arc = Arc::new(Mutex::new(VecDeque::<OwnedFd>::new()));
        let (socket_send_tx, socket_send_rx) =
            channel::<WaylandProtocolMessage>(CLIENT_SEND_QUEUE_LIMIT);

        let cancel_token = cancel_token.child_token();

        compositor_message_channel
            .send(WaylandSocketMessage::NewClient(WaylandNewClientMessage {
                client_id,
                socket_sender: socket_send_tx.clone(),
                fd_queue: pending_fds_arc.clone(),
                client_cancel_token: cancel_token.clone(),
            }))
            .await
            .unwrap();

        let sender_cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            let mut socket_send_rx = socket_send_rx;

            debug!("Wayland socket send task started");

            loop {
                select! {
                    message = socket_send_rx.recv() => {
                        if let Some(message) = message {
                            let mut buffer = Vec::new();
                            buffer.extend_from_slice(&message.object_id.to_le_bytes());
                            let message_length_and_opcode =
                                ((u32::try_from(message.args.len()).expect("args should not be a length exceeds u32::MAX") + 8) << 16) | u32::from(message.op_code);
                            buffer.extend_from_slice(&message_length_and_opcode.to_le_bytes());
                            buffer.extend_from_slice(&message.args);

                            let raw_fds: Vec<RawFd> =
                                message.fds.iter().map(AsRawFd::as_raw_fd).collect();
                            let mut bytes_sent = 0;
                            let mut fds_sent = false;
                            while bytes_sent < buffer.len() {
                                let writable = select! {
                                    res = sender_stream.writable() => res,
                                    () = sender_cancel_token.cancelled() => {
                                        debug!("Wayland socket send task cancelled mid-write");
                                        return;
                                    }
                                };
                                if let Err(e) = writable {
                                    debug!("Error waiting for writable: {}", e);
                                    return;
                                }
                                let fds_to_send = if fds_sent { &[] } else { &raw_fds[..] };
                                match sender_stream.try_io(tokio::io::Interest::WRITABLE, || {
                                    sender_stream.send_with_fd(&buffer[bytes_sent..], fds_to_send)
                                }) {
                                    Ok(n) => {
                                        bytes_sent += n;
                                        fds_sent = true;
                                    }
                                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
                                    Err(e) => {
                                        debug!("Error sending message to client: {}", e);
                                        return;
                                    }
                                }
                            }
                        } else {
                            debug!("Wayland socket send channel closed");
                            break;
                        }
                    }
                    () = sender_cancel_token.cancelled() => {
                        debug!("Wayland socket send task received shutdown signal");
                        break;
                    }
                }
            }
        });

        'outer: loop {
            select! {
                () = cancel_token.cancelled() => {
                    debug!("Wayland socket receive task received shutdown signal");
                    break;
                }
                res = stream.readable() => {
                    match res {
                        Ok(()) => {}
                        Err(e) => {
                            debug!("Client disconnected: {}", e);
                            break;
                        }
                    }
                }
            }

            let mut buffer = [0u8; 4096];
            let mut fds = [0; MAX_FDS_IN];
            let result = stream.try_io(tokio::io::Interest::READABLE, || {
                stream.recv_with_fd(&mut buffer, &mut fds)
            });
            match result {
                Ok((0, _)) => {
                    debug!("Client disconnected");
                    break;
                }
                Ok((data_read, fds_read)) => {
                    for byte in &buffer[..data_read] {
                        data.push_back(*byte);
                    }

                    {
                        let mut pending_fds = pending_fds_arc.lock().unwrap();
                        for &fd in &fds[..fds_read] {
                            // `sendfd` does not pass MSG_CMSG_CLOEXEC, so mark the
                            // descriptor close-on-exec ourselves. Not atomic with
                            // the recvmsg, but way-small never forks, so there is
                            // no window for it to leak into a child.
                            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
                            pending_fds.push_back(unsafe { OwnedFd::from_raw_fd(fd) });
                        }
                        if pending_fds.len() > MAX_PENDING_FDS {
                            tracing::warn!(
                                "Client has {} unclaimed file descriptors queued (limit {}), disconnecting it",
                                pending_fds.len(),
                                MAX_PENDING_FDS,
                            );
                            // Close all of the fds and exit
                            pending_fds.clear();
                            drop(pending_fds);
                            cancel_token.cancel();
                            break 'outer;
                        }
                    }

                    while data.len() >= 8 {
                        // Peak bytes 4-7 to check if we have a complete message
                        let message_length_and_opcode =
                            u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                        let message_length = (message_length_and_opcode >> 16) as u16;
                        if message_length < 8 {
                            debug!(
                                "Invalid message length {} from client, disconnecting",
                                message_length
                            );
                            break 'outer;
                        }
                        if data.len() < message_length as usize {
                            break;
                        }

                        // Start popping the message (object id)
                        let object_id = u32::from_le_bytes([
                            data.pop_front().unwrap(),
                            data.pop_front().unwrap(),
                            data.pop_front().unwrap(),
                            data.pop_front().unwrap(),
                        ]);
                        let op_code = (message_length_and_opcode & 0xFFFF) as u16;

                        // Now pop the length and opcode bytes, we already read them without
                        // popping, so we need to pop them now to move the buffer forward
                        data.pop_front();
                        data.pop_front();
                        data.pop_front();
                        data.pop_front();

                        let mut args_buffer = vec![0u8; message_length as usize - 8];
                        (0..args_buffer.len()).for_each(|i| {
                            args_buffer[i] = data.pop_front().unwrap();
                        });

                        let msg = WaylandProtocolMessage {
                            object_id,
                            op_code,
                            args: args_buffer,
                            // FDs are not included in incoming messages, they have to be read
                            // from the pending_queue (we can't know what FDs belong to
                            // what message without looking at the message and registry
                            // and those are owned/managed by the compositor thread.)
                            fds: vec![],
                        };

                        if let Err(e) = compositor_message_channel
                            .send(WaylandSocketMessage::Message(
                                WaylandProtocolMessageWithClientInfo {
                                    client_id,
                                    message: msg,
                                },
                            ))
                            .await
                        {
                            debug!("Failed to send message to compositor: {}", e);
                            break 'outer;
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    debug!("Client disconnected: {}", e);
                    break;
                }
            }
        }

        let _ = compositor_message_channel
            .send(WaylandSocketMessage::ClientDisconnected { client_id })
            .await;
    })
}
