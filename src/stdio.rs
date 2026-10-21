use std::io::{BufRead, Write};
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use crate::error::Error;
use crate::session::Session;

pub async fn run() -> i32 {
    let mut lines = spawn_stdin_reader();
    let url = match resolve_url(&mut lines).await {
        Ok(url) => url,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };
    let mut session = match Session::connect(&url).await {
        Ok(session) => session,
        Err(err) => {
            eprintln!("Failed to connect to the WebSocket server. Exiting... {err}");
            return 1;
        }
    };
    loop {
        tokio::select! {
            incoming = session.recv() => {
                match incoming {
                    Ok(Some(frame)) => {
                        if write_stdout(&frame).is_err() {
                            return 1;
                        }
                    }
                    Ok(None) => return 0,
                    Err(err) => {
                        eprintln!("{err}");
                        return 1;
                    }
                }
            }
            outgoing = lines.recv() => {
                match outgoing {
                    Some(Ok(line)) => {
                        if line.trim() == "exit" {
                            let _ = session.close().await;
                            return 0;
                        }
                        if session.send(&line).await.is_err() {
                            eprintln!("Failed to forward a frame. Exiting...");
                            return 1;
                        }
                    }
                    Some(Err(err)) => {
                        eprintln!("{err}");
                        return 1;
                    }
                    None => {
                        let _ = session.close().await;
                        return 0;
                    }
                }
            }
        }
    }
}

fn spawn_stdin_reader() -> UnboundedReceiver<Result<String, Error>> {
    let (tx, rx) = unbounded_channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let reader = std::io::BufReader::new(stdin.lock());
        for line in reader.lines() {
            let message = line.map_err(Error::from);
            if tx.send(message).is_err() {
                break;
            }
        }
    });
    rx
}

fn write_stdout(frame: &str) -> Result<(), Error> {
    let mut stdout = std::io::stdout();
    stdout.write_all(frame.as_bytes())?;
    if !frame.ends_with('\n') {
        stdout.write_all(b"\n")?;
    }
    stdout.flush()?;
    Ok(())
}

async fn resolve_url(
    lines: &mut UnboundedReceiver<Result<String, Error>>,
) -> Result<String, Error> {
    if let Ok(url) = std::env::var("SELECTIVE_TRANSPORT_URL") {
        if !url.is_empty() {
            return Ok(url);
        }
    }
    match tokio::time::timeout(Duration::from_secs(30), lines.recv()).await {
        Ok(Some(Ok(line))) => {
            let url = line.trim().to_owned();
            if url.starts_with("ws://") || url.starts_with("wss://") {
                Ok(url)
            } else {
                Err(Error::new(
                    "the first stdin line must be a ws:// or wss:// url when SELECTIVE_TRANSPORT_URL is unset",
                ))
            }
        }
        _ => Err(Error::new(
            "SELECTIVE_TRANSPORT_URL is unset and stdin did not provide a websocket url",
        )),
    }
}
