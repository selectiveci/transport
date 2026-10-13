use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

const WAIT: Duration = Duration::from_secs(10);
const CONNECTED: &str = r#"{"command":"connected"}"#;
const RUN_TEST_CASES: &str = r#"{"command":"run_test_cases"}"#;
const RESUME: &str = r#"{"command":"transport_reconnect","headers":{"x-test":"1"}}"#;

// The two named pipes the transport talks to the client library over.
struct Pipes {
    runner_id: String,
}

impl Pipes {
    fn new(name: &str) -> Self {
        let pipes = Pipes {
            runner_id: format!("transport-test-{name}-{}", std::process::id()),
        };

        for path in [pipes.input_path(), pipes.output_path()] {
            let _ = std::fs::remove_file(&path);
            let status = std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap();
            assert!(status.success(), "mkfifo {path} failed");
        }

        pipes
    }

    // Read by the transport, written by the client.
    fn input_path(&self) -> String {
        format!("/tmp/{}_1", self.runner_id)
    }

    // Written by the transport, read by the client.
    fn output_path(&self) -> String {
        format!("/tmp/{}_2", self.runner_id)
    }

    async fn open(&self) -> (BufReader<File>, File) {
        let mut read = OpenOptions::new();
        read.read(true);
        let mut write = OpenOptions::new();
        write.write(true);

        let (output, input) = timeout(WAIT, async {
            tokio::join!(read.open(self.output_path()), write.open(self.input_path()))
        })
        .await
        .expect("timed out opening the pipes");

        (BufReader::new(output.unwrap()), input.unwrap())
    }
}

impl Drop for Pipes {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.input_path());
        let _ = std::fs::remove_file(self.output_path());
    }
}

struct Accepted {
    socket: WebSocketStream<TcpStream>,
    uri: String,
    x_test: Option<String>,
}

fn spawn_transport(listener: &TcpListener, pipes: &Pipes) -> Child {
    let url = format!(
        "ws://{}/transport/websocket?runner_id={}",
        listener.local_addr().unwrap(),
        pipes.runner_id
    );

    Command::new(env!("CARGO_BIN_EXE_transport"))
        .arg(url)
        .arg(&pipes.runner_id)
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

// The handshake callback's error type is fixed by tungstenite.
#[allow(clippy::result_large_err)]
async fn accept(listener: &TcpListener) -> Accepted {
    let (stream, _) = timeout(WAIT, listener.accept())
        .await
        .expect("timed out waiting for the transport to connect")
        .unwrap();

    let mut seen = None;
    let socket =
        tokio_tungstenite::accept_hdr_async(stream, |request: &Request, response: Response| {
            seen = Some((
                request.uri().to_string(),
                request
                    .headers()
                    .get("x-test")
                    .and_then(|value| value.to_str().ok())
                    .map(String::from),
            ));
            Ok(response)
        })
        .await
        .unwrap();

    let (uri, x_test) = seen.unwrap();
    Accepted {
        socket,
        uri,
        x_test,
    }
}

async fn send(socket: &mut WebSocketStream<TcpStream>, text: &str) {
    socket.send(Message::text(text)).await.unwrap();
}

async fn next_text(socket: &mut WebSocketStream<TcpStream>) -> String {
    timeout(WAIT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => return text,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                other => panic!("expected a text message, got {other:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for a message from the transport")
}

async fn read_line(output: &mut BufReader<File>) -> String {
    let mut line = String::new();
    timeout(WAIT, output.read_line(&mut line))
        .await
        .expect("timed out waiting for the transport to write to the pipe")
        .unwrap();
    line.trim_end().to_string()
}

async fn write_line(input: &mut File, line: &str) {
    input
        .write_all(format!("{line}\n").as_bytes())
        .await
        .unwrap();
    input.flush().await.unwrap();
}

#[tokio::test]
async fn moves_to_a_new_connection_when_asked() {
    let pipes = Pipes::new("resume");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut child = spawn_transport(&listener, &pipes);
    let (mut output, mut input) = pipes.open().await;

    let mut first = accept(&listener).await;
    assert!(
        first.uri.contains("transport_features=resume"),
        "{}",
        first.uri
    );
    assert!(!first.uri.contains("resume=true"), "{}", first.uri);

    send(&mut first.socket, CONNECTED).await;
    assert_eq!(read_line(&mut output).await, CONNECTED);

    write_line(&mut input, "hello-1").await;
    assert_eq!(next_text(&mut first.socket).await.trim_end(), "hello-1");

    send(&mut first.socket, RESUME).await;

    let mut second = accept(&listener).await;
    assert!(second.uri.contains("reconnect=true"), "{}", second.uri);
    assert!(second.uri.contains("resume=true"), "{}", second.uri);
    assert!(
        second.uri.contains("transport_features=resume"),
        "{}",
        second.uri
    );
    assert_eq!(second.x_test.as_deref(), Some("1"));

    // The client already has its greeting, so the second one isn't passed on.
    send(&mut second.socket, CONNECTED).await;
    send(&mut second.socket, RUN_TEST_CASES).await;
    assert_eq!(read_line(&mut output).await, RUN_TEST_CASES);

    // The transport closes the connection it moved off.
    let closed = timeout(WAIT, async {
        loop {
            match first.socket.next().await {
                Some(Ok(Message::Close(_))) => return true,
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return false,
            }
        }
    })
    .await
    .expect("timed out waiting for the old connection to close");
    assert!(closed, "the old connection ended without a close frame");

    write_line(&mut input, "hello-2").await;
    assert_eq!(next_text(&mut second.socket).await.trim_end(), "hello-2");

    child.kill().await.unwrap();
}

#[tokio::test]
async fn exits_when_the_server_closes_the_connection() {
    let pipes = Pipes::new("close");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut child = spawn_transport(&listener, &pipes);
    let (mut output, _input) = pipes.open().await;

    let mut first = accept(&listener).await;
    send(&mut first.socket, CONNECTED).await;
    assert_eq!(read_line(&mut output).await, CONNECTED);

    first.socket.close(None).await.unwrap();

    let status = timeout(WAIT, child.wait())
        .await
        .expect("timed out waiting for the transport to exit")
        .unwrap();
    assert!(status.success(), "transport exited with {status}");
}
