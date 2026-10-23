use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::process::exit;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, timeout_at, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, Request};
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream,
};
use transport::Keepalive;
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

pub async fn run_fifo(url: &str, runner_id: &str) -> i32 {
    let url = with_params(&Url::parse(url).unwrap(), &[FEATURES_PARAM]);

    let input_pipe_path = format!("/tmp/{runner_id}_1");
    let output_pipe_path = format!("/tmp/{runner_id}_2");

    let (pipe_tx, pipe_rx) = mpsc::unbounded_channel::<PipeItem>();
    tokio::spawn(read_pipe(input_pipe_path, pipe_tx));

    let out = OpenOptions::new()
        .write(true)
        .open(output_pipe_path)
        .await
        .unwrap();
    drive(&url, pipe_rx, out, None, false).await
}

pub async fn run_stdio(keepalive: Option<Keepalive>) -> i32 {
    let (pipe_tx, mut pipe_rx) = mpsc::unbounded_channel::<PipeItem>();
    tokio::spawn(read_lines(tokio::io::stdin(), pipe_tx));
    let url = match stdio_url(&mut pipe_rx).await {
        Ok(url) => with_params(&url, &[FEATURES_PARAM]),
        Err(message) => {
            eprintln!("{message}");
            exit(1);
        }
    };
    drive(&url, pipe_rx, tokio::io::stdout(), keepalive, true).await
}

async fn drive<W: AsyncWrite + Unpin + Send + 'static>(
    url: &Url,
    mut pipe_rx: mpsc::UnboundedReceiver<PipeItem>,
    out: W,
    keepalive: Option<Keepalive>,
    announce_endings: bool,
) -> i32 {
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
        resume_url: with_params(url, &[("reconnect", "true"), ("resume", "true")]),
        current: None,
        retired: HashMap::new(),
        resuming: false,
        exit_requested: false,
        ended: HashSet::new(),
        held: VecDeque::new(),
        next_id: 0,
        events_tx,
        resume_tx,
        liveness: keepalive.map(Liveness::new),
        announce_endings,
    };
    transport.adopt(socket);

    loop {
        let stop = tokio::select! {
            Some((id, message)) = events_rx.recv() => transport.on_server_event(id, message).await,
            Some(resumed) = resume_rx.recv() => transport.on_resume_finished(resumed).await,
            () = sleep_until_some(transport.ping_due()) => transport.ping().await,
            () = sleep_until_some(transport.liveness_deadline()) => transport.check_liveness().await,
            item = pipe_rx.recv(), if !transport.exit_requested => {
                // A confirmed move that's already waiting decides which
                // connection an exit closes.
                let mut stop = None;
                if matches!(item, Some(PipeItem::Exit) | None) {
                    if let Ok(resumed) = resume_rx.try_recv() {
                        stop = transport.on_resume_finished(resumed).await;
                    }
                }
                match stop {
                    Some(code) => Some(code),
                    None => transport.on_pipe_item(item).await,
                }
            }
        };

        if let Some(code) = stop {
            transport.wind_down(&mut events_rx, code).await;
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
    // Connections whose reader has reported their end.
    ended: HashSet<u64>,
    held: VecDeque<Message>,
    next_id: u64,
    events_tx: mpsc::UnboundedSender<ServerEvent>,
    resume_tx: mpsc::UnboundedSender<Option<Resumed>>,
    liveness: Option<Liveness>,
    announce_endings: bool,
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
        let heard = self.liveness.as_ref().map(|liveness| {
            liveness.hear();
            Arc::clone(&liveness.heard)
        });
        spawn_reader(id, stream, self.events_tx.clone(), heard);
        self.current.replace((id, sink))
    }

    // Handlers return an exit code when the transport should wind down.
    async fn on_server_event(&mut self, id: u64, message: Option<Message>) -> Option<i32> {
        let live = Some(id) == self.current_id() || self.retired.contains_key(&id);

        match message {
            Some(Message::Text(text)) if live => {
                if let Some(headers) = resume_headers(&text) {
                    if Some(id) == self.current_id() {
                        self.start_resume(headers).await;
                    }
                    return None;
                }

                self.write_line(text.into_bytes());
                None
            }
            Some(Message::Binary(data)) if live => {
                self.write_line(data);
                None
            }
            Some(Message::Close(_)) => {
                self.ended.insert(id);
                self.on_connection_ended(id, "websocket closed by the server")
            }
            None => {
                self.ended.insert(id);
                self.on_connection_ended(id, "websocket connection dropped")
            }
            _ => None,
        }
    }

    fn on_connection_ended(&mut self, id: u64, ending: &str) -> Option<i32> {
        if let Some(sink) = self.retired.remove(&id) {
            close_in_background(sink);
            return None;
        }

        if Some(id) != self.current_id() {
            return None;
        }

        if !self.resuming {
            if self.announce_endings {
                eprintln!("{ending}");
            }
            return Some(0);
        }

        // Asked to move and the old connection went first: wait for the
        // new one rather than exiting.
        if let Some((_, sink)) = self.current.take() {
            close_in_background(sink);
        }
        None
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

    async fn on_resume_finished(&mut self, resumed: Option<Resumed>) -> Option<i32> {
        if !self.resuming {
            return None;
        }
        self.resuming = false;

        // The server may already have moved the runner, so carrying on with the
        // old connection could lose results. The client reconnects in full.
        let Some(Resumed { socket, again }) = resumed else {
            eprintln!("Failed to move to a new connection. Exiting...");
            self.send_held().await;
            if self.exit_requested {
                self.close_current().await;
                exit(1);
            }
            return Some(1);
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
        None
    }

    async fn on_pipe_item(&mut self, item: Option<PipeItem>) -> Option<i32> {
        match item {
            Some(PipeItem::Line(message)) if self.resuming => {
                self.held.push_back(message);
                None
            }
            Some(PipeItem::Line(message)) => self.send(message).await,
            // The client stops the transport a few seconds after asking, so an
            // exit doesn't wait for a resume: what's held goes out on the
            // connection still open.
            Some(PipeItem::Exit) | None if self.resuming && self.current.is_none() => {
                self.exit_requested = true;
                None
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

    async fn send(&mut self, message: Message) -> Option<i32> {
        let (_, sink) = self.current.as_mut()?;
        let sent = match &self.liveness {
            None => sink.send(message).await,
            Some(liveness) => match liveness.race(sink.send(message)).await {
                Ok(sent) => sent,
                Err(budget) => return self.keepalive_timeout(budget).await,
            },
        };
        if sent.is_err() && !self.resuming {
            if self.announce_endings {
                eprintln!("websocket connection dropped");
            }
            return Some(0);
        }
        None
    }

    fn ping_due(&self) -> Option<Instant> {
        let liveness = self.liveness.as_ref()?;
        (self.current.is_some() && !self.resuming).then_some(liveness.next_ping)
    }

    async fn ping(&mut self) -> Option<i32> {
        let liveness = self.liveness.as_mut()?;
        liveness.next_ping = Instant::now() + liveness.keepalive.ping_interval;
        self.send(Message::Ping(b"selective".to_vec())).await
    }

    fn liveness_deadline(&self) -> Option<Instant> {
        let liveness = self.liveness.as_ref()?;
        (!self.resuming).then(|| liveness.deadline())
    }

    async fn check_liveness(&mut self) -> Option<i32> {
        let budget = self
            .liveness
            .as_ref()
            .filter(|liveness| liveness.expired())?
            .budget();
        self.keepalive_timeout(budget).await
    }

    async fn keepalive_timeout(&mut self, budget: Duration) -> Option<i32> {
        eprintln!(
            "websocket keepalive timeout: no frame from the server for {}s",
            budget.as_secs_f64()
        );
        self.finish_output().await;
        exit(0)
    }

    // Closes every connection, passes on what they still deliver (bounded by
    // the close timeout), writes everything out to the client, then exits.
    async fn wind_down(
        &mut self,
        events: &mut mpsc::UnboundedReceiver<ServerEvent>,
        code: i32,
    ) -> ! {
        let mut open: HashSet<u64> = self.retired.keys().copied().collect();
        open.extend(self.current_id());
        open.retain(|id| !self.ended.contains(id));

        self.close_current().await;
        for sink in self.retired.values_mut() {
            let _ = timeout(CLOSE_TIMEOUT, sink.close()).await;
        }

        let deadline = Instant::now() + CLOSE_TIMEOUT;
        while !open.is_empty() {
            match timeout_at(deadline, events.recv()).await {
                Ok(Some((id, Some(Message::Text(text))))) if open.contains(&id) => {
                    if resume_headers(&text).is_none() {
                        self.write_line(text.into_bytes());
                    }
                }
                Ok(Some((id, Some(Message::Binary(data))))) if open.contains(&id) => {
                    self.write_line(data);
                }
                Ok(Some((id, Some(Message::Close(_)) | None))) => {
                    open.remove(&id);
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }

        self.finish_output().await;
        exit(code);
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

async fn write_pipe<W: AsyncWrite + Unpin>(
    mut out: W,
    mut lines: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    while let Some(line) = lines.recv().await {
        if let Err(e) = write_all(&mut out, &line).await {
            eprintln!("Failed to write to output pipe: {e}");
            exit(1);
        }
    }
}

async fn write_all<W: AsyncWrite + Unpin>(out: &mut W, line: &[u8]) -> std::io::Result<()> {
    out.write_all(line).await?;
    out.flush().await
}

fn spawn_reader(
    id: u64,
    mut stream: SplitStream<Socket>,
    tx: mpsc::UnboundedSender<ServerEvent>,
    heard: Option<Arc<Mutex<Instant>>>,
) {
    tokio::spawn(async move {
        while let Some(result) = stream.next().await {
            match result {
                Ok(message) => {
                    if let Some(heard) = &heard {
                        *heard.lock().unwrap() = Instant::now();
                    }
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
        Message::Text(_) => eprintln!("Received Text message"),
        Message::Binary(_) => eprintln!("Received Binary message"),
        Message::Ping(_) => eprintln!("Received Ping message"),
        Message::Pong(_) => eprintln!("Received Pong message"),
        Message::Close(_) => eprintln!("Received Close message"),
        Message::Frame(_) => eprintln!("Received an unexpected type of message"),
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
    let pipe = OpenOptions::new().read(true).open(path).await.unwrap();
    read_lines(pipe, tx).await;
}

async fn read_lines<R: AsyncRead + Unpin>(mut pipe: R, tx: mpsc::UnboundedSender<PipeItem>) {
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

struct Liveness {
    keepalive: Keepalive,
    heard: Arc<Mutex<Instant>>,
    next_ping: Instant,
}

impl Liveness {
    fn new(keepalive: Keepalive) -> Self {
        let now = Instant::now();
        Self {
            keepalive,
            heard: Arc::new(Mutex::new(now)),
            next_ping: now + keepalive.ping_interval,
        }
    }

    fn hear(&self) {
        *self.heard.lock().unwrap() = Instant::now();
    }

    fn budget(&self) -> Duration {
        self.keepalive.budget()
    }

    fn deadline(&self) -> Instant {
        *self.heard.lock().unwrap() + self.budget()
    }

    fn expired(&self) -> bool {
        Instant::now() >= self.deadline()
    }

    async fn race<F: Future>(&self, sending: F) -> Result<F::Output, Duration> {
        tokio::pin!(sending);
        loop {
            tokio::select! {
                sent = &mut sending => return Ok(sent),
                () = sleep(self.deadline().saturating_duration_since(Instant::now())) => {
                    if self.expired() {
                        return Err(self.budget());
                    }
                }
            }
        }
    }
}

async fn sleep_until_some(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep(deadline.saturating_duration_since(Instant::now())).await,
        None => std::future::pending().await,
    }
}

async fn stdio_url(lines: &mut mpsc::UnboundedReceiver<PipeItem>) -> Result<Url, String> {
    let text = match std::env::var("SELECTIVE_TRANSPORT_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            match timeout(Duration::from_secs(30), lines.recv()).await {
                Ok(Some(PipeItem::Line(Message::Text(line)))) => {
                    let line = line.trim().to_owned();
                    if !(line.starts_with("ws://") || line.starts_with("wss://")) {
                        return Err("the first stdin line must be a ws:// or wss:// url when SELECTIVE_TRANSPORT_URL is unset".to_owned());
                    }
                    line
                }
                _ => return Err(
                    "SELECTIVE_TRANSPORT_URL is unset and stdin did not provide a websocket url"
                        .to_owned(),
                ),
            }
        }
    };
    Url::parse(&text).map_err(|err| format!("invalid websocket url {text}: {err}"))
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
