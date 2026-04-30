use std::{collections::VecDeque, sync::Arc};

use sendfd::{RecvWithFd, SendWithFd};
use tokio::{
    net::{UnixSocket, UnixStream},
    select,
    sync::mpsc::{Sender, channel},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

pub struct WaylandMessage {
    pub object_id: u32,
    pub op_code: u16,
    pub args: Vec<u8>,
    pub fds: VecDeque<i32>,
}

pub struct WaylandSocketMessage {
    pub message: WaylandMessage,
    pub send_channel: Sender<WaylandMessage>,
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

    loop {
        let res = select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _)) => {
                        let stream = Arc::new(stream);
                        let compositor_message_channel = compositor_message_sender.clone();
                        handle_client(stream, compositor_message_channel, cancel_token.clone()).await
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
    info!("Wayland socket shutting down...");
    Ok(())
}

async fn handle_client(
    stream: Arc<UnixStream>,
    compositor_message_channel: Sender<WaylandSocketMessage>,
    cancel_token: CancellationToken,
) -> anyhow::Result<()> {
    let sender_stream = stream.clone();
    tokio::spawn(async move {
        debug!("New client connected");
        let mut data = VecDeque::<u8>::new();
        let mut pending_fds = VecDeque::<i32>::new();
        let (socket_send_tx, socket_send_rx) = channel::<WaylandMessage>(1);

        // Spawn a task to handle sending messages back to the client
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

                            if let Err(e) = sender_stream
                                .send_with_fd(&buffer, &message.fds.iter().cloned().collect::<Vec<i32>>())
                            {
                                debug!("Error sending message to client: {}", e);
                                break;
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

        loop {
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
            let result = stream.recv_with_fd(&mut buffer, &mut fds);
            match result {
                Ok((0, 0)) => {
                    debug!("Client disconnected");
                    break;
                }
                Ok((data_read, fds_read)) => {
                    // Append the received data to the buffer
                    for byte in &buffer[..data_read] {
                        data.push_back(*byte);
                    }

                    // Append the received file descriptors to the pending_fds queue
                    for &fd in &fds[..fds_read] {
                        pending_fds.push_back(fd);
                    }

                    while data.len() >= 8 {
                        // Peak bytes 4-7 to check if we have a complete message
                        let message_length_and_opcode =
                            u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                        let message_length = (message_length_and_opcode >> 16) as u16;
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

                        let msg = WaylandMessage {
                            object_id,
                            op_code: op_code as u16,
                            args: args_buffer,
                            fds: pending_fds.clone(),
                        };

                        compositor_message_channel
                            .clone()
                            .try_send(WaylandSocketMessage {
                                message: msg,
                                send_channel: socket_send_tx.clone(),
                            })
                            .ok();

                        pending_fds.clear();
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
    });
    Ok(())
}
