#![warn(clippy::perf, clippy::style, clippy::pedantic)]

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::process::exit;
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, Request};
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream,
};
use url::Url;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Sink = SplitSink<Socket, Message>;
type ServerEvent = (u64, Option<Message>);
type Headers = Vec<(String, String)>;

// A new connection the server has confirmed, and the headers of any further
// move it asked for before confirming.
struct Resumed {
    socket: Socket,
    again: Option<Headers>,
}

// Tells the server this transport can move its own connection when asked to.
const FEATURES_PARAM: (&str, &str) = ("transport_features", "resume");

// Sent by a server that wants this runner on a new connection. The client only
// reads the pipe between tests, so the transport moves the connection itself.
const RESUME_COMMAND: &str = "transport_reconnect";
// Sent on the new connection once the server has moved the runner onto it.
const RESUMED_COMMAND: &str = "transport_resumed";
// Sent on the old connection when the transport starts holding: nothing more
// will follow it there.
const MOVING_MESSAGE: &str = r#"{"type":"transport_moving"}"#;

const RESUME_DEADLINE: Duration = Duration::from_mins(1);
const RESUME_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
const RESUMED_TIMEOUT: Duration = Duration::from_secs(15);
const RESUME_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const RESUME_MAX_BACKOFF: Duration = Duration::from_secs(4);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
// The server closes a replaced connection once it has drained it; this is the backstop.
const RETIRED_TIMEOUT: Duration = Duration::from_mins(1);

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <url> <runner_id>", args[0]);
        exit(1);
    }

    let url = with_params(&Url::parse(&args[1]).unwrap(), &[FEATURES_PARAM]);
    let runner_id: String = args[2].clone();

    let input_pipe_path = format!("/tmp/{runner_id}_1");
    let output_pipe_path = format!("/tmp/{runner_id}_2");

    let (pipe_tx, mut pipe_rx) = mpsc::unbounded_channel::<PipeItem>();
    tokio::spawn(read_pipe(input_pipe_path, pipe_tx));

    let out = OpenOptions::new()
        .write(true)
        .open(output_pipe_path)
        .await
        .unwrap();
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(write_pipe(out, out_rx));

    let Ok((socket, _)) = connect_async(url.as_str()).await else {
        eprintln!("Failed to connect to the WebSocket server. Exiting...");
        exit(1);
    };

    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<ServerEvent>();
    let (resume_tx, mut resume_rx) = mpsc::unbounded_channel::<Option<Resumed>>();

    let mut transport = Transport {
        out: Some(out_tx),
        writer: Some(writer),
        resume_url: with_params(&url, &[("reconnect", "true"), ("resume", "true")]),
        current: None,
        retired: HashMap::new(),
        resuming: false,
        exit_requested: false,
        held: VecDeque::new(),
        next_id: 0,
        events_tx,
        resume_tx,
    };
    transport.adopt(socket);

    loop {
        tokio::select! {
            Some((id, message)) = events_rx.recv() => transport.on_server_event(id, message).await,
            Some(resumed) = resume_rx.recv() => transport.on_resume_finished(resumed).await,
            item = pipe_rx.recv() => {
                // A confirmed move that's already waiting decides which
                // connection an exit closes.
                if matches!(item, Some(PipeItem::Exit) | None) {
                    if let Ok(resumed) = resume_rx.try_recv() {
                        transport.on_resume_finished(resumed).await;
                    }
                }
                transport.on_pipe_item(item).await;
            }
        }
    }
}

// Lines and the exit request share one channel so an exit is only acted on
// after every line written before it has been sent.
enum PipeItem {
    Line(Message),
    Exit,
}

struct Transport {
    // Lines for the client, written by their own task so a client that isn't
    // reading never holds up traffic to the server.
    out: Option<mpsc::UnboundedSender<Vec<u8>>>,
    writer: Option<JoinHandle<()>>,
    resume_url: Url,
    // The connection messages from the pipe are sent on.
    current: Option<(u64, Sink)>,
    // Connections being replaced. Each is read until it ends, since the
    // server may have sent on it before switching over.
    retired: HashMap<u64, Sink>,
    // While a resume is in flight, pipe messages wait for the new connection.
    resuming: bool,
    // The client asked to stop while a resume had no connection to send on.
    exit_requested: bool,
    held: VecDeque<Message>,
    next_id: u64,
    events_tx: mpsc::UnboundedSender<ServerEvent>,
    resume_tx: mpsc::UnboundedSender<Option<Resumed>>,
}

impl Transport {
    fn current_id(&self) -> Option<u64> {
        self.current.as_ref().map(|(id, _)| *id)
    }

    // Makes `socket` the current connection and returns the one it replaced.
    fn adopt(&mut self, socket: Socket) -> Option<(u64, Sink)> {
        let id = self.next_id;
        self.next_id += 1;

        let (sink, stream) = socket.split();
        spawn_reader(id, stream, self.events_tx.clone());
        self.current.replace((id, sink))
    }

    async fn on_server_event(&mut self, id: u64, message: Option<Message>) {
        let live = Some(id) == self.current_id() || self.retired.contains_key(&id);

        match message {
            Some(Message::Text(text)) if live => {
                if let Some(headers) = resume_headers(&text) {
                    if Some(id) == self.current_id() {
                        self.start_resume(headers).await;
                    }
                    return;
                }

                self.write_line(text.into_bytes());
            }
            Some(Message::Binary(data)) if live => self.write_line(data),
            Some(Message::Close(_)) | None => self.on_connection_ended(id).await,
            _ => {}
        }
    }

    async fn on_connection_ended(&mut self, id: u64) {
        if let Some(sink) = self.retired.remove(&id) {
            close_in_background(sink);
            return;
        }

        if Some(id) == self.current_id() {
            if !self.resuming {
                self.finish_output().await;
                exit(0);
            }

            // Asked to move and the old connection went first: wait for the
            // new one rather than exiting.
            if let Some((_, sink)) = self.current.take() {
                close_in_background(sink);
            }
        }
    }

    async fn start_resume(&mut self, headers: Headers) {
        if self.resuming {
            return;
        }

        self.resuming = true;
        if let Some((_, sink)) = self.current.as_mut() {
            let _ = sink.send(Message::text(MOVING_MESSAGE)).await;
        }
        let url = self.resume_url.clone();
        let resume_tx = self.resume_tx.clone();
        tokio::spawn(async move {
            let _ = resume_tx.send(connect_with_retry(&url, &headers).await);
        });
    }

    async fn on_resume_finished(&mut self, resumed: Option<Resumed>) {
        if !self.resuming {
            return;
        }
        self.resuming = false;

        // The server may already have moved the runner, so carrying on with the
        // old connection could lose results. The client reconnects in full.
        let Some(Resumed { socket, again }) = resumed else {
            eprintln!("Failed to move to a new connection. Exiting...");
            self.send_held().await;
            self.close_current().await;
            if !self.exit_requested {
                self.finish_output().await;
            }
            exit(1);
        };

        if let Some((old_id, old_sink)) = self.adopt(socket) {
            self.retired.insert(old_id, old_sink);
            let events_tx = self.events_tx.clone();
            tokio::spawn(async move {
                sleep(RETIRED_TIMEOUT).await;
                let _ = events_tx.send((old_id, None));
            });
        }

        self.send_held().await;

        if self.exit_requested {
            self.close_current().await;
            exit(0);
        }

        if let Some(headers) = again {
            self.start_resume(headers).await;
        }
    }

    async fn on_pipe_item(&mut self, item: Option<PipeItem>) {
        match item {
            Some(PipeItem::Line(message)) if self.resuming => self.held.push_back(message),
            Some(PipeItem::Line(message)) => self.send(message).await,
            // The client stops the transport a few seconds after asking, so an
            // exit doesn't wait for a resume: what's held goes out on the
            // connection still open.
            Some(PipeItem::Exit) | None if self.resuming && self.current.is_none() => {
                self.exit_requested = true;
            }
            Some(PipeItem::Exit) | None => {
                self.send_held().await;
                self.close_current().await;
                exit(0);
            }
        }
    }

    // Best effort: a connection that has gone is reported by its reader.
    async fn send_held(&mut self) {
        while let Some(message) = self.held.pop_front() {
            let Some((_, sink)) = self.current.as_mut() else {
                return;
            };
            if sink.send(message).await.is_err() {
                return;
            }
        }
    }

    async fn send(&mut self, message: Message) {
        let Some((_, sink)) = self.current.as_mut() else {
            return;
        };

        if sink.send(message).await.is_err() && !self.resuming {
            self.finish_output().await;
            exit(0);
        }
    }

    async fn close_current(&mut self) {
        if let Some((_, sink)) = self.current.as_mut() {
            let _ = timeout(CLOSE_TIMEOUT, sink.close()).await;
        }
    }

    fn write_line(&mut self, mut data: Vec<u8>) {
        data.push(b'\n');
        if let Some(out) = &self.out {
            let _ = out.send(data);
        }
    }

    // Waits until every line for the client has been written.
    async fn finish_output(&mut self) {
        self.out = None;
        if let Some(writer) = self.writer.take() {
            let _ = writer.await;
        }
    }
}

// Answers or sends the close frame, so the server sees a clean close.
fn close_in_background(mut sink: Sink) {
    tokio::spawn(async move {
        let _ = timeout(CLOSE_TIMEOUT, sink.close()).await;
    });
}

async fn write_pipe(mut out: File, mut lines: mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Some(line) = lines.recv().await {
        if let Err(e) = write_all(&mut out, &line).await {
            eprintln!("Failed to write to output pipe: {e}");
            exit(1);
        }
    }
}

async fn write_all(out: &mut File, line: &[u8]) -> std::io::Result<()> {
    out.write_all(line).await?;
    out.flush().await
}

fn spawn_reader(id: u64, mut stream: SplitStream<Socket>, tx: mpsc::UnboundedSender<ServerEvent>) {
    tokio::spawn(async move {
        while let Some(result) = stream.next().await {
            match result {
                Ok(message) => {
                    if cfg!(debug_assertions) {
                        log_message(&message);
                    }

                    let closing = message.is_close();
                    if tx.send((id, Some(message))).is_err() || closing {
                        break;
                    }
                }
                Err(e) => {
                    if cfg!(debug_assertions) {
                        eprintln!("Error receiving message: {e}");
                    }
                    break;
                }
            }
        }

        let _ = tx.send((id, None));
    });
}

fn log_message(message: &Message) {
    match message {
        Message::Text(_) => println!("Received Text message"),
        Message::Binary(_) => println!("Received Binary message"),
        Message::Ping(_) => println!("Received Ping message"),
        Message::Pong(_) => println!("Received Pong message"),
        Message::Close(_) => println!("Received Close message"),
        Message::Frame(_) => println!("Received an unexpected type of message"),
    }
}

// Retries until the server takes the new connection or the deadline passes. A
// server that answers with anything other than the resumed command has
// declined, and is not asked again.
async fn connect_with_retry(url: &Url, headers: &[(String, String)]) -> Option<Resumed> {
    let deadline = Instant::now() + RESUME_DEADLINE;
    let mut backoff = RESUME_INITIAL_BACKOFF;

    loop {
        let request = build_request(url, headers)?;
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(RESUME_ATTEMPT_TIMEOUT.min(left), connect_async(request)).await {
            Ok(Ok((mut socket, _))) => {
                let left = deadline.saturating_duration_since(Instant::now());
                return match timeout(RESUMED_TIMEOUT.min(left), await_resumed(&mut socket)).await {
                    Ok(Some(again)) => Some(Resumed { socket, again }),
                    Ok(None) => {
                        eprintln!("The server declined the new connection");
                        None
                    }
                    Err(_) => {
                        eprintln!("The server didn't confirm the new connection in time");
                        None
                    }
                };
            }
            Ok(Err(e)) => eprintln!("Reconnect attempt failed: {e}"),
            Err(_) => eprintln!("Reconnect attempt timed out"),
        }

        if Instant::now() + backoff >= deadline {
            return None;
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(RESUME_MAX_BACKOFF);
    }
}

// Waits for the server to confirm it moved the runner onto `socket`; `None` if
// it declines. The client already has its greeting, so the new connection's
// isn't passed on. A request to move again before the confirmation is kept.
async fn await_resumed(socket: &mut Socket) -> Option<Option<Headers>> {
    let mut again = None;
    while let Some(Ok(message)) = socket.next().await {
        match message {
            Message::Text(text) if is_command(&text, "connected") => {}
            Message::Text(text) if is_command(&text, RESUMED_COMMAND) => return Some(again),
            Message::Text(text) => match resume_headers(&text) {
                Some(headers) => again = Some(headers),
                None => return None,
            },
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            Message::Binary(_) | Message::Close(_) => return None,
        }
    }
    None
}

fn build_request(url: &Url, headers: &[(String, String)]) -> Option<Request<()>> {
    let mut request = match url.as_str().into_client_request() {
        Ok(request) => request,
        Err(e) => {
            eprintln!("Invalid reconnect URL: {e}");
            return None;
        }
    };

    for (name, value) in headers {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(value)) if !is_reserved_header(&name) => {
                request.headers_mut().insert(name, value);
            }
            _ => eprintln!("Ignoring reconnect header: {name}"),
        }
    }

    Some(request)
}

// Headers the websocket handshake sets itself.
fn is_reserved_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    matches!(
        name,
        "host" | "connection" | "upgrade" | "content-length" | "transfer-encoding"
    ) || name.starts_with("sec-websocket-")
}

// Returns `url` with each of `params` set, replacing any existing values.
fn with_params(url: &Url, params: &[(&str, &str)]) -> Url {
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| !params.iter().any(|(name, _)| key == name))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();

    let mut url = url.clone();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(kept)
        .extend_pairs(params.iter().copied());
    url
}

fn parse_command(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text)
        .ok()
        .filter(|value| value.get("command").is_some())
}

fn is_command(text: &str, command: &str) -> bool {
    text.contains(command)
        && parse_command(text)
            .is_some_and(|value| value.get("command").and_then(Value::as_str) == Some(command))
}

// The headers to send with the new connection, when `text` asks for a resume.
fn resume_headers(text: &str) -> Option<Headers> {
    if !is_command(text, RESUME_COMMAND) {
        return None;
    }

    let headers = parse_command(text)?
        .get("headers")
        .and_then(Value::as_object)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();

    Some(headers)
}

async fn read_pipe(path: String, tx: mpsc::UnboundedSender<PipeItem>) {
    let mut pipe = OpenOptions::new().read(true).open(path).await.unwrap();
    let mut buf: Vec<u8> = Vec::new();
    // Reused across iterations rather than reallocated per read. 64 KiB instead
    // of 1 KiB cuts the syscall count on large messages by 64x.
    let mut temp_buf = vec![0u8; 65536];
    loop {
        let n = match pipe.read(&mut temp_buf).await {
            Err(_) | Ok(0) => break,
            Ok(n) => n,
        };

        // Only the bytes that just arrived can contain a newline we have not
        // already looked at. Rescanning `buf` from index 0 on every read makes
        // delivery quadratic in message size: a 14 MB test manifest contains no
        // newline until its final byte, so the old code scanned an average of
        // 7 MB per read for ~220 reads. Measured end to end, a 4.3 MB manifest
        // took 5.4s to arrive and 8.6 MB never arrived at all.
        let mut from = buf.len();
        buf.extend_from_slice(&temp_buf[..n]);

        while let Some(rel) = buf[from..].iter().position(|&b| b == b'\n') {
            let idx = from + rel;
            let line = buf.drain(..=idx).collect::<Vec<_>>();
            let line_str = String::from_utf8(line).unwrap();
            if line_str.trim() == "exit" {
                let _ = tx.send(PipeItem::Exit);
                return;
            }
            let _ = tx.send(PipeItem::Line(Message::text(line_str)));
            // Everything still buffered sits after the newline we just consumed
            // and has not been scanned yet.
            from = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_params_replaces_existing_values_and_keeps_the_rest() {
        let url = Url::parse("wss://example.test/ws?runner_id=7&reconnect=false").unwrap();
        let url = with_params(&url, &[("reconnect", "true"), ("resume", "true")]);

        assert_eq!(url.query(), Some("runner_id=7&reconnect=true&resume=true"));
    }

    #[test]
    fn resume_headers_reads_the_command() {
        let text = r#"{"command":"transport_reconnect","headers":{"x-a":"1","x-b":2}}"#;
        assert_eq!(
            resume_headers(text),
            Some(vec![("x-a".to_string(), "1".to_string())])
        );
        assert_eq!(
            resume_headers(r#"{"command":"transport_reconnect"}"#),
            Some(vec![])
        );
    }

    #[test]
    fn resume_headers_ignores_other_messages() {
        assert_eq!(resume_headers(r#"{"command":"reconnect"}"#), None);
        assert_eq!(
            resume_headers(r#"{"command":"print_notice","message":"transport_reconnect"}"#),
            None
        );
        assert_eq!(resume_headers("transport_reconnect"), None);
    }

    #[test]
    fn build_request_keeps_the_handshake_headers_its_own() {
        let url = Url::parse("ws://example.test/ws").unwrap();
        let headers = [
            ("fly-prefer-region", "sjc"),
            ("Host", "elsewhere.test"),
            ("Sec-WebSocket-Key", "forged"),
            ("upgrade", "h2c"),
        ]
        .map(|(name, value)| (name.to_string(), value.to_string()));

        let request = build_request(&url, &headers).unwrap();
        let get = |name: &str| {
            request
                .headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_string())
        };

        assert_eq!(get("fly-prefer-region").as_deref(), Some("sjc"));
        assert_eq!(get("host").as_deref(), Some("example.test"));
        assert_ne!(get("sec-websocket-key").as_deref(), Some("forged"));
        assert_eq!(get("upgrade").as_deref(), Some("websocket"));
    }
}
