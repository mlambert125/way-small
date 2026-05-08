//! Wayland Unix socket listener and per-client I/O.
//!
//! Accepts client connections on the Wayland socket, spawns read/write tasks
//! per client, frames the wire protocol (8-byte header + args), passes FDs,
//! and forwards parsed messages to the compositor via a channel.

use std::{
    collections::VecDeque,
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

pub struct WaylandProtocolMessage {
    pub object_id: u32,
    pub op_code: u16,
    pub args: Vec<u8>,

    // This is a bit strange, but this is a shared queue of fds for a client
    // that the compositor should pop_front on if the specific object/op code expects an fd.
    // We can not calculate which message an fd belongs to at socket
    // level because doing so requires finding the type of the message
    // which requires looking up the id in the registry and then
    // decoding the message arguments to find the op code
    pub fd_queue: Arc<Mutex<VecDeque<i32>>>,
}

pub struct WaylandProtocolMessageWithClientInfo {
    pub client_id: u32,
    pub message: WaylandProtocolMessage,
    pub socket_sender: Sender<WaylandProtocolMessage>,
}

pub enum WaylandSocketMessage {
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
                        Err(anyhow::anyhow!("Error accepting client: {}", e))
                    }
                }
            }
            _ = cancel_token.cancelled() => {
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
        let pending_fds_arc = Arc::new(Mutex::new(VecDeque::<i32>::new()));
        let (socket_send_tx, socket_send_rx) = channel::<WaylandProtocolMessage>(64);

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
                                ((message.args.len() as u32 + 8) << 16) | (message.op_code as u32);
                            buffer.extend_from_slice(&message_length_and_opcode.to_le_bytes());
                            buffer.extend_from_slice(&message.args);

                            let fds_vec: Vec<i32> = message.fd_queue.lock().unwrap()
                                .iter().cloned().collect();
                            let mut bytes_sent = 0;
                            let mut fds_sent = false;
                            while bytes_sent < buffer.len() {
                                if let Err(e) = sender_stream.writable().await {
                                    debug!("Error waiting for writable: {}", e);
                                    return;
                                }
                                let fds_to_send = if fds_sent { &[][..] } else { &fds_vec[..] };
                                match sender_stream.try_io(tokio::io::Interest::WRITABLE, || {
                                    sender_stream.send_with_fd(&buffer[bytes_sent..], fds_to_send)
                                }) {
                                    Ok(n) => {
                                        bytes_sent += n;
                                        fds_sent = true;
                                    }
                                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
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
                    _ = sender_cancel_token.cancelled() => {
                        debug!("Wayland socket send task received shutdown signal");
                        break;
                    }
                }
            }
        });

        'outer: loop {
            select! {
                 _ = cancel_token.cancelled() => {
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
            let mut fds = [0; 10];
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
                            pending_fds.push_back(fd);
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
                        let op_code = (message_length_and_opcode & 0xFFFF) as usize;

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
                            op_code: op_code as u16,
                            args: args_buffer,
                            fd_queue: pending_fds_arc.clone(),
                        };

                        if let Err(e) = compositor_message_channel
                            .send(WaylandSocketMessage::Message(
                                WaylandProtocolMessageWithClientInfo {
                                    client_id,
                                    message: msg,
                                    socket_sender: socket_send_tx.clone(),
                                },
                            ))
                            .await
                        {
                            debug!("Failed to send message to compositor: {}", e);
                            break 'outer;
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    continue;
                }
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
