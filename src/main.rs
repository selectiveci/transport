#![warn(clippy::perf, clippy::style, clippy::pedantic)]

use futures_util::{future, pin_mut, StreamExt};
use std::process::exit;
use std::sync::Arc;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <url> <runner_id>", args[0]);
        exit(1);
    }

    let url = Url::parse(&args[1]).unwrap();
    let runner_id: String = args[2].clone();

    let input_pipe_path = format!("/tmp/{runner_id}_1");
    let output_pipe_path = format!("/tmp/{runner_id}_2");

    let (pipe_tx, pipe_rx) = futures_channel::mpsc::unbounded();
    let (exit_tx, mut exit_rx) = mpsc::channel(1);
    let exit_tx_clone = exit_tx.clone(); // Clone exit_tx for use in the read_pipe function.
    tokio::spawn(read_pipe(input_pipe_path.clone(), pipe_tx, exit_tx_clone));

    let output_fifo = Arc::new(Mutex::new(
        OpenOptions::new()
            .write(true)
            .open(output_pipe_path)
            .await
            .unwrap(),
    ));
    let output_fifo_clone = Arc::clone(&output_fifo);

    if let Ok((ws_stream, _)) = connect_async(url).await {
        let (write, read) = ws_stream.split();
        let pipe_to_ws = pipe_rx.map(Ok).forward(write);
        let ws_to_pipe = {
            read.for_each(move |message_result| {
                let output_fifo = Arc::clone(&output_fifo_clone);
                let exit_tx_clone = exit_tx.clone();
                async move {
                    if cfg!(debug_assertions) {
                        match &message_result {
                            Ok(Message::Text(_)) => println!("Received Text message"),
                            Ok(Message::Binary(_)) => println!("Received Binary message"),
                            Ok(Message::Ping(_)) => println!("Received Ping message"),
                            Ok(Message::Pong(_)) => println!("Received Pong message"),
                            Ok(Message::Close(_)) => println!("Received Close message, exiting..."),
                            Err(e) => eprintln!("Error receiving message: {}", e),
                            _ => println!("Received an unexpected type of message"),
                        }
                    }

                    match &message_result {
                        Ok(Message::Close(_)) => {
                            let _ = exit_tx_clone
                                .send(())
                                .await
                                .expect("Failed to send exit signal");
                            return;
                        }
                        _ => {}
                    }

                    if let Ok(message) = message_result {
                        let data = message.into_data();
                        let mut data_with_newline = data.clone();
                        data_with_newline.push(b'\n');
                        let mut output_fifo = output_fifo.lock().await;
                        output_fifo
                            .write_all(&data_with_newline)
                            .await
                            .expect("Failed to write to output pipe");
                        output_fifo
                            .flush()
                            .await
                            .expect("Failed to flush output pipe");
                        drop(output_fifo);
                    }
                }
            })
        };

        let exit_fut = async {
            exit_rx.recv().await;
            exit(0);
        };

        pin_mut!(pipe_to_ws, ws_to_pipe, exit_fut);
        future::select(future::select(pipe_to_ws, ws_to_pipe), exit_fut).await;
    } else {
        eprintln!("Failed to connect to the WebSocket server. Exiting...");
        exit(1);
    }
}

async fn read_pipe(
    path: String,
    tx: futures_channel::mpsc::UnboundedSender<Message>,
    exit_tx: mpsc::Sender<()>,
) {
    let mut pipe = OpenOptions::new().read(true).open(path).await.unwrap();
    let mut buf = Vec::new();
    loop {
        let mut temp_buf = vec![0; 1024];
        let n = match pipe.read(&mut temp_buf).await {
            Err(_) | Ok(0) => break,
            Ok(n) => n,
        };
        buf.extend(&temp_buf[..n]);

        // Split the buffer by '\n' and process each line
        while let Some(i) = buf.iter().position(|&b| b == b'\n') {
            let line = buf.drain(..=i).collect::<Vec<_>>();
            let line_str = String::from_utf8(line.clone()).unwrap();
            if line_str.trim() == "exit" {
                exit_tx.send(()).await.expect("Failed to send exit signal");
                return;
            }
            tx.unbounded_send(Message::text(line_str)).unwrap();
        }
    }
}
