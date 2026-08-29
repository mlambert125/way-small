//! Wayland Unix socket listener and per-client I/O.
//!
//! Accepts client connections on the Wayland socket, spawns read/write tasks
//! per client, frames the wire protocol (8-byte header + args), passes FDs,
//! and forwards parsed messages to the compositor via a channel.

use std::{
    collections::VecDeque,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
};

use sendfd::{RecvWithFd, SendWithFd};
use tokio::{
    net::{UnixSocket, UnixStream},
    select,
    sync::mpsc::{Sender, channel},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

static NEXT_CLIENT_ID: AtomicU32 = AtomicU32::new(1);

/// Size of the ancillary buffer used when receiving file descriptors. Matches
/// libwayland's `MAX_FDS_OUT`, which bounds how many fds a well-behaved client
/// attaches to a single `sendmsg`. Note that `sendfd` does not surface
/// `MSG_CTRUNC`, so overflow past this would be silent — the kernel closes the
/// excess fds and the client's requests would then be missing them.
const MAX_FDS_IN: usize = 28;

/// Maximum number of file descriptors a client may have queued but unclaimed.
///
/// Requests that take no fd never drain the queue, so a client that attaches
/// descriptors to them would otherwise grow it without bound and exhaust the
/// compositor's `RLIMIT_NOFILE` — which would break every other client, plus
/// `mmap` and `memfd_create` process-wide. libwayland bounds the same queue
/// with a fixed-size ring and errors the connection on overflow; this is the
/// equivalent. Sized well above any legitimate burst: descriptors only arrive
/// with `wl_shm.create_pool` and `wl_data_offer.receive`, and the compositor
/// drains its whole message channel on every loop iteration.
const MAX_PENDING_FDS: usize = 256;

/// Maximum number of messages that may be queued for a single client before we
/// give up on it. A client that stops draining its socket must never be able to
/// stall the compositor loop, so once this many messages are outstanding the
/// client is disconnected instead of being waited on.
pub const CLIENT_SEND_QUEUE_LIMIT: usize = 4096;

pub struct WaylandProtocolMessage {
    pub object_id: u32,
    pub op_code: u16,
    pub args: Vec<u8>,

    /// File descriptors to attach as ancillary data when sending this message.
    ///
    /// Always empty on inbound messages: the socket task cannot tell which
    /// message an fd belongs to without protocol state, so incoming fds go on
    /// the client's `fd_queue` and are claimed by `protocol::handle_message`.
    ///
    /// `OwnedFd` rather than `RawFd` so that dropping a message closes its fds.
    /// `SCM_RIGHTS` duplicates the descriptor into the receiver, so we always
    /// own our copy and must close it whether or not the send succeeded.
    pub fds: Vec<OwnedFd>,
}

pub struct WaylandProtocolMessageWithClientInfo {
    pub client_id: u32,
    pub message: WaylandProtocolMessage,
}

pub struct WaylandNewClientMessage {
    pub client_id: u32,
    pub socket_sender: Sender<WaylandProtocolMessage>,
    pub fd_queue: Arc<Mutex<VecDeque<OwnedFd>>>,
    /// Cancels just this client's socket tasks, leaving other clients running.
    /// The compositor triggers it to drop a client that has stopped reading.
    pub client_cancel_token: CancellationToken,
}

pub enum WaylandSocketMessage {
    NewClient(WaylandNewClientMessage),
    Message(WaylandProtocolMessageWithClientInfo),
    ClientDisconnected { client_id: u32 },
}

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

    let listener = socket.listen(1024)?;

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

    for handle in client_handles {
        let _ = handle.await;
    }
    info!("Wayland socket shutting down...");
    if let Err(e) = std::fs::remove_file(&socket_path) {
        debug!("Failed to remove socket file: {}", e);
    }
    Ok(())
}

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

        // A child of the global token: cancelled by shutdown, but also
        // cancellable on its own so the compositor can drop this client alone.
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
                                // A client that has stopped reading can leave us
                                // parked here indefinitely, so honour cancellation.
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
                                let fds_to_send = if fds_sent { &[][..] } else { &raw_fds[..] };
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
                            // The kernel just handed us these descriptors, so this
                            // is the point ownership transfers to us.
                            pending_fds.push_back(unsafe { OwnedFd::from_raw_fd(fd) });
                        }
                        if pending_fds.len() > MAX_PENDING_FDS {
                            tracing::warn!(
                                "Client has {} unclaimed file descriptors queued (limit {}), disconnecting it",
                                pending_fds.len(),
                                MAX_PENDING_FDS,
                            );
                            // Dropping the queued `OwnedFd`s closes them.
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
                            fds: vec![], // FDs are not included in the message struct, they are
                                         // read separately and accessed via the pending_fds queue
                                         // in the WaylandNewClientMessage
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
