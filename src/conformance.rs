use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::error::Error;
use crate::reference_client::Via;

#[derive(Debug, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Step {
    Send { message: Value },
    ExpectManifest { tree: bool },
    ExpectResult { id: String, status: String },
    ExpectResults { ids: Vec<String> },
    ExpectType { frame_type: String },
    ExpectTerminating { signal: String },
    Drop,
    ExpectReconnect,
    ExpectFragment { ids: Vec<String> },
    ExpectReport { connection_retries: u64 },
    ExpectNoFrame { ms: u64 },
    ArmHoldUpload,
    WaitUpload,
    SignalTerm,
    ExpectExit { code: i32 },
    RejectConnections { times: u32 },
}

#[derive(Clone, Copy, Debug)]
pub enum SuiteVia {
    Direct,
    Stdio,
    Both,
}

impl SuiteVia {
    fn list(self) -> Vec<Via> {
        match self {
            Self::Direct => vec![Via::Direct],
            Self::Stdio => vec![Via::Stdio],
            Self::Both => vec![Via::Direct, Via::Stdio],
        }
    }
}

pub fn scenarios() -> Result<Vec<Scenario>, Error> {
    const RAW: &[&str] = &[
        include_str!("../conformance/scenarios/happy_two_batches.json"),
        include_str!("../conformance/scenarios/remove_failed.json"),
        include_str!("../conformance/scenarios/repeated_id.json"),
        include_str!("../conformance/scenarios/reconnect.json"),
        include_str!("../conformance/scenarios/socket_drop.json"),
        include_str!("../conformance/scenarios/close_rejected.json"),
        include_str!("../conformance/scenarios/sigterm_after_failure.json"),
        include_str!("../conformance/scenarios/test_map_fragment.json"),
        include_str!("../conformance/scenarios/queued_before_connected.json"),
        include_str!("../conformance/scenarios/unknown_command.json"),
        include_str!("../conformance/scenarios/reconnect_exhausted.json"),
        include_str!("../conformance/scenarios/overlap_buffered.json"),
        include_str!("../conformance/scenarios/upload_stall.json"),
    ];
    RAW.iter()
        .map(|raw| serde_json::from_str(raw).map_err(Error::from))
        .collect()
}

pub fn scenario_names() -> Result<Vec<String>, Error> {
    Ok(scenarios()?
        .into_iter()
        .map(|scenario| scenario.name)
        .collect())
}

pub async fn run_suite(bin: &Path, filter: Option<&str>, via: SuiteVia) -> Result<(), Error> {
    let mut failures = Vec::new();
    for scenario in scenarios()? {
        if filter.is_some_and(|name| name != scenario.name) {
            continue;
        }
        if scenario
            .steps
            .iter()
            .any(|step| matches!(step, Step::SignalTerm))
            && !cfg!(unix)
        {
            eprintln!("skip {} (signals are unix-only)", scenario.name);
            continue;
        }
        for via in via.list() {
            match run_one(bin, &scenario, via).await {
                Ok(()) => eprintln!("ok {} {}", scenario.name, via_name(via)),
                Err(err) => failures.push(format!("{} {}: {err}", scenario.name, via_name(via))),
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::new(failures.join("\n")))
    }
}

fn via_name(via: Via) -> &'static str {
    match via {
        Via::Direct => "direct",
        Via::Stdio => "stdio",
    }
}

async fn run_one(bin: &Path, scenario: &Scenario, via: Via) -> Result<(), Error> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let url = format!(
        "ws://{addr}/transport/websocket?api_key=conformance-key&run_id=run-1&run_attempt=1&runner_id=runner-1&metadata=%7B%22ci%22%3A%22conformance%22%7D"
    );
    let gate = Arc::new(Gate {
        reject_budget: AtomicU32::new(match scenario.steps.first() {
            Some(Step::RejectConnections { times }) => *times,
            _ => 0,
        }),
        hold_uploads: AtomicBool::new(false),
        upload_held: AtomicBool::new(false),
        upload_notify: Notify::new(),
    });
    let logs = Arc::new(Mutex::new(String::new()));
    let mut command = Command::new(bin);
    command
        .arg("conformance-client")
        .env("SELECTIVE_TRANSPORT_URL", &url)
        .env("SELECTIVE_CONFORMANCE_FAST", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if matches!(via, Via::Stdio) {
        command.arg("--via-stdio");
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let accept_gate = Arc::clone(&gate);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            let gate = Arc::clone(&accept_gate);
            tokio::spawn(async move {
                match classify(stream, &gate).await {
                    Ok(incoming) => {
                        let _ = tx.send(incoming);
                    }
                    Err(_) => {}
                }
            });
        }
    });
    let mut child = command.spawn()?;
    drain_pipe(child.stdout.take(), Arc::clone(&logs));
    drain_pipe(child.stderr.take(), Arc::clone(&logs));
    let mut player = Player {
        incoming: rx,
        current: None,
        fragments: Vec::new(),
        child: Some(child),
        saw_connection: false,
        gate,
    };
    let played = player.play(scenario).await;
    if played.is_err() {
        if let Some(mut child) = player.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
    played.map_err(|err| {
        let extra = logs.lock().map(|guard| guard.clone()).unwrap_or_default();
        Error::new(format!("{err}\n--- client output ---\n{extra}"))
    })
}

fn drain_pipe<R>(pipe: Option<R>, logs: Arc<Mutex<String>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(pipe) = pipe else {
        return;
    };
    tokio::spawn(async move {
        let mut reader = BufReader::new(pipe);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Ok(mut guard) = logs.lock() {
                        guard.push_str(&line);
                    }
                }
            }
        }
    });
}

struct Live {
    uri: String,
    ws: WebSocketStream<TcpStream>,
}

struct CapturedHttp {
    header: String,
    body: Vec<u8>,
}

enum Incoming {
    Socket(Live),
    Fragment(CapturedHttp),
    Rejected,
}

struct Gate {
    reject_budget: AtomicU32,
    hold_uploads: AtomicBool,
    upload_held: AtomicBool,
    upload_notify: Notify,
}

struct Player {
    incoming: tokio::sync::mpsc::UnboundedReceiver<Incoming>,
    current: Option<Live>,
    fragments: Vec<CapturedHttp>,
    child: Option<Child>,
    saw_connection: bool,
    gate: Arc<Gate>,
}

impl Player {
    async fn play(&mut self, scenario: &Scenario) -> Result<(), Error> {
        for step in &scenario.steps {
            self.step(step)
                .await
                .map_err(|err| Error::new(format!("{}: {err}", scenario.name)))?;
        }
        Ok(())
    }

    async fn step(&mut self, step: &Step) -> Result<(), Error> {
        match step {
            Step::Send { message } => {
                self.ensure_connection().await?;
                let text = serde_json::to_string(message)?;
                let live = self
                    .current
                    .as_mut()
                    .ok_or_else(|| Error::new("no websocket"))?;
                live.ws.send(Message::Text(text)).await?;
                Ok(())
            }
            Step::ExpectManifest { tree } => {
                let frame = self.next_frame().await?;
                expect_manifest(&frame, *tree)
            }
            Step::ExpectResult { id, status } => {
                let frame = self.next_frame().await?;
                expect_result(&frame, id, status)
            }
            Step::ExpectResults { ids } => {
                for id in ids {
                    let frame = self.next_frame().await?;
                    let status = frame
                        .pointer("/data/status")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    expect_result(&frame, id, status)?;
                }
                Ok(())
            }
            Step::ExpectType { frame_type } => {
                let frame = self.next_frame().await?;
                let got = frame.get("type").and_then(Value::as_str).unwrap_or("");
                if got == frame_type {
                    Ok(())
                } else {
                    Err(Error::new(format!(
                        "expected type {frame_type}, got {frame}"
                    )))
                }
            }
            Step::ExpectTerminating { signal } => {
                let frame = self.next_frame().await?;
                let got_type = frame.get("type").and_then(Value::as_str).unwrap_or("");
                let got_signal = frame
                    .pointer("/data/signal")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if got_type == "terminating" && got_signal == signal {
                    Ok(())
                } else {
                    Err(Error::new(format!(
                        "expected terminating {signal}, got {frame}"
                    )))
                }
            }
            Step::Drop => {
                self.current = None;
                Ok(())
            }
            Step::ExpectReconnect => self.expect_reconnect().await,
            Step::ExpectFragment { ids } => self.expect_fragment(ids).await,
            Step::ExpectReport { connection_retries } => {
                let frame = self.next_frame().await?;
                expect_report(&frame, *connection_retries)
            }
            Step::ExpectNoFrame { ms } => self.expect_no_frame(*ms).await,
            Step::ArmHoldUpload => {
                self.gate.hold_uploads.store(true, Ordering::SeqCst);
                Ok(())
            }
            Step::WaitUpload => self.wait_upload().await,
            Step::SignalTerm => self.signal_term(),
            Step::ExpectExit { code } => self.expect_exit(*code).await,
            Step::RejectConnections { times } => self.reject_connections(*times).await,
        }
    }

    async fn ensure_connection(&mut self) -> Result<(), Error> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.current.is_none() {
            self.pump(deadline).await?;
        }
        Ok(())
    }

    async fn expect_reconnect(&mut self) -> Result<(), Error> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self
                .current
                .as_ref()
                .is_some_and(|live| live.uri.contains("reconnect=true"))
            {
                return Ok(());
            }
            self.pump(deadline).await?;
        }
        Err(Error::new("timed out waiting for reconnect=true"))
    }

    async fn next_frame(&mut self) -> Result<Value, Error> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(frame) = self.pump(deadline).await? {
                if frame.as_str() == Some("exit") {
                    return Err(Error::new("exit was forwarded as a websocket frame"));
                }
                return Ok(frame);
            }
        }
        Err(Error::new("timed out waiting for a client frame"))
    }

    async fn pump(&mut self, deadline: Instant) -> Result<Option<Value>, Error> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::new("timed out"));
        }
        let mut live = self.current.take();
        let event = {
            let incoming = &mut self.incoming;
            tokio::select! {
                next = incoming.recv() => Event::Accepted(next),
                message = recv_live(live.as_mut()), if live.is_some() => Event::Socket(message),
                _ = tokio::time::sleep(remaining) => Event::Timeout,
            }
        };
        match event {
            Event::Accepted(Some(Incoming::Socket(socket))) => {
                self.install(socket)?;
                Ok(None)
            }
            Event::Accepted(Some(Incoming::Fragment(fragment))) => {
                self.current = live;
                self.fragments.push(fragment);
                Ok(None)
            }
            Event::Accepted(Some(Incoming::Rejected)) => {
                self.current = live;
                Err(Error::new(
                    "handshake was refused outside reject_connections",
                ))
            }
            Event::Accepted(None) => {
                self.current = live;
                Err(Error::new("connection listener stopped"))
            }
            Event::Socket(Pump::Frame(value)) => {
                self.current = live;
                Ok(Some(value))
            }
            Event::Socket(Pump::Continue) => {
                self.current = live;
                Ok(None)
            }
            Event::Socket(Pump::Closed) => Ok(None),
            Event::Timeout => {
                self.current = live;
                Err(Error::new("timed out waiting for the client"))
            }
        }
    }

    fn install(&mut self, socket: Live) -> Result<(), Error> {
        if !connect_metadata_is_json(&socket.uri) {
            return Err(Error::new(format!(
                "connect metadata is missing or not JSON: {}",
                socket.uri
            )));
        }
        if !self.saw_connection {
            if socket.uri.contains("reconnect=true") {
                return Err(Error::new(
                    "the first connection must not set reconnect=true",
                ));
            }
            self.saw_connection = true;
        }
        self.current = Some(socket);
        Ok(())
    }

    async fn expect_no_frame(&mut self, ms: u64) -> Result<(), Error> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            if Instant::now() >= deadline {
                return Ok(());
            }
            match self.pump(deadline).await {
                Ok(Some(frame)) => {
                    return Err(Error::new(format!(
                        "expected no frame before connected, got {frame}"
                    )));
                }
                Ok(None) => {}
                Err(err) if err.to_string() == "timed out waiting for the client" => return Ok(()),
                Err(err) => return Err(err),
            }
        }
    }

    async fn wait_upload(&mut self) -> Result<(), Error> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.gate.upload_held.load(Ordering::SeqCst) {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::new("timed out waiting for the stalled upload"));
            }
            let _ = tokio::time::timeout(remaining, self.gate.upload_notify.notified()).await;
        }
    }

    async fn expect_fragment(&mut self, ids: &[String]) -> Result<(), Error> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.fragments.is_empty() && Instant::now() < deadline {
            match self.pump(deadline).await {
                Ok(_) => {}
                Err(err) if self.fragments.is_empty() => return Err(err),
                Err(_) => break,
            }
        }
        let captured = self
            .fragments
            .first()
            .ok_or_else(|| Error::new("missing fragment POST"))?;
        let header = captured.header.to_ascii_lowercase();
        if !header.contains("authorization: conformance-key") {
            return Err(Error::new(format!(
                "fragment authorization header missing: {}",
                captured.header
            )));
        }
        if !header.contains("content-type: application/gzip") {
            return Err(Error::new("fragment content-type is not application/gzip"));
        }
        let request_line = captured.header.lines().next().unwrap_or("");
        if !request_line.contains("/test_maps/fragments")
            || !request_line.contains("run_id=run-1")
            || !request_line.contains("run_attempt=1")
            || !request_line.contains("runner_id=runner-1")
        {
            return Err(Error::new(format!(
                "unexpected fragment request line: {request_line}"
            )));
        }
        let mut decoder = flate2::read::GzDecoder::new(captured.body.as_slice());
        let mut plain = String::new();
        use std::io::Read;
        decoder.read_to_string(&mut plain)?;
        let json: Value = serde_json::from_str(&plain)?;
        if json.get("version").and_then(Value::as_u64) != Some(1) {
            return Err(Error::new(format!("fragment version: {json}")));
        }
        let files = json
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::new("fragment files"))?;
        let tests = json
            .get("tests")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::new("fragment tests"))?;
        let got: Vec<&str> = tests
            .iter()
            .filter_map(|test| test.get("id").and_then(Value::as_str))
            .collect();
        let expected: Vec<&str> = ids.iter().map(String::as_str).collect();
        if got != expected {
            return Err(Error::new(format!("fragment ids {got:?} != {expected:?}")));
        }
        for test in tests {
            let deps = test
                .get("deps")
                .and_then(Value::as_array)
                .ok_or_else(|| Error::new("fragment deps"))?;
            for dep in deps {
                let index = dep.as_u64().ok_or_else(|| Error::new("dep index"))? as usize;
                if index >= files.len() {
                    return Err(Error::new("dep index out of range"));
                }
            }
        }
        Ok(())
    }

    fn signal_term(&mut self) -> Result<(), Error> {
        let child = self
            .child
            .as_ref()
            .ok_or_else(|| Error::new("no client process"))?;
        let pid = child_term_target(child.id())?;
        let status = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::new("kill -TERM failed"))
        }
    }

    async fn expect_exit(&mut self, code: i32) -> Result<(), Error> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| Error::new("no client process"))?;
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .map_err(|_| Error::new("timed out waiting for the client to exit"))??;
        if status.code() == Some(code) {
            Ok(())
        } else {
            Err(Error::new(format!("exit {status}, expected {code}")))
        }
    }

    async fn reject_connections(&mut self, times: u32) -> Result<(), Error> {
        for _ in 0..times {
            let next = tokio::time::timeout(Duration::from_secs(5), self.incoming.recv())
                .await
                .map_err(|_| Error::new("timed out accepting a rejected connection"))?
                .ok_or_else(|| Error::new("connection listener stopped"))?;
            if !matches!(next, Incoming::Rejected) {
                return Err(Error::new("expected the handshake to be refused with 503"));
            }
        }
        Ok(())
    }
}

fn child_term_target(pid: Option<u32>) -> Result<u32, Error> {
    match pid {
        Some(pid) if pid != 0 => Ok(pid),
        _ => Err(Error::new("client pid is unavailable")),
    }
}

enum Event {
    Accepted(Option<Incoming>),
    Socket(Pump),
    Timeout,
}

async fn classify(mut stream: TcpStream, gate: &Arc<Gate>) -> Result<Incoming, Error> {
    let mut peeked = [0u8; 4];
    let n = stream.peek(&mut peeked).await.unwrap_or(0);
    if n >= 4 && &peeked == b"POST" {
        if gate.hold_uploads.load(Ordering::SeqCst) {
            let _held = read_http_request(&mut stream).await?;
            gate.upload_held.store(true, Ordering::SeqCst);
            gate.upload_notify.notify_waiters();
            loop {
                std::future::pending::<()>().await;
            }
        }
        return Ok(Incoming::Fragment(read_http(stream).await?));
    }
    let uri = Arc::new(Mutex::new(String::new()));
    let uri_for_cb = Arc::clone(&uri);
    let budget = Arc::clone(gate);
    let rejected = Arc::new(AtomicBool::new(false));
    let rejected_cb = Arc::clone(&rejected);
    let handshake = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &Request, response: Response| {
            if budget
                .reject_budget
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                    current.checked_sub(1)
                })
                .is_ok()
            {
                rejected_cb.store(true, Ordering::SeqCst);
                return Err(http::Response::builder()
                    .status(503)
                    .body(Some("Service Unavailable".to_owned()))
                    .expect("503 response"));
            }
            if let Ok(mut slot) = uri_for_cb.lock() {
                *slot = request.uri().to_string();
            }
            Ok(response)
        },
    )
    .await;
    if rejected.load(Ordering::SeqCst) {
        return Ok(Incoming::Rejected);
    }
    let ws = handshake?;
    let uri = uri.lock().map(|guard| guard.clone()).unwrap_or_default();
    Ok(Incoming::Socket(Live { uri, ws }))
}

enum Pump {
    Continue,
    Frame(Value),
    Closed,
}

async fn recv_live(live: Option<&mut Live>) -> Pump {
    let Some(live) = live else {
        std::future::pending::<()>().await;
        return Pump::Closed;
    };
    match live.ws.next().await {
        Some(Ok(Message::Text(text))) => {
            if text.trim() == "exit" {
                return Pump::Frame(Value::String("exit".to_owned()));
            }
            match serde_json::from_str(text.trim()) {
                Ok(value) => Pump::Frame(value),
                Err(_) => Pump::Frame(Value::String(text)),
            }
        }
        Some(Ok(Message::Ping(payload))) => {
            let _ = live.ws.send(Message::Pong(payload)).await;
            Pump::Continue
        }
        Some(Ok(Message::Close(_))) | None => Pump::Closed,
        Some(Ok(_)) => Pump::Continue,
        Some(Err(_)) => Pump::Closed,
    }
}

fn expect_manifest(frame: &Value, tree: bool) -> Result<(), Error> {
    if frame.get("type").and_then(Value::as_str) != Some("test_manifest") {
        return Err(Error::new(format!("expected test_manifest, got {frame}")));
    }
    let ids: Vec<&str> = frame
        .pointer("/data/test_cases")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::new("manifest test_cases"))?
        .iter()
        .filter_map(|case| case.get("id").and_then(Value::as_str))
        .collect();
    let expected = ["case-pass", "case-fail", "case-pend", "case-flake"];
    if ids != expected {
        return Err(Error::new(format!("manifest ids {ids:?}")));
    }
    let tree_value = frame.pointer("/data/test_map_tree");
    if tree {
        let tree_value = tree_value.ok_or_else(|| Error::new("missing test_map_tree"))?;
        for key in ["sha", "prefix", "files", "dirty"] {
            if tree_value.get(key).is_none() {
                return Err(Error::new(format!("test_map_tree missing {key}")));
            }
        }
        if !tree_value.get("files").is_some_and(Value::is_object)
            || !tree_value.get("dirty").is_some_and(Value::is_array)
        {
            return Err(Error::new("test_map_tree shape"));
        }
    } else if tree_value.is_some() {
        return Err(Error::new("test_map_tree was not requested"));
    }
    Ok(())
}

fn expect_result(frame: &Value, id: &str, status: &str) -> Result<(), Error> {
    if frame.get("type").and_then(Value::as_str) != Some("test_case_result") {
        return Err(Error::new(format!(
            "expected test_case_result, got {frame}"
        )));
    }
    let got_id = frame
        .pointer("/data/id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let got_status = frame
        .pointer("/data/status")
        .and_then(Value::as_str)
        .unwrap_or("");
    if got_id == id && got_status == status {
        if got_status == "failed" {
            let backtrace = frame
                .pointer("/data/failure_formatted_backtrace")
                .and_then(Value::as_array);
            let first = backtrace
                .and_then(|items| items.first())
                .and_then(Value::as_str)
                .unwrap_or("");
            if !first.starts_with("tests/") {
                return Err(Error::new(format!(
                    "failure backtrace must start in the repo: {first}"
                )));
            }
        }
        Ok(())
    } else {
        Err(Error::new(format!(
            "expected {id} {status}, got {got_id} {got_status}"
        )))
    }
}

fn expect_report(frame: &Value, connection_retries: u64) -> Result<(), Error> {
    if frame.get("type").and_then(Value::as_str) != Some("report_at_finish") {
        return Err(Error::new(format!(
            "expected report_at_finish, got {frame}"
        )));
    }
    let got = frame
        .pointer("/data/connection_retries")
        .and_then(Value::as_u64);
    if got != Some(connection_retries) {
        Err(Error::new(format!(
            "connection_retries {got:?}, expected {connection_retries}"
        )))
    } else {
        Ok(())
    }
}

fn connect_metadata_is_json(uri: &str) -> bool {
    let Ok(parsed) = url::Url::parse(&format!("ws://conformance.invalid{uri}")) else {
        return false;
    };
    parsed
        .query_pairs()
        .find(|(key, _)| key == "metadata")
        .is_some_and(|(_, value)| serde_json::from_str::<Value>(&value).is_ok())
}

async fn read_http(mut stream: TcpStream) -> Result<CapturedHttp, Error> {
    let captured = read_http_request(&mut stream).await?;
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await?;
    Ok(captured)
}

async fn read_http_request(stream: &mut TcpStream) -> Result<CapturedHttp, Error> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(Error::new("eof during fragment headers"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 1_048_576 {
            return Err(Error::new("fragment headers too large"));
        }
    };
    let header = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut body = buf[header_end + 4..].to_vec();
    let length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(body.len());
    while body.len() < length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(length);
    Ok(CapturedHttp { header, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_must_be_json_and_pid_zero_is_not_signalled() {
        assert!(connect_metadata_is_json(
            "/transport/websocket?metadata=%7B%22ci%22%3A%22conformance%22%7D"
        ));
        assert!(!connect_metadata_is_json("/transport/websocket?api_key=k"));
        assert!(!connect_metadata_is_json(
            "/transport/websocket?metadata=not-json"
        ));
        assert!(child_term_target(None).is_err());
        assert!(child_term_target(Some(0)).is_err());
        assert_eq!(child_term_target(Some(42)).unwrap(), 42);
    }

    #[test]
    fn signal_death_is_not_a_normal_exit() {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg("kill -TERM $$")
            .status()
            .unwrap();
        assert!(status.code().is_none());
        let message = format!("exit {status}, expected 1");
        assert!(message.contains("SIGTERM") || message.contains("signal"));
    }
}
