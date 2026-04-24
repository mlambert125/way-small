use std::collections::VecDeque;

use futures::pin_mut;
use sendfd::RecvWithFd;
use tokio::{
    net::{UnixSocket, UnixStream},
    sync::mpsc::{self, Receiver},
};
use tracing::{debug, info};

use async_stream::try_stream;
use futures_core::stream::Stream;
use futures_util::stream::StreamExt;

struct WaylandClient {
    stream: UnixStream,
}
impl WaylandClient {
    fn message_stream(&mut self) -> impl Stream<Item = anyhow::Result<RawWaylandMessage>> {
        try_stream! {
            let mut data = VecDeque::<u8>::new();
            let mut pending_fds = VecDeque::<i32>::new();

            loop {
                let mut buffer = [0u8; 4096];
                let mut fds = [0; 10];
                let result = self.stream.recv_with_fd(&mut buffer, &mut fds);

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

                            yield RawWaylandMessage {
                                object_id,
                                op_code: op_code as u16,
                                args: args_buffer,
                                fds: pending_fds.clone(),
                            };
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
        }
    }
}

struct RawWaylandMessage {
    object_id: u32,
    op_code: u16,
    args: Vec<u8>,
    fds: VecDeque<i32>,
}

async fn compositor_loop(
    mut rx: Receiver<(&mut WaylandClient, RawWaylandMessage)>,
) -> anyhow::Result<()> {
    // This is where the main compositor logic would go, handling clients, rendering, etc.
    while let Some((_, message)) = rx.recv().await {
        info!(
            "Processing message: object_id={}, op_code={}, args={:?}, fds={:?}",
            message.object_id, message.op_code, message.args, message.fds
        );
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let (tx, rx) = mpsc::channel(1000);

    let compositor_handle = tokio::spawn(async move {
        if let Err(e) = compositor_loop(rx).await {
            eprintln!("Compositor loop error: {}", e);
        }
    });

    let clients = wayland_client_stream();
    pin_mut!(clients);

    while let Some(client) = clients.next().await {
        match client {
            Ok(mut client) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    debug!("New client connected");
                    let message_stream = client.message_stream();
                    pin_mut!(message_stream);

                    while let Some(message) = message_stream.next().await {
                        match message {
                            Ok(msg) => {
                                info!(
                                    "Received message: object_id={}, op_code={}, args={:?}, fds={:?}",
                                    msg.object_id, msg.op_code, msg.args, msg.fds
                                );
                                match tx.send((client, msg)).await {
                                    Ok(_) => {}
                                    Err(e) => {
                                        debug!("Error sending message to compositor: {}", e);
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                debug!("Error receiving message: {}", e);
                                break;
                            }
                        }
                    }
                    debug!("Client message stream ended");
                });
            }
            Err(e) => {
                debug!("Error accepting client: {}", e);
            }
        }
    }

    compositor_handle.await?;

    Ok(())
}

fn wayland_client_stream() -> impl Stream<Item = anyhow::Result<WaylandClient>> {
    try_stream! {
        let socket_path = "/tmp/way-small.sock";
        let _ = std::fs::remove_file(socket_path);

        let socket = UnixSocket::new_stream()?;
        socket.bind(socket_path)?;

        let listener = socket.listen(1024)?;
        debug!("Listening on {:?}", socket_path);

        loop {
            let (stream, _) = listener.accept().await?;
            yield WaylandClient { stream };
        }
    }
}
