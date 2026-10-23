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
const RESUMED: &str = r#"{"command":"transport_resumed"}"#;
const MOVING: &str = r#"{"type":"transport_moving"}"#;

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

// Waits for the connection's close frame, skipping any other messages.
async fn expect_close(socket: &mut WebSocketStream<TcpStream>) {
    let closed = timeout(WAIT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(_))) => return true,
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return false,
            }
        }
    })
    .await
    .expect("timed out waiting for a close frame");
    assert!(closed, "the connection ended without a close frame");
}

// Every text message until the connection ends.
async fn drain_texts(socket: &mut WebSocketStream<TcpStream>) -> Vec<String> {
    timeout(WAIT, async {
        let mut seen = Vec::new();
        while let Some(Ok(message)) = socket.next().await {
            if let Message::Text(text) = message {
                seen.push(text.trim_end().to_string());
            }
        }
        seen
    })
    .await
    .expect("timed out draining the connection")
}

// Nothing arrives on `socket` for a short while.
async fn expect_quiet(socket: &mut WebSocketStream<TcpStream>) {
    let early = timeout(Duration::from_millis(300), socket.next()).await;
    assert!(early.is_err(), "expected nothing, got {early:?}");
}

async fn expect_exit(child: &mut Child, code: i32) {
    let status = timeout(WAIT, child.wait())
        .await
        .expect("timed out waiting for the transport to exit")
        .unwrap();
    assert_eq!(status.code(), Some(code), "transport exited with {status}");
}

// A transport connected to `listener` whose client has its greeting.
async fn connected(
    name: &str,
    listener: &TcpListener,
) -> (Pipes, Child, BufReader<File>, File, Accepted) {
    let pipes = Pipes::new(name);
    let child = spawn_transport(listener, &pipes);
    let (mut output, input) = pipes.open().await;

    let mut first = accept(listener).await;
    send(&mut first.socket, CONNECTED).await;
    assert_eq!(read_line(&mut output).await, CONNECTED);

    (pipes, child, output, input, first)
}

// Asks the transport to move, and takes the new connection as a server would.
async fn resume_onto(listener: &TcpListener, old: &mut Accepted) -> Accepted {
    send(&mut old.socket, RESUME).await;
    assert_eq!(next_text(&mut old.socket).await, MOVING);
    let mut new = accept(listener).await;
    send(&mut new.socket, CONNECTED).await;
    send(&mut new.socket, RESUMED).await;
    new
}

#[tokio::test]
async fn moves_to_a_new_connection_when_asked() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pipes = Pipes::new("resume");
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

    let mut second = resume_onto(&listener, &mut first).await;
    assert!(second.uri.contains("reconnect=true"), "{}", second.uri);
    assert!(second.uri.contains("resume=true"), "{}", second.uri);
    assert!(
        second.uri.contains("transport_features=resume"),
        "{}",
        second.uri
    );
    assert_eq!(second.x_test.as_deref(), Some("1"));

    // Neither the second greeting nor the confirmation reaches the client.
    send(&mut second.socket, RUN_TEST_CASES).await;
    assert_eq!(read_line(&mut output).await, RUN_TEST_CASES);

    write_line(&mut input, "hello-2").await;
    assert_eq!(next_text(&mut second.socket).await.trim_end(), "hello-2");

    // The server closes the replaced connection; the transport carries on.
    first.socket.close(None).await.unwrap();
    write_line(&mut input, "hello-3").await;
    assert_eq!(next_text(&mut second.socket).await.trim_end(), "hello-3");

    child.kill().await.unwrap();
}

#[tokio::test]
async fn exits_when_the_server_closes_the_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_pipes, mut child, mut output, _input, mut first) = connected("close", &listener).await;

    // What the server sent before closing still reaches the client.
    send(&mut first.socket, r#"{"command":"close","exit_status":0}"#).await;
    first.socket.close(None).await.unwrap();

    assert_eq!(
        read_line(&mut output).await,
        r#"{"command":"close","exit_status":0}"#
    );
    expect_exit(&mut child, 0).await;
}

#[tokio::test]
async fn sends_every_line_before_acting_on_exit() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

    for trial in 0..50 {
        let (_pipes, mut child, _output, mut input, mut first) =
            connected(&format!("exit-{trial}"), &listener).await;

        // One write, so both lines are queued together.
        write_line(&mut input, "report\nexit").await;

        assert_eq!(
            next_text(&mut first.socket).await.trim_end(),
            "report",
            "trial {trial}"
        );
        expect_close(&mut first.socket).await;
        expect_exit(&mut child, 0).await;
    }
}

#[tokio::test]
async fn closes_the_connection_when_the_pipe_closes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_pipes, mut child, _output, input, mut first) = connected("eof", &listener).await;

    drop(input);

    expect_close(&mut first.socket).await;
    expect_exit(&mut child, 0).await;
}

#[tokio::test]
async fn delivers_a_large_line_promptly() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_pipes, mut child, _output, mut input, mut first) = connected("large", &listener).await;

    let line = "x".repeat(10 * 1024 * 1024);
    let started = std::time::Instant::now();
    let (_, received) = tokio::join!(write_line(&mut input, &line), next_text(&mut first.socket));

    assert_eq!(received.trim_end().len(), line.len());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );

    child.kill().await.unwrap();
}

#[tokio::test]
async fn keeps_sending_while_the_client_is_not_reading() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_pipes, mut child, _output, mut input, mut first) = connected("busy", &listener).await;

    // Far more than a pipe holds, and the client reads none of it.
    let backlog = format!(
        r#"{{"command":"bulk","data":"{}"}}"#,
        "y".repeat(2 * 1024 * 1024)
    );
    send(&mut first.socket, &backlog).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    write_line(&mut input, "result-1").await;
    assert_eq!(next_text(&mut first.socket).await.trim_end(), "result-1");

    child.kill().await.unwrap();
}

#[tokio::test]
async fn holds_pipe_messages_until_the_server_confirms_the_move() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_pipes, mut child, mut output, mut input, mut first) = connected("hold", &listener).await;

    send(&mut first.socket, RESUME).await;
    assert_eq!(next_text(&mut first.socket).await, MOVING);
    let mut second = accept(&listener).await;
    send(&mut second.socket, CONNECTED).await;

    // Greeted but not yet confirmed: the old connection is still the server's.
    write_line(&mut input, "held").await;
    send(&mut first.socket, RUN_TEST_CASES).await;
    assert_eq!(read_line(&mut output).await, RUN_TEST_CASES);
    expect_quiet(&mut first.socket).await;
    expect_quiet(&mut second.socket).await;

    send(&mut second.socket, RESUMED).await;
    assert_eq!(next_text(&mut second.socket).await.trim_end(), "held");

    write_line(&mut input, "after").await;
    assert_eq!(next_text(&mut second.socket).await.trim_end(), "after");
    expect_quiet(&mut first.socket).await;

    child.kill().await.unwrap();
}

#[tokio::test]
async fn exit_during_a_move_sends_what_is_held_and_stops() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_pipes, mut child, _output, mut input, mut first) =
        connected("exit-hold", &listener).await;

    send(&mut first.socket, RESUME).await;
    // The new connection never gets confirmed.
    let _second = accept(&listener).await;

    let started = std::time::Instant::now();
    write_line(&mut input, "terminating\nexit").await;

    assert_eq!(
        drain_texts(&mut first.socket).await,
        vec![MOVING, "terminating"]
    );
    expect_exit(&mut child, 0).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn exits_when_the_server_declines_the_new_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_pipes, mut child, mut output, mut input, mut first) =
        connected("declined", &listener).await;

    send(&mut first.socket, RESUME).await;
    let mut second = accept(&listener).await;
    write_line(&mut input, "held").await;
    // Let the transport read the line before the server declines.
    tokio::time::sleep(Duration::from_millis(200)).await;
    send(&mut second.socket, CONNECTED).await;
    send(&mut second.socket, r#"{"command":"close","exit_status":1}"#).await;

    // The client reconnects in full; what was held goes out first, and the
    // refusal itself isn't passed on.
    assert_eq!(drain_texts(&mut first.socket).await, vec![MOVING, "held"]);
    expect_exit(&mut child, 1).await;
    let mut rest = String::new();
    let _ = timeout(
        WAIT,
        tokio::io::AsyncReadExt::read_to_string(&mut output, &mut rest),
    )
    .await;
    assert_eq!(rest, "");
}

#[tokio::test]
async fn reads_every_retired_connection_until_it_ends() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_pipes, mut child, mut output, _input, mut first) = connected("retired", &listener).await;

    let mut second = resume_onto(&listener, &mut first).await;
    let mut third = resume_onto(&listener, &mut second).await;

    send(&mut first.socket, r#"{"command":"late","from":"first"}"#).await;
    assert_eq!(
        read_line(&mut output).await,
        r#"{"command":"late","from":"first"}"#
    );
    send(&mut second.socket, r#"{"command":"late","from":"second"}"#).await;
    assert_eq!(
        read_line(&mut output).await,
        r#"{"command":"late","from":"second"}"#
    );
    send(&mut third.socket, RUN_TEST_CASES).await;
    assert_eq!(read_line(&mut output).await, RUN_TEST_CASES);

    // A resume command on a replaced connection is ignored.
    send(&mut first.socket, RESUME).await;
    expect_quiet_listener(&listener).await;

    child.kill().await.unwrap();
}

async fn expect_quiet_listener(listener: &TcpListener) {
    let early = timeout(Duration::from_millis(300), listener.accept()).await;
    assert!(early.is_err(), "unexpected new connection");
}

#[tokio::test]
async fn moves_again_when_asked_before_the_first_move_is_confirmed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_pipes, mut child, _output, mut input, mut first) = connected("again", &listener).await;

    send(&mut first.socket, RESUME).await;
    assert_eq!(next_text(&mut first.socket).await, MOVING);
    let mut second = accept(&listener).await;
    send(&mut second.socket, CONNECTED).await;
    // The node the new connection landed on is itself being drained.
    send(&mut second.socket, RESUME).await;
    send(&mut second.socket, RESUMED).await;

    assert_eq!(next_text(&mut second.socket).await, MOVING);
    let mut third = accept(&listener).await;
    send(&mut third.socket, CONNECTED).await;
    send(&mut third.socket, RESUMED).await;

    write_line(&mut input, "result").await;
    assert_eq!(next_text(&mut third.socket).await.trim_end(), "result");

    child.kill().await.unwrap();
}

#[tokio::test]
async fn exits_with_failure_when_declined_after_the_old_connection_closed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_pipes, mut child, _output, mut input, mut first) =
        connected("declined-closed", &listener).await;

    send(&mut first.socket, RESUME).await;
    assert_eq!(next_text(&mut first.socket).await, MOVING);
    let mut second = accept(&listener).await;
    write_line(&mut input, "held").await;
    first.socket.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    send(&mut second.socket, r#"{"command":"close","exit_status":1}"#).await;
    expect_exit(&mut child, 1).await;
}

#[tokio::test]
async fn a_declined_move_delivers_everything_for_the_client_before_exiting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_pipes, mut child, mut output, _input, mut first) =
        connected("declined-output", &listener).await;

    // More than a pipe holds, sent while the client is busy and not reading.
    let big = format!(
        r#"{{"command":"bulk","data":"{}"}}"#,
        "z".repeat(300 * 1024)
    );
    send(&mut first.socket, &big).await;
    send(&mut first.socket, RUN_TEST_CASES).await;
    send(&mut first.socket, RESUME).await;
    let mut second = accept(&listener).await;
    send(&mut second.socket, r#"{"command":"close","exit_status":1}"#).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(read_line(&mut output).await.len(), big.len());
    assert_eq!(read_line(&mut output).await, RUN_TEST_CASES);
    expect_exit(&mut child, 1).await;
}

#[tokio::test]
async fn exit_with_no_open_connection_waits_for_the_move() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_pipes, mut child, _output, mut input, mut first) =
        connected("exit-orphaned", &listener).await;

    send(&mut first.socket, RESUME).await;
    assert_eq!(next_text(&mut first.socket).await, MOVING);
    let mut second = accept(&listener).await;
    first.socket.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    write_line(&mut input, "final\nexit").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    send(&mut second.socket, CONNECTED).await;
    send(&mut second.socket, RESUMED).await;

    assert_eq!(drain_texts(&mut second.socket).await, vec!["final"]);
    expect_exit(&mut child, 0).await;
}

#[tokio::test]
async fn answers_the_server_closing_a_replaced_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_pipes, mut child, _output, _input, mut first) =
        connected("retire-close", &listener).await;

    let _second = resume_onto(&listener, &mut first).await;
    first.socket.close(None).await.unwrap();

    let reply = timeout(Duration::from_secs(2), first.socket.next())
        .await
        .expect("the transport never answered the close");
    assert!(
        matches!(reply, Some(Ok(Message::Close(_))) | None),
        "got {reply:?}"
    );

    child.kill().await.unwrap();
}
