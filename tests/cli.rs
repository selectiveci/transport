use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::timeout;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_transport")
}

async fn within<F: std::future::Future>(future: F) -> F::Output {
    timeout(Duration::from_secs(5), future)
        .await
        .expect("timed out after 5s")
}

#[cfg(unix)]
#[tokio::test]
async fn fifo_round_trip_keeps_the_legacy_framing() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (id, input, output) = fifo_paths();
    let mut child = Command::new(bin())
        .arg(format!("ws://{addr}"))
        .arg(&id)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (writer, reader) = open_fifos(&input, &output);
    let (sock, _) = within(listener.accept()).await.unwrap();
    let mut ws = accept_async(sock).await.unwrap();
    let mut to_transport = writer.await.unwrap();
    let mut from_transport = BufReader::new(reader.await.unwrap());
    to_transport
        .write_all(b"{\"type\":\"ping\"}\n")
        .await
        .unwrap();
    to_transport.flush().await.unwrap();
    let frame = within(ws.next()).await.unwrap().unwrap();
    match frame {
        Message::Text(text) => assert_eq!(text, "{\"type\":\"ping\"}\n"),
        other => panic!("unexpected {other:?}"),
    }
    ws.send(Message::Text("{\"command\":\"connected\"}".into()))
        .await
        .unwrap();
    let mut line = String::new();
    within(from_transport.read_line(&mut line)).await.unwrap();
    assert_eq!(line, "{\"command\":\"connected\"}\n");
    to_transport.write_all(b"  exit  \n").await.unwrap();
    to_transport.flush().await.unwrap();
    let status = within(child.wait()).await.unwrap();
    assert!(status.success());
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
}

#[cfg(unix)]
fn open_fifos(
    input: &str,
    output: &str,
) -> (
    tokio::task::JoinHandle<tokio::fs::File>,
    tokio::task::JoinHandle<tokio::fs::File>,
) {
    let input_path = input.to_owned();
    let output_path = output.to_owned();
    let writer = tokio::spawn(async move {
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(input_path)
            .await
            .unwrap()
    });
    let reader = tokio::spawn(async move {
        tokio::fs::OpenOptions::new()
            .read(true)
            .open(output_path)
            .await
            .unwrap()
    });
    (writer, reader)
}

#[cfg(unix)]
fn fifo_paths() -> (String, String, String) {
    let id = format!(
        "seltest{}{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let input = format!("/tmp/{id}_1");
    let output = format!("/tmp/{id}_2");
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    assert!(std::process::Command::new("mkfifo")
        .arg(&input)
        .status()
        .unwrap()
        .success());
    assert!(std::process::Command::new("mkfifo")
        .arg(&output)
        .status()
        .unwrap()
        .success());
    (id, input, output)
}

#[cfg(unix)]
#[tokio::test]
async fn fifo_exits_when_connect_is_refused_while_the_input_pipe_stays_open() {
    let (id, input, output) = fifo_paths();
    let mut child = Command::new(bin())
        .arg("ws://127.0.0.1:1")
        .arg(&id)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (writer, reader) = open_fifos(&input, &output);
    let _held_input = within(writer).await.unwrap();
    let _held_output = within(reader).await.unwrap();
    let status = timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("fifo connect failure waited on the input pipe")
        .unwrap();
    assert_eq!(status.code(), Some(1));
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
}
