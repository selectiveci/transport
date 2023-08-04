use futures_util::{future, pin_mut, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tokio::fs::OpenOptions;
use url::Url;
use std::sync::Arc;
use tokio::sync::Mutex;
use std::process::exit;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <url> <runner_id>", args[0]);
        exit(1);
    }

    let url = Url::parse(&args[1]).unwrap();
    let runner_id: String = args[2].clone();

    let input_pipe_path = format!("/tmp/{}_1", runner_id);
    let output_pipe_path = format!("/tmp/{}_2", runner_id);

    let (pipe_tx, pipe_rx) = futures_channel::mpsc::unbounded();
    tokio::spawn(read_pipe(input_pipe_path.clone(), pipe_tx));

    let output_fifo = Arc::new(Mutex::new(OpenOptions::new().write(true).open(output_pipe_path).await.unwrap()));
    let output_fifo_clone = Arc::clone(&output_fifo);

    match connect_async(url).await {
        Ok((ws_stream, _)) => {
            let (write, read) = ws_stream.split();
            let pipe_to_ws = pipe_rx.map(Ok).forward(write);
            let ws_to_pipe = {
                read.for_each(move |message| {
                    let output_fifo = Arc::clone(&output_fifo_clone);
                    async move {
                        let mut data = message.unwrap().into_data();
                        data.push('\n' as u8);
                        let mut output_fifo= output_fifo.lock().await;
                        output_fifo.write_all(&data).await.expect("Failed to write to output pipe");
                        output_fifo.flush().await.expect("Failed to flush output pipe");
                    }
                })
            };

            pin_mut!(pipe_to_ws, ws_to_pipe);
            future::select(pipe_to_ws, ws_to_pipe).await;
        },
        Err(_) => {
            eprintln!("Failed to connect to the WebSocket server. Exiting...");
            exit(1);
        }
    }
}

// Our helper method which will read data from the input pipe and send it along the
// sender provided.
async fn read_pipe(path: String, tx: futures_channel::mpsc::UnboundedSender<Message>) {
    let mut pipe = OpenOptions::new().read(true).open(path).await.unwrap();
    let mut buf = Vec::new();
    loop {
        let mut temp_buf = vec![0; 1024];
        let n = match pipe.read(&mut temp_buf).await {
            Err(_) | Ok(0) => break,
            Ok(n) => n,
        };
        buf.extend(&temp_buf[..n]);
        while let Some(i) = buf.iter().position(|&b| b == b'\n') {
            let line = buf.drain(..=i).collect::<Vec<_>>();
            tx.unbounded_send(Message::text(String::from_utf8(line).unwrap())).unwrap();
        }
    }
}
