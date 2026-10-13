#![warn(clippy::perf, clippy::style, clippy::pedantic)]

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::process::exit;
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
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

// Tells the server this transport can move its own connection when asked to.
const FEATURES_PARAM: (&str, &str) = ("transport_features", "resume");

// Sent by a server that wants this runner on a new connection (for example,
// because the node holding the current one is shutting down). The client
// library only reads the pipe between tests, so a runner busy in a long test
// can't act on a reconnect command in time. The transport owns the socket, so
// it moves the connection itself and the client never notices.
const RESUME_COMMAND: &str = "transport_reconnect";

const RESUME_DEADLINE: Duration = Duration::from_mins(1);
const RESUME_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
const RESUME_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const RESUME_MAX_BACKOFF: Duration = Duration::from_secs(4);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

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

    let (pipe_tx, mut pipe_rx) = futures_channel::mpsc::unbounded();
    let (exit_tx, mut exit_rx) = mpsc::channel(1);
    tokio::spawn(read_pipe(input_pipe_path, pipe_tx, exit_tx));

    let out = OpenOptions::new()
        .write(true)
        .open(output_pipe_path)
        .await
        .unwrap();

    let Ok((socket, _)) = connect_async(url.as_str()).await else {
        eprintln!("Failed to connect to the WebSocket server. Exiting...");
        exit(1);
    };

    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<ServerEvent>();
    let (resume_tx, mut resume_rx) = mpsc::unbounded_channel::<Option<Socket>>();

    let mut transport = Transport {
        out,
        resume_url: with_params(&url, &[("reconnect", "true"), ("resume", "true")]),
        current: None,
        retired: None,
        awaiting_connected: None,
        resuming: false,
        pending: None,
        next_id: 0,
        events_tx,
        resume_tx,
    };
    transport.adopt(socket);

    loop {
        tokio::select! {
            Some((id, message)) = events_rx.recv() => transport.on_server_event(id, message).await,
            Some(socket) = resume_rx.recv() => transport.on_resume_finished(socket).await,
            message = pipe_rx.next(), if transport.can_send() => transport.on_pipe_message(message).await,
            _ = exit_rx.recv() => exit(0),
        }
    }
}

struct Transport {
    out: File,
    resume_url: Url,
    // The connection messages from the pipe are sent on.
    current: Option<(u64, Sink)>,
    // The connection being replaced. It is read until it ends, since the
    // server may have sent on it before switching over.
    retired: Option<u64>,
    // A resumed connection whose greeting hasn't arrived yet. The client
    // already had one, so it is not forwarded.
    awaiting_connected: Option<u64>,
    resuming: bool,
    // A message whose send failed while a resume was in flight; sent on the
    // new connection once it is up.
    pending: Option<Message>,
    next_id: u64,
    events_tx: mpsc::UnboundedSender<ServerEvent>,
    resume_tx: mpsc::UnboundedSender<Option<Socket>>,
}

impl Transport {
    fn can_send(&self) -> bool {
        self.current.is_some() && self.pending.is_none()
    }

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
        let live = Some(id) == self.current_id() || Some(id) == self.retired;

        match message {
            Some(Message::Text(text)) if live => {
                if self
                    .awaiting_connected
                    .take_if(|awaiting| *awaiting == id)
                    .is_some()
                    && is_command(&text, "connected")
                {
                    return;
                }

                if let Some(headers) = resume_headers(&text) {
                    if Some(id) == self.current_id() {
                        self.start_resume(headers);
                    }
                    return;
                }

                self.write_line(text.as_bytes()).await;
            }
            Some(Message::Binary(data)) if live => {
                self.awaiting_connected.take_if(|awaiting| *awaiting == id);
                self.write_line(&data).await;
            }
            Some(Message::Close(_)) | None => self.on_connection_ended(id),
            _ => {}
        }
    }

    fn on_connection_ended(&mut self, id: u64) {
        if Some(id) == self.retired {
            self.retired = None;
        } else if Some(id) == self.current_id() {
            if !self.resuming {
                exit(0);
            }

            // Asked to move and the old connection went first: wait for the
            // new one rather than exiting.
            self.current = None;
        }
    }

    fn start_resume(&mut self, headers: Vec<(String, String)>) {
        if self.resuming {
            return;
        }

        self.resuming = true;
        let url = self.resume_url.clone();
        let resume_tx = self.resume_tx.clone();
        tokio::spawn(async move {
            let _ = resume_tx.send(connect_with_retry(&url, &headers).await);
        });
    }

    async fn on_resume_finished(&mut self, socket: Option<Socket>) {
        self.resuming = false;

        let Some(socket) = socket else {
            if self.current.is_none() || self.pending.is_some() {
                eprintln!("Failed to reconnect to the WebSocket server. Exiting...");
                exit(1);
            }

            // The old connection is still up; carry on with it.
            return;
        };

        if let Some((old_id, mut old_sink)) = self.adopt(socket) {
            self.retired = Some(old_id);
            let _ = timeout(CLOSE_TIMEOUT, old_sink.close()).await;
        }
        self.awaiting_connected = self.current_id();

        if let Some(message) = self.pending.take() {
            self.send(message).await;
        }
    }

    async fn on_pipe_message(&mut self, message: Option<Message>) {
        match message {
            Some(message) => self.send(message).await,
            None => exit(0),
        }
    }

    async fn send(&mut self, message: Message) {
        let resuming = self.resuming;
        let Some((_, sink)) = self.current.as_mut() else {
            self.pending = Some(message);
            return;
        };

        if resuming {
            let copy = message.clone();
            if sink.send(message).await.is_err() {
                self.pending = Some(copy);
            }
        } else if sink.send(message).await.is_err() {
            exit(0);
        }
    }

    async fn write_line(&mut self, data: &[u8]) {
        let mut line = Vec::with_capacity(data.len() + 1);
        line.extend_from_slice(data);
        line.push(b'\n');
        self.out
            .write_all(&line)
            .await
            .expect("Failed to write to output pipe");
        self.out.flush().await.expect("Failed to flush output pipe");
    }
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

async fn connect_with_retry(url: &Url, headers: &[(String, String)]) -> Option<Socket> {
    let deadline = Instant::now() + RESUME_DEADLINE;
    let mut backoff = RESUME_INITIAL_BACKOFF;

    loop {
        let request = build_request(url, headers)?;
        match timeout(RESUME_ATTEMPT_TIMEOUT, connect_async(request)).await {
            Ok(Ok((socket, _))) => return Some(socket),
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
            (Ok(name), Ok(value)) => {
                request.headers_mut().insert(name, value);
            }
            _ => eprintln!("Ignoring invalid reconnect header: {name}"),
        }
    }

    Some(request)
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
fn resume_headers(text: &str) -> Option<Vec<(String, String)>> {
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

async fn read_pipe(
    path: String,
    tx: futures_channel::mpsc::UnboundedSender<Message>,
    exit_tx: mpsc::Sender<()>,
) {
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
                exit_tx.send(()).await.expect("Failed to send exit signal");
                return;
            }
            tx.unbounded_send(Message::text(line_str)).unwrap();
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
}
