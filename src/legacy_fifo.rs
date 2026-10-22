use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::{future, pin_mut, StreamExt};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;
use url::Url;

pub async fn run(url: &str, runner_id: &str) -> i32 {
    #[cfg(not(unix))]
    {
        let _ = (url, runner_id);
        eprintln!("FIFO transport is only available on unix. Use --stdio.");
        return 1;
    }
    #[cfg(unix)]
    {
        run_unix(url, runner_id).await
    }
}

#[cfg(unix)]
async fn run_unix(url: &str, runner_id: &str) -> i32 {
    let url = Url::parse(url).unwrap();
    let input_pipe_path = format!("/tmp/{runner_id}_1");
    let output_pipe_path = format!("/tmp/{runner_id}_2");

    let (pipe_tx, pipe_rx) = futures_channel::mpsc::unbounded();
    let (exit_tx, mut exit_rx) = mpsc::channel(1);
    let exit_tx_clone = exit_tx.clone();
    tokio::spawn(read_pipe(
        input_pipe_path.clone(),
        pipe_tx.clone(),
        exit_tx_clone,
    ));
    let keepalive = crate::session::Keepalive::from_env();
    let last_pong = Arc::new(AtomicU64::new(unix_millis()));
    if keepalive.ping_interval.is_zero() {
        drop(pipe_tx);
    } else {
        let ping_tx = pipe_tx;
        let interval = keepalive.ping_interval;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if ping_tx
                    .unbounded_send(Message::Ping(b"selective".to_vec()))
                    .is_err()
                {
                    break;
                }
            }
        });
        let watched = Arc::clone(&last_pong);
        let exit_tx_ping = exit_tx.clone();
        let budget = keepalive.ping_interval + keepalive.pong_timeout;
        tokio::spawn(async move {
            let limit = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX);
            loop {
                tokio::time::sleep(keepalive.pong_timeout).await;
                let stale = unix_millis().saturating_sub(watched.load(Ordering::SeqCst));
                if stale >= limit {
                    eprintln!("websocket pong timeout");
                    let _ = exit_tx_ping.send(()).await;
                    break;
                }
            }
        });
    }

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
            let last_pong = Arc::clone(&last_pong);
            read.for_each(move |message_result| {
                let output_fifo = Arc::clone(&output_fifo_clone);
                let exit_tx_clone = exit_tx.clone();
                let last_pong = Arc::clone(&last_pong);
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
                        Ok(Message::Pong(_)) => {
                            last_pong.store(unix_millis(), Ordering::SeqCst);
                            return;
                        }
                        Ok(Message::Ping(_) | Message::Frame(_)) => return,
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
        };

        pin_mut!(pipe_to_ws, ws_to_pipe, exit_fut);
        future::select(future::select(pipe_to_ws, ws_to_pipe), exit_fut).await;
        0
    } else {
        eprintln!("Failed to connect to the WebSocket server. Exiting...");
        1
    }
}

#[cfg(unix)]
fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(unix)]
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
