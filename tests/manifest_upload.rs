use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

const WAIT: Duration = Duration::from_secs(15);
const CONNECTED: &str = r#"{"command":"connected"}"#;
const RESUME: &str = r#"{"command":"transport_reconnect"}"#;
const RESUMED: &str = r#"{"command":"transport_resumed"}"#;

type Reply = (u16, String, u64);
type Script = Arc<dyn Fn(&Seen, usize) -> Reply + Send + Sync>;
type Log = Arc<Mutex<Vec<Seen>>>;

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    token: String,
    body: Vec<u8>,
}

struct Socket {
    uri: String,
    sink: SplitSink<WebSocketStream<TcpStream>, Message>,
    texts: mpsc::UnboundedReceiver<String>,
    pings: Arc<AtomicUsize>,
}

struct Server {
    addr: SocketAddr,
    sockets: mpsc::UnboundedReceiver<Socket>,
    http: Log,
}

struct Run {
    child: Child,
    output: BufReader<ChildStdout>,
    input: ChildStdin,
    server: Server,
    socket: Socket,
}

fn ok() -> Script {
    Arc::new(|_, _| (200, String::new(), 0))
}

fn status(code: u16) -> Script {
    Arc::new(move |_, _| (code, String::new(), 0))
}

fn delayed(code: u16, millis: u64) -> Script {
    Arc::new(move |_, _| (code, String::new(), millis))
}

fn manifest_command(token: &str, max_chunk_bytes: usize) -> String {
    format!(
        r#"{{"command":"test_manifest","upload":{{"url":"/manifest_uploads","token":"{token}","max_chunk_bytes":{max_chunk_bytes},"max_total_bytes":64000000,"max_inflated_bytes":64000000}}}}"#
    )
}

fn without_upload(command: &str) -> String {
    command.split(r#","upload":"#).next().unwrap().to_owned() + "}"
}

fn manifest_line(marker: &str) -> String {
    let cases: Vec<String> = (0..60)
        .map(|index| format!(r#"{{"id":"{marker}-{index}","file_path":"./spec/a_spec.rb"}}"#))
        .collect();
    format!(
        r#"{{"data":{{"test_cases":[{}]}},"type":"test_manifest"}}"#,
        cases.join(",")
    )
}

async fn read_http(stream: &mut TcpStream) -> Option<Seen> {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 65536];
    let header_end = loop {
        if let Some(at) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            break at;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let path = lines.next()?.split(' ').nth(1)?.to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
        .collect();
    let length: usize = headers.get("content-length")?.parse().ok()?;
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Seen {
        path,
        token: headers
            .get("x-selective-upload-token")
            .cloned()
            .unwrap_or_default(),
        body,
    })
}

async fn serve_http(mut stream: TcpStream, script: Script, log: Log) {
    let Some(seen) = read_http(&mut stream).await else {
        return;
    };
    let index = {
        let mut log = log.lock().unwrap();
        log.push(seen.clone());
        log.len() - 1
    };
    let (code, body, delay) = script(&seen, index);
    tokio::time::sleep(Duration::from_millis(delay)).await;
    let body = if code == 200 && body.is_empty() {
        r#"{"status":"applied"}"#.to_owned()
    } else {
        body
    };
    let response = format!(
        "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

#[allow(clippy::result_large_err)]
async fn serve_socket(stream: TcpStream, sockets: mpsc::UnboundedSender<Socket>) {
    let mut uri = String::new();
    let handshake =
        tokio_tungstenite::accept_hdr_async(stream, |request: &Request, response: Response| {
            uri = request.uri().to_string();
            Ok(response)
        })
        .await;
    let Ok(ws) = handshake else {
        return;
    };
    let (sink, mut stream) = ws.split();
    let (texts_tx, texts) = mpsc::unbounded_channel();
    let pings = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&pings);
    tokio::spawn(async move {
        while let Some(Ok(message)) = stream.next().await {
            match message {
                Message::Text(text) => {
                    let _ = texts_tx.send(text);
                }
                Message::Ping(_) => {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });
    let _ = sockets.send(Socket {
        uri,
        sink,
        texts,
        pings,
    });
}

impl Server {
    async fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (sockets_tx, sockets) = mpsc::unbounded_channel();
        let http: Log = Arc::default();
        let log = Arc::clone(&http);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut peeked = [0u8; 4];
                let n = stream.peek(&mut peeked).await.unwrap_or(0);
                if n == 4 && &peeked == b"POST" {
                    tokio::spawn(serve_http(stream, Arc::clone(&script), Arc::clone(&log)));
                } else {
                    tokio::spawn(serve_socket(stream, sockets_tx.clone()));
                }
            }
        });
        Self {
            addr,
            sockets,
            http,
        }
    }

    async fn next_socket(&mut self) -> Socket {
        timeout(WAIT, self.sockets.recv())
            .await
            .expect("timed out waiting for the transport to connect")
            .unwrap()
    }

    fn requests(&self) -> Vec<Seen> {
        self.http.lock().unwrap().clone()
    }
}

impl Socket {
    async fn send(&mut self, text: &str) {
        self.sink.send(Message::text(text)).await.unwrap();
    }

    async fn next_text(&mut self) -> String {
        timeout(WAIT, self.texts.recv())
            .await
            .expect("timed out waiting for a text frame from the transport")
            .unwrap()
            .trim_end()
            .to_owned()
    }

    async fn expect_quiet(&mut self) {
        let early = timeout(Duration::from_millis(300), self.texts.recv()).await;
        assert!(early.is_err(), "expected nothing, got {early:?}");
    }
}

impl Run {
    async fn start(script: Script, env: &[(&str, &str)]) -> Self {
        Self::start_with_query(script, env, "api_key=suite-key&run_id=1").await
    }

    async fn start_with_query(script: Script, env: &[(&str, &str)], query: &str) -> Self {
        let mut server = Server::start(script).await;
        let binary = std::env::var("TRANSPORT_UNDER_TEST")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_transport").to_owned());
        let mut command = Command::new(binary);
        command
            .arg("--stdio")
            .env(
                "SELECTIVE_TRANSPORT_URL",
                format!("ws://{}/transport/websocket?{query}", server.addr),
            )
            .env_remove("SELECTIVE_WS_PING_INTERVAL_MS")
            .env_remove("SELECTIVE_WS_PONG_TIMEOUT_MS")
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let input = child.stdin.take().unwrap();
        let mut socket = server.next_socket().await;
        socket.send(CONNECTED).await;
        let mut run = Self {
            child,
            output,
            input,
            server,
            socket,
        };
        assert_eq!(run.read_line().await, CONNECTED);
        run
    }

    async fn read_line(&mut self) -> String {
        let mut line = String::new();
        timeout(WAIT, self.output.read_line(&mut line))
            .await
            .expect("timed out waiting for the transport to write to the pipe")
            .unwrap();
        line.trim_end().to_owned()
    }

    async fn write_line(&mut self, line: &str) {
        self.input
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        self.input.flush().await.unwrap();
    }

    async fn ask(&mut self, command: &str) -> String {
        self.socket.send(command).await;
        self.read_line().await
    }

    async fn wait_requests(&self, count: usize) -> Vec<Seen> {
        timeout(WAIT, async {
            loop {
                let seen = self.server.requests();
                if seen.len() >= count {
                    return seen;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("timed out waiting for upload requests")
    }

    async fn wait_commit(&self) -> Vec<Seen> {
        timeout(WAIT, async {
            loop {
                let seen = self.server.requests();
                if seen
                    .last()
                    .is_some_and(|last| last.path.ends_with("/commit"))
                {
                    return seen;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("timed out waiting for the commit request")
    }

    async fn expect_exit(&mut self, code: i32) {
        let status = timeout(WAIT, self.child.wait())
            .await
            .expect("timed out waiting for the transport to exit")
            .unwrap();
        assert_eq!(status.code(), Some(code), "transport exited with {status}");
    }
}

fn reassemble(requests: &[Seen]) -> String {
    let mut chunks: Vec<(usize, &Seen)> = requests
        .iter()
        .filter_map(|seen| {
            let index = seen.path.strip_prefix("/manifest_uploads/chunks/")?;
            Some((index.parse().ok()?, seen))
        })
        .collect();
    chunks.sort_by_key(|(index, _)| *index);
    let joined: Vec<u8> = chunks
        .iter()
        .flat_map(|(_, seen)| seen.body.clone())
        .collect();
    let mut text = String::new();
    flate2::read::GzDecoder::new(joined.as_slice())
        .read_to_string(&mut text)
        .unwrap();
    text
}

#[tokio::test]
async fn the_client_sees_no_upload_block_and_the_manifest_goes_over_http() {
    let mut run = Run::start(ok(), &[]).await;
    assert!(
        run.socket.uri.contains("manifest_http"),
        "{}",
        run.socket.uri
    );

    let command = manifest_command("t1", 60);
    assert_eq!(run.ask(&command).await, without_upload(&command));

    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    run.write_line("after").await;
    assert_eq!(run.socket.next_text().await, "after");

    let requests = run.wait_commit().await;
    assert!(requests.iter().all(|seen| seen.token == "t1"));
    assert!(requests.len() > 3, "{} requests", requests.len());
    assert_eq!(requests.last().unwrap().path, "/manifest_uploads/commit");
    assert_eq!(reassemble(&requests), manifest);
}

#[tokio::test]
async fn commands_without_an_upload_block_are_forwarded_byte_for_byte() {
    let mut run = Run::start(ok(), &[]).await;
    for command in [
        "{\"command\" :  \"test_manifest\" ,\"x\":1e3, \"s\":\"\\u00e9\"}",
        r#"{"command":"print_message","message":"upload"}"#,
        r#"{"command":"run_test_cases","upload":{"url":"/x"}}"#,
    ] {
        assert_eq!(run.ask(command).await, command);
    }
    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    assert_eq!(run.socket.next_text().await, manifest);
    assert!(run.server.requests().is_empty());
}

#[tokio::test]
async fn a_413_falls_back_to_the_websocket_while_later_lines_go_straight_out() {
    let mut run = Run::start(status(413), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    run.write_line("after").await;
    assert_eq!(run.socket.next_text().await, "after");
    assert_eq!(run.socket.next_text().await, manifest);
    assert_eq!(run.server.requests().len(), 1);
}

#[tokio::test]
async fn a_410_stops_that_upload_and_a_later_ask_uploads_again() {
    let by_token: Script = Arc::new(|seen, _| {
        let code = if seen.token == "old" { 410 } else { 200 };
        (code, String::new(), 0)
    });
    let mut run = Run::start(by_token, &[]).await;
    run.ask(&manifest_command("old", 1_000_000)).await;
    run.write_line(&manifest_line("first")).await;
    run.write_line("after").await;
    assert_eq!(run.socket.next_text().await, "after");
    run.wait_requests(1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(run.server.requests().len(), 1);

    run.ask(&manifest_command("fresh", 1_000_000)).await;
    let second = manifest_line("second");
    run.write_line(&second).await;
    run.write_line("after-2").await;
    assert_eq!(run.socket.next_text().await, "after-2");
    let requests = run.wait_requests(3).await;
    assert_eq!(requests.len(), 3);
    assert!(requests[1..].iter().all(|seen| seen.token == "fresh"));
    assert_eq!(reassemble(&requests[1..]), second);
}

#[tokio::test]
async fn a_manifest_over_the_inflated_limit_skips_http() {
    let mut run = Run::start(ok(), &[]).await;
    let command = manifest_command("t1", 1_000_000).replace("64000000}}", "100}}");
    run.ask(&command).await;
    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    assert_eq!(run.socket.next_text().await, manifest);
    assert!(run.server.requests().is_empty());
}

#[tokio::test]
async fn pings_keep_the_websocket_alive_while_an_upload_is_slow() {
    let mut run = Run::start(
        delayed(200, 1500),
        &[("SELECTIVE_UPLOAD_PING_INTERVAL_MS", "100")],
    )
    .await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    assert_eq!(run.socket.pings.load(Ordering::SeqCst), 0);
    run.write_line(&manifest_line("m")).await;
    run.wait_requests(2).await;
    let during = run.socket.pings.load(Ordering::SeqCst);
    assert!(during >= 3, "{during} pings during a 1.5 s stall");
}

#[tokio::test]
async fn no_pings_are_sent_when_no_upload_is_running() {
    let mut run = Run::start(ok(), &[("SELECTIVE_UPLOAD_PING_INTERVAL_MS", "50")]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(run.socket.pings.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_exit_written_mid_upload_aborts_it_and_sends_the_manifest_over_the_websocket() {
    let mut run = Run::start(delayed(413, 2000), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    run.wait_requests(1).await;
    run.write_line("after").await;
    let started = std::time::Instant::now();
    run.write_line("exit").await;
    assert_eq!(run.socket.next_text().await, "after");
    assert_eq!(run.socket.next_text().await, manifest);
    run.expect_exit(0).await;
    assert!(started.elapsed() < Duration::from_millis(1500));
}

#[tokio::test]
async fn a_new_ask_supersedes_the_running_upload_and_the_next_manifest_uploads_under_its_token() {
    let mut run = Run::start(delayed(200, 500), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    let first = manifest_line("first");
    run.write_line(&first).await;
    run.wait_requests(1).await;
    run.ask(&manifest_command("t2", 1_000_000)).await;
    let second = manifest_line("second");
    run.write_line(&second).await;
    run.wait_requests(3).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    let requests = run.server.requests();
    let tokens: Vec<&str> = requests.iter().map(|seen| seen.token.as_str()).collect();
    assert_eq!(tokens, ["t1", "t2", "t2"]);
    assert_eq!(reassemble(&requests[1..]), second);
    run.socket.expect_quiet().await;
}

#[tokio::test]
async fn a_resume_during_the_upload_puts_the_fallback_on_the_new_connection() {
    let mut run = Run::start(delayed(413, 800), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    run.wait_requests(1).await;
    run.write_line("after").await;
    assert_eq!(run.socket.next_text().await, "after");
    run.socket.send(RESUME).await;
    let mut new = run.server.next_socket().await;
    new.send(CONNECTED).await;
    new.send(RESUMED).await;
    assert_eq!(new.next_text().await, manifest);
    assert!(new.uri.contains("resume=true"), "{}", new.uri);
}

#[tokio::test]
async fn a_later_ask_without_a_block_goes_over_the_websocket() {
    let mut run = Run::start(ok(), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    run.ask(r#"{"command":"test_manifest"}"#).await;
    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    assert_eq!(run.socket.next_text().await, manifest);
    assert!(run.server.requests().is_empty());
}

#[tokio::test]
async fn a_newer_ask_drops_the_running_upload_instead_of_falling_back() {
    let mut run = Run::start(delayed(413, 600), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    run.wait_requests(1).await;
    run.ask(&manifest_command("t2", 1_000_000)).await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    run.socket.expect_quiet().await;
    let requests = run.server.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].token, "t1");
}

#[tokio::test]
async fn an_ask_without_a_block_leaves_the_running_upload_alone() {
    let mut run = Run::start(delayed(200, 300), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    run.wait_requests(1).await;
    run.ask(r#"{"command":"test_manifest"}"#).await;
    let requests = run.wait_commit().await;
    assert!(requests.iter().all(|seen| seen.token == "t1"));
    assert_eq!(reassemble(&requests), manifest);
}

#[tokio::test]
async fn a_second_ask_mid_upload_keeps_the_manifest_and_later_lines_when_the_websocket_dies() {
    let mut run = Run::start(slow_first_chunk(1500), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    run.write_line(&manifest_line("first")).await;
    run.wait_requests(1).await;
    run.ask(&manifest_command("t2", 1_000_000)).await;
    let second = manifest_line("second");
    run.write_line(&second).await;
    run.write_line("r9").await;
    assert_eq!(run.socket.next_text().await, "r9");
    run.socket.sink.send(Message::Close(None)).await.unwrap();
    let requests = run.wait_commit().await;
    let tokens: Vec<&str> = requests.iter().map(|seen| seen.token.as_str()).collect();
    assert_eq!(tokens, ["t1", "t2", "t2"]);
    assert_eq!(reassemble(&requests[1..]), second);
    run.expect_exit(0).await;
}

#[tokio::test]
async fn a_fallback_that_settles_mid_resume_goes_out_once_the_move_completes() {
    let mut run = Run::start(delayed(413, 300), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    run.wait_requests(1).await;
    run.write_line("after").await;
    assert_eq!(run.socket.next_text().await, "after");
    run.socket.send(RESUME).await;
    let mut new = run.server.next_socket().await;
    new.send(CONNECTED).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    new.send(RESUMED).await;
    assert_eq!(new.next_text().await, manifest);
}

#[tokio::test]
async fn a_command_written_while_the_transport_winds_down_has_no_upload_block() {
    let mut run = Run::start(ok(), &[]).await;
    run.socket.send(RESUME).await;
    let mut new = run.server.next_socket().await;
    new.send(CONNECTED).await;
    new.send(RESUMED).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let command = manifest_command("t1", 1_000_000);
    new.sink.send(Message::Close(None)).await.unwrap();
    run.socket.send(&command).await;
    assert_eq!(run.read_line().await, without_upload(&command));
    let _ = run.socket.sink.send(Message::Close(None)).await;
    run.expect_exit(0).await;
}

#[tokio::test]
async fn an_exit_during_a_resume_sends_a_held_manifest_on_the_old_socket() {
    let mut run = Run::start(ok(), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    run.socket.send(RESUME).await;
    assert_eq!(
        run.socket.next_text().await,
        r#"{"type":"transport_moving"}"#
    );
    let _new = run.server.next_socket().await;
    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    run.write_line("exit").await;
    assert_eq!(run.socket.next_text().await, manifest);
    run.expect_exit(0).await;
    assert!(run.server.requests().is_empty());
}

#[tokio::test]
async fn a_declined_move_sends_a_held_manifest_on_the_old_socket() {
    let mut run = Run::start(ok(), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    run.socket.send(RESUME).await;
    assert_eq!(
        run.socket.next_text().await,
        r#"{"type":"transport_moving"}"#
    );
    let mut new = run.server.next_socket().await;
    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    new.send(r#"{"command":"close","exit_status":1}"#).await;
    assert_eq!(run.socket.next_text().await, manifest);
    run.expect_exit(1).await;
    assert!(run.server.requests().is_empty());
}

#[tokio::test]
async fn a_stalled_upload_falls_back_at_its_deadline_and_releases_the_held_lines() {
    let mut run = Run::start(
        delayed(200, 30_000),
        &[("SELECTIVE_UPLOAD_DEADLINE_MS", "700")],
    )
    .await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    let manifest = manifest_line("m");
    let started = std::time::Instant::now();
    run.write_line(&manifest).await;
    run.write_line("after").await;
    assert_eq!(run.socket.next_text().await, "after");
    assert_eq!(run.socket.next_text().await, manifest);
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(600), "{waited:?}");
    assert!(waited < Duration::from_secs(4), "{waited:?}");
    assert_eq!(run.server.requests().len(), 1);
}

#[tokio::test]
async fn manifest_http_is_advertised_only_when_there_is_an_api_key_to_upload_with() {
    let with_key = Run::start(ok(), &[]).await;
    assert!(
        with_key.socket.uri.contains("manifest_http"),
        "{}",
        with_key.socket.uri
    );
    let without_key = Run::start_with_query(ok(), &[], "run_id=1").await;
    assert!(without_key.socket.uri.contains("transport_features=resume"));
    assert!(
        !without_key.socket.uri.contains("manifest_http"),
        "{}",
        without_key.socket.uri
    );
}

#[tokio::test]
async fn a_fallback_and_held_lines_survive_a_resume_whose_old_socket_closed_first() {
    let mut run = Run::start(delayed(413, 700), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    run.wait_requests(1).await;
    run.socket.send(RESUME).await;
    let mut new = run.server.next_socket().await;
    new.send(CONNECTED).await;
    run.socket.sink.send(Message::Close(None)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    run.write_line("held-one").await;
    run.write_line("held-two").await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    new.send(RESUMED).await;
    assert_eq!(new.next_text().await, manifest);
    assert_eq!(new.next_text().await, "held-one");
    assert_eq!(new.next_text().await, "held-two");
}

#[tokio::test]
async fn an_ask_that_arrives_on_a_retired_socket_still_lets_the_reply_upload() {
    let mut run = Run::start(ok(), &[]).await;
    run.socket.send(RESUME).await;
    let mut new = run.server.next_socket().await;
    new.send(CONNECTED).await;
    new.send(RESUMED).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let command = manifest_command("late", 1_000_000);
    run.socket.send(&command).await;
    assert_eq!(run.read_line().await, without_upload(&command));
    run.write_line(&manifest_line("m")).await;
    let seen = run.wait_requests(2).await;
    assert!(seen.iter().all(|request| request.token == "late"));
}

#[tokio::test]
async fn an_unanswered_ask_leaves_its_block_for_a_late_reply() {
    let mut run = Run::start(ok(), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    let notice = r#"{"command":"print_notice","message":"hi"}"#;
    assert_eq!(run.ask(notice).await, notice);
    run.write_line(&manifest_line("m")).await;
    let seen = run.wait_requests(2).await;
    assert!(seen.iter().all(|request| request.token == "t1"));
}

#[tokio::test]
async fn a_reply_ending_in_a_carriage_return_still_uploads() {
    let mut run = Run::start(ok(), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    let manifest = manifest_line("m");
    run.write_line(&format!("{manifest}\r")).await;
    let seen = run.wait_requests(2).await;
    assert_eq!(reassemble(&seen), manifest);
}

#[tokio::test]
async fn an_exit_with_an_upload_in_flight_and_no_connection_sends_the_manifest_on_the_new_one() {
    let mut run = Run::start(delayed(200, 30_000), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    run.wait_requests(1).await;
    run.socket.send(RESUME).await;
    let mut new = run.server.next_socket().await;
    new.send(CONNECTED).await;
    run.socket.sink.send(Message::Close(None)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    run.write_line("exit").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    new.send(RESUMED).await;
    assert_eq!(new.next_text().await, manifest);
    run.expect_exit(0).await;
}

async fn collect_texts(socket: &mut Socket, quiet: Duration) -> Vec<String> {
    let mut texts = Vec::new();
    while let Ok(Some(text)) = timeout(quiet, socket.texts.recv()).await {
        texts.push(text.trim_end().to_owned());
    }
    texts
}

fn slow_first_chunk(millis: u64) -> Script {
    Arc::new(move |_, index| (200, String::new(), if index == 0 { millis } else { 0 }))
}

#[tokio::test]
async fn results_written_behind_an_upload_reach_the_socket_before_a_declined_move() {
    let mut run = Run::start(slow_first_chunk(2500), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    run.write_line(&manifest_line("m")).await;
    run.write_line("r0").await;
    run.write_line("r1").await;
    run.wait_requests(1).await;
    run.socket.send(RESUME).await;
    let mut new = run.server.next_socket().await;
    run.socket.sink.send(Message::Close(None)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    new.send(r#"{"command":"close","exit_status":1}"#).await;
    let declined = std::time::Instant::now();
    let seen = collect_texts(&mut run.socket, Duration::from_millis(300)).await;
    assert_eq!(seen, ["r0", "r1", r#"{"type":"transport_moving"}"#]);
    run.expect_exit(1).await;
    assert!(declined.elapsed() < Duration::from_secs(6));
    assert!(run
        .server
        .requests()
        .last()
        .unwrap()
        .path
        .ends_with("/commit"));
}

#[tokio::test]
async fn a_websocket_that_dies_mid_upload_does_not_stop_the_upload_or_lose_the_results() {
    let mut run = Run::start(slow_first_chunk(1000), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    run.write_line(&manifest_line("m")).await;
    run.write_line("r0").await;
    run.write_line("r1").await;
    run.wait_requests(1).await;
    let killed = std::time::Instant::now();
    run.socket.sink.send(Message::Close(None)).await.unwrap();
    let seen = collect_texts(&mut run.socket, Duration::from_millis(300)).await;
    assert_eq!(seen, ["r0", "r1"]);
    let requests = run.wait_commit().await;
    assert!(requests[1].path.ends_with("/commit"));
    run.expect_exit(0).await;
    assert!(killed.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn stdin_closing_while_the_resume_gap_holds_a_manifest_exits_with_it_on_the_new_socket() {
    let mut run = Run::start(ok(), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    run.socket.send(RESUME).await;
    let mut new = run.server.next_socket().await;
    new.send(CONNECTED).await;
    run.socket.sink.send(Message::Close(None)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let manifest = manifest_line("m");
    run.write_line(&manifest).await;
    run.write_line("exit").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let released = std::time::Instant::now();
    new.send(RESUMED).await;
    assert_eq!(new.next_text().await, manifest);
    run.expect_exit(0).await;
    assert!(released.elapsed() < Duration::from_secs(5));
    assert!(run.server.requests().is_empty());
}

#[tokio::test]
async fn a_fallback_with_no_websocket_left_cannot_be_delivered_and_the_transport_exits() {
    let mut run = Run::start(delayed(403, 500), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    run.write_line(&manifest_line("m")).await;
    run.wait_requests(1).await;
    run.socket.sink.send(Message::Close(None)).await.unwrap();
    assert!(collect_texts(&mut run.socket, Duration::from_secs(1))
        .await
        .is_empty());
    run.expect_exit(0).await;
    assert_eq!(run.server.requests().len(), 1);
}

#[tokio::test]
async fn an_upload_that_outlives_a_dead_websocket_is_cut_off_after_the_grace_period() {
    let mut run = Run::start(delayed(200, 60_000), &[]).await;
    run.ask(&manifest_command("t1", 1_000_000)).await;
    run.write_line(&manifest_line("m")).await;
    run.wait_requests(1).await;
    let killed = std::time::Instant::now();
    run.socket.sink.send(Message::Close(None)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert!(run.child.try_wait().unwrap().is_none());
    run.expect_exit(0).await;
    let lived = killed.elapsed();
    assert!(lived >= Duration::from_secs(9), "{lived:?}");
    assert!(lived < Duration::from_secs(13), "{lived:?}");
}
