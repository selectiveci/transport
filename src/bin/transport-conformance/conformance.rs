use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::fixtures::FixtureIds;
use crate::reference_client::Via;
use transport::Error;

#[derive(Debug, Deserialize, Serialize)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub requires_record: bool,
    pub steps: Vec<Step>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Step {
    Send { message: Value },
    ExpectManifest { tree: bool },
    ExpectResult { id: String, status: String },
    ExpectUncollected { id: String },
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
    Idle,
    IgnorePongs,
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

fn scenarios() -> Result<Vec<Scenario>, Error> {
    const RAW: &[&str] = &[
        include_str!("../../../conformance/scenarios/happy_two_batches.json"),
        include_str!("../../../conformance/scenarios/remove_failed.json"),
        include_str!("../../../conformance/scenarios/repeated_id.json"),
        include_str!("../../../conformance/scenarios/reconnect.json"),
        include_str!("../../../conformance/scenarios/socket_drop.json"),
        include_str!("../../../conformance/scenarios/close_rejected.json"),
        include_str!("../../../conformance/scenarios/sigterm_after_failure.json"),
        include_str!("../../../conformance/scenarios/test_map_fragment.json"),
        include_str!("../../../conformance/scenarios/queued_before_connected.json"),
        include_str!("../../../conformance/scenarios/unknown_command.json"),
        include_str!("../../../conformance/scenarios/reconnect_exhausted.json"),
        include_str!("../../../conformance/scenarios/overlap_buffered.json"),
        include_str!("../../../conformance/scenarios/upload_stall.json"),
        include_str!("../../../conformance/scenarios/signal_keeps_close_status.json"),
        include_str!("../../../conformance/scenarios/uncollected_id.json"),
        include_str!("../../../conformance/scenarios/remove_failed_mid_batch.json"),
        include_str!("../../../conformance/scenarios/idle_keepalive.json"),
        include_str!("../../../conformance/scenarios/pong_timeout.json"),
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

pub struct ConformanceRun<'a> {
    pub filter: Option<&'a str>,
    pub via: SuiteVia,
    pub client: Option<&'a str>,
    pub fixtures: FixtureIds,
    pub fixture_file: Option<&'a Path>,
    pub transport: Option<&'a Path>,
}

pub async fn run_suite_with(bin: &Path, run: ConformanceRun<'_>) -> Result<(), Error> {
    let pace = conformance_pace();
    let mut failures = Vec::new();
    let mut trials = 0usize;
    for scenario in scenarios()? {
        if run.filter.is_some_and(|name| name != scenario.name) {
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
        let mut scenario = scenario;
        apply_fixtures(&mut scenario, &run.fixtures)?;
        let vias = if run.client.is_some() {
            vec![Via::Direct]
        } else {
            run.via.list()
        };
        for via in vias {
            trials += 1;
            let trial = Trial {
                bin,
                scenario: &scenario,
                via,
                client: run.client,
                fixture_file: run.fixture_file,
                transport: run.transport,
                fixtures: &run.fixtures,
                pace,
            };
            let label = if run.client.is_some() {
                "client"
            } else {
                via_name(via)
            };
            match run_one(&trial).await {
                Ok(Played::Passed) => eprintln!("ok {} {label}", scenario.name),
                Ok(Played::NotApplicable(why)) => eprintln!("n/a {} {label}: {why}", scenario.name),
                Err(err) => failures.push(format!("{} {label}: {err}", scenario.name)),
            }
        }
    }
    if trials == 0 {
        return Err(match run.filter {
            Some(name) => Error::new(format!(
                "no conformance scenarios ran for --scenario {name}"
            )),
            None => Error::new("no conformance scenarios ran"),
        });
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::new(failures.join("\n")))
    }
}

#[derive(Clone, Copy)]
struct Pace {
    ping: Duration,
    pong: Duration,
}

struct Trial<'a> {
    bin: &'a Path,
    scenario: &'a Scenario,
    via: Via,
    client: Option<&'a str>,
    fixture_file: Option<&'a Path>,
    fixtures: &'a FixtureIds,
    transport: Option<&'a Path>,
    pace: Pace,
}

fn conformance_pace() -> Pace {
    Pace {
        ping: Duration::from_millis(env_u64("SELECTIVE_CONFORMANCE_PING_INTERVAL_MS", 200)),
        pong: Duration::from_millis(env_u64("SELECTIVE_CONFORMANCE_PONG_TIMEOUT_MS", 1_000)),
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn idle_duration(ping_interval: Duration) -> Duration {
    let scaled = ping_interval.saturating_mul(3);
    if ping_interval >= Duration::from_secs(20) {
        Duration::from_secs(65).max(scaled)
    } else {
        scaled
    }
}

fn apply_fixtures(scenario: &mut Scenario, fixtures: &FixtureIds) -> Result<(), Error> {
    for step in &mut scenario.steps {
        let mut value = serde_json::to_value(&*step)?;
        rewrite_value(&mut value, fixtures);
        *step = serde_json::from_value(value)?;
    }
    Ok(())
}

fn rewrite_value(value: &mut Value, fixtures: &FixtureIds) {
    match value {
        Value::String(text) => {
            if let Some(mapped) = fixtures.rewrite(text) {
                *text = mapped;
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite_value(item, fixtures);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                rewrite_value(item, fixtures);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn client_command(trial: &Trial<'_>) -> Result<Command, Error> {
    if let Some(line) = trial.client {
        let words = shell_words(line)?;
        let mut command = Command::new(&words[0]);
        command.args(&words[1..]);
        return Ok(command);
    }
    let mut command = Command::new(trial.bin);
    command.arg("client");
    if matches!(trial.via, Via::Stdio) {
        command.arg("--via-stdio");
    }
    Ok(command)
}

fn shell_words(input: &str) -> Result<Vec<String>, Error> {
    let words =
        shell_words::split(input).map_err(|_| Error::new("unclosed quote in --client command"))?;
    if words.is_empty() {
        return Err(Error::new("--client command is empty"));
    }
    Ok(words)
}

fn via_name(via: Via) -> &'static str {
    match via {
        Via::Direct => "direct",
        Via::Stdio => "stdio",
    }
}

async fn run_one(trial: &Trial<'_>) -> Result<Played, Error> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let url = format!(
        "ws://{addr}/transport/websocket?api_key=conformance-key&run_id=run-1&run_attempt=1&runner_id=runner-1&metadata=%7B%22ci%22%3A%22conformance%22%7D"
    );
    let gate = Arc::new(Gate {
        reject_budget: Mutex::new(match trial.scenario.steps.first() {
            Some(Step::RejectConnections { times }) => *times,
            _ => 0,
        }),
        hold_uploads: AtomicBool::new(false),
        upload_held: AtomicBool::new(false),
        upload_notify: Notify::new(),
    });
    let logs = Arc::new(Mutex::new(String::new()));
    let mut command = client_command(trial)?;
    command
        .env("SELECTIVE_TRANSPORT_URL", &url)
        .env("SELECTIVE_API_KEY", "conformance-key")
        .env("SELECTIVE_HOST", format!("ws://{addr}"))
        .env("SELECTIVE_RUN_ID", "run-1")
        .env("SELECTIVE_RUN_ATTEMPT", "1")
        .env("SELECTIVE_RUNNER_ID", "runner-1")
        .env("SELECTIVE_TEST_MAP_UPLOAD_KEY", "conformance-key")
        .env("SELECTIVE_CONFORMANCE_FAST", "1")
        .env(
            "SELECTIVE_WS_PING_INTERVAL_MS",
            trial.pace.ping.as_millis().to_string(),
        )
        .env(
            "SELECTIVE_WS_PONG_TIMEOUT_MS",
            trial.pace.pong.as_millis().to_string(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(path) = trial.fixture_file {
        command.env("SELECTIVE_CONFORMANCE_FIXTURE_IDS", path);
    }
    if let Some(path) = trial.transport {
        command.env("SELECTIVE_TRANSPORT", path);
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
                forward_connection(stream, gate, tx).await;
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
        test_map: None,
        gate,
        fixtures: trial.fixtures.clone(),
        ping_interval: trial.pace.ping,
        ignore_pongs: false,
        pings: 0,
    };
    let outcome = player.play(trial.scenario).await;
    if !matches!(outcome, Ok(Played::Passed)) {
        if let Some(mut child) = player.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
    outcome.map_err(|err| {
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
    Socket(Box<Live>),
    Fragment(CapturedHttp),
    Rejected,
    Failed(Error),
}

struct Gate {
    reject_budget: Mutex<u32>,
    hold_uploads: AtomicBool,
    upload_held: AtomicBool,
    upload_notify: Notify,
}

enum Played {
    Passed,
    NotApplicable(String),
}

struct Player {
    incoming: tokio::sync::mpsc::UnboundedReceiver<Incoming>,
    current: Option<Live>,
    fragments: Vec<CapturedHttp>,
    child: Option<Child>,
    saw_connection: bool,
    test_map: Option<String>,
    gate: Arc<Gate>,
    fixtures: FixtureIds,
    ping_interval: Duration,
    ignore_pongs: bool,
    pings: u32,
}

impl Player {
    async fn play(&mut self, scenario: &Scenario) -> Result<Played, Error> {
        if scenario.requires_record {
            self.ensure_connection().await?;
            let advertised = self.test_map.as_deref().unwrap_or("");
            if advertised != "record" {
                return Ok(Played::NotApplicable(format!(
                    "client advertises test_map={advertised}"
                )));
            }
        }
        for step in &scenario.steps {
            self.step(step)
                .await
                .map_err(|err| Error::new(format!("{}: {err}", scenario.name)))?;
        }
        Ok(Played::Passed)
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
                let expected = self.fixtures.manifest_ids();
                expect_manifest(&frame, *tree, &expected)
            }
            Step::ExpectResult { id, status } => {
                let frame = self.next_frame().await?;
                expect_result(&frame, id, status)
            }
            Step::ExpectUncollected { id } => {
                let frame = self.next_frame().await?;
                expect_uncollected(&frame, id)
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
            Step::Idle => self.idle().await,
            Step::IgnorePongs => {
                self.ignore_pongs = true;
                Ok(())
            }
        }
    }

    async fn idle(&mut self) -> Result<(), Error> {
        let before = self.pings;
        let deadline = Instant::now() + idle_duration(self.ping_interval);
        loop {
            if Instant::now() >= deadline {
                break;
            }
            match self.pump(deadline).await {
                Ok(Some(frame)) => {
                    return Err(Error::new(format!(
                        "application frame during idle: {frame}"
                    )));
                }
                Ok(None) => {}
                Err(err) => return Err(err),
            }
        }
        if self.pings <= before {
            return Err(Error::new("idle connection received no websocket ping"));
        }
        if self
            .current
            .as_ref()
            .is_some_and(|live| live.uri.contains("reconnect=true"))
        {
            return Err(Error::new(
                "idle connection reconnects when pongs are answered",
            ));
        }
        if self.current.is_none() {
            return Err(Error::new("idle connection dropped"));
        }
        Ok(())
    }

    async fn ensure_connection(&mut self) -> Result<(), Error> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.current.is_none() {
            if Instant::now() >= deadline {
                return Err(Error::new("timed out waiting for the client"));
            }
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
            return Ok(None);
        }
        let read_allowed = self.current.is_some() && !self.ignore_pongs;
        let event = tokio::select! {
            next = self.incoming.recv() => Event::Accepted(next),
            message = recv_live(self.current.as_mut()), if read_allowed => Event::Socket(message),
            () = tokio::time::sleep(remaining) => Event::Timeout,
        };
        match event {
            Event::Accepted(Some(Incoming::Socket(socket))) => {
                self.install(*socket)?;
                Ok(None)
            }
            Event::Accepted(Some(Incoming::Fragment(fragment))) => {
                self.fragments.push(fragment);
                Ok(None)
            }
            Event::Accepted(Some(Incoming::Rejected)) => Err(Error::new(
                "handshake was refused outside reject_connections",
            )),
            Event::Accepted(Some(Incoming::Failed(err))) => Err(err),
            Event::Accepted(None) => Err(Error::new("connection listener stopped")),
            Event::Socket(Pump::Frame(value)) => Ok(Some(value)),
            Event::Socket(Pump::Ping) => {
                self.pings = self.pings.saturating_add(1);
                Ok(None)
            }
            Event::Socket(Pump::Closed) => {
                self.current = None;
                Ok(None)
            }
            Event::Socket(Pump::Continue) | Event::Timeout => Ok(None),
        }
    }

    fn install(&mut self, socket: Live) -> Result<(), Error> {
        if !connect_metadata_is_json(&socket.uri) {
            return Err(Error::new(format!(
                "connect metadata is missing or not JSON: {}",
                socket.uri
            )));
        }
        if self.saw_connection {
            self.ignore_pongs = false;
        } else {
            if socket.uri.contains("reconnect=true") {
                return Err(Error::new(
                    "the first connection must not set reconnect=true",
                ));
            }
            self.saw_connection = true;
            self.test_map = query_value(&socket.uri, "test_map");
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
            if let Some(frame) = self.pump(deadline).await? {
                return Err(Error::new(format!(
                    "expected no frame before connected, got {frame}"
                )));
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
            self.pump(deadline).await?;
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
            || query_param(request_line, "run_id") != Some("run-1")
            || query_param(request_line, "run_attempt") != Some("1")
            || query_param(request_line, "runner_id") != Some("runner-1")
        {
            return Err(Error::new(format!(
                "unexpected fragment request line: {request_line}"
            )));
        }
        let mut decoder = flate2::read::GzDecoder::new(captured.body.as_slice());
        let mut plain = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut plain)?;
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
                let index = dep
                    .as_u64()
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or_else(|| Error::new("dep index"))?;
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

fn query_param<'a>(request_line: &'a str, key: &str) -> Option<&'a str> {
    let query = request_line.split_once('?')?.1.split_once(' ')?.0;
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then_some(value)
    })
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

async fn forward_connection(
    stream: TcpStream,
    gate: Arc<Gate>,
    tx: tokio::sync::mpsc::UnboundedSender<Incoming>,
) {
    match classify(stream, &gate).await {
        Ok(incoming) => {
            let _ = tx.send(incoming);
        }
        Err(err) => {
            let _ = tx.send(Incoming::Failed(err));
        }
    }
}

// The handshake callback's error type is fixed by tungstenite.
#[allow(clippy::result_large_err)]
async fn classify(mut stream: TcpStream, gate: &Arc<Gate>) -> Result<Incoming, Error> {
    let mut peeked = [0u8; 4];
    let n = stream.peek(&mut peeked).await.unwrap_or(0);
    if n >= 4 && &peeked == b"POST" {
        if gate.hold_uploads.load(Ordering::SeqCst) {
            let _held = read_http_request(&mut stream).await?;
            gate.upload_held.store(true, Ordering::SeqCst);
            gate.upload_notify.notify_waiters();
            return std::future::pending().await;
        }
        return Ok(Incoming::Fragment(read_http(stream).await?));
    }
    let mut uri = String::new();
    let mut rejected = false;
    let handshake =
        tokio_tungstenite::accept_hdr_async(stream, |request: &Request, response: Response| {
            let reject = {
                let mut left = gate.reject_budget.lock().expect("reject budget");
                let reject = *left > 0;
                *left = left.saturating_sub(1);
                reject
            };
            if reject {
                rejected = true;
                return Err(http::Response::builder()
                    .status(503)
                    .body(Some("Service Unavailable".to_owned()))
                    .expect("503 response"));
            }
            uri = request.uri().to_string();
            Ok(response)
        })
        .await;
    if rejected {
        return Ok(Incoming::Rejected);
    }
    let ws = handshake?;
    Ok(Incoming::Socket(Box::new(Live { uri, ws })))
}

enum Pump {
    Continue,
    Ping,
    Frame(Value),
    Closed,
}

async fn recv_live(live: Option<&mut Live>) -> Pump {
    let Some(live) = live else {
        return std::future::pending().await;
    };
    match live.ws.next().await {
        Some(Ok(Message::Text(text))) => Pump::Frame(
            serde_json::from_str(text.trim()).unwrap_or_else(|_| Value::String(text.clone())),
        ),
        Some(Ok(Message::Ping(payload))) => {
            let _ = live.ws.send(Message::Pong(payload)).await;
            Pump::Ping
        }
        Some(Ok(Message::Close(_)) | Err(_)) | None => Pump::Closed,
        Some(Ok(_)) => Pump::Continue,
    }
}

fn expect_manifest(frame: &Value, tree: bool, expected_ids: &[String]) -> Result<(), Error> {
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
    let expected: Vec<&str> = expected_ids.iter().map(String::as_str).collect();
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

fn repo_relative_backtrace(frame: &str) -> bool {
    let frame = frame.trim().replace('\\', "/");
    if frame.is_empty() || frame.starts_with('/') {
        return false;
    }
    let bytes = frame.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return false;
    }
    ![
        "site-packages/",
        "dist-packages/",
        "node_modules/",
        "vendor/bundle/",
        "vendor/gems/",
        "/gems/",
        ".gem/",
    ]
    .iter()
    .any(|marker| frame.contains(marker))
}

fn expect_result(frame: &Value, id: &str, status: &str) -> Result<(), Error> {
    expect_status(frame, id, status)?;
    if status == "failed" {
        let first = frame
            .pointer("/data/failure_formatted_backtrace")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(Value::as_str)
            .unwrap_or("");
        if !repo_relative_backtrace(first) {
            return Err(Error::new(format!(
                "failure backtrace must be a repo-relative frame: {first}"
            )));
        }
    }
    Ok(())
}

fn expect_uncollected(frame: &Value, id: &str) -> Result<(), Error> {
    expect_status(frame, id, "failed")?;
    match frame.pointer("/data/failure_formatted_backtrace") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Array(items)) if items.is_empty() => Ok(()),
        Some(other) => Err(Error::new(format!(
            "an uncollected id has no frame to report, got {other}"
        ))),
    }
}

fn expect_status(frame: &Value, id: &str, status: &str) -> Result<(), Error> {
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
    if got == Some(connection_retries) {
        Ok(())
    } else {
        Err(Error::new(format!(
            "connection_retries {got:?}, expected {connection_retries}"
        )))
    }
}

fn connect_metadata_is_json(uri: &str) -> bool {
    query_value(uri, "metadata").is_some_and(|value| serde_json::from_str::<Value>(&value).is_ok())
}

fn query_value(uri: &str, key: &str) -> Option<String> {
    url::Url::parse(&format!("ws://conformance.invalid{uri}"))
        .ok()?
        .query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
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

    #[tokio::test]
    async fn a_broken_handshake_is_reported() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let gate = Arc::new(Gate {
            reject_budget: Mutex::new(0),
            hold_uploads: AtomicBool::new(false),
            upload_held: AtomicBool::new(false),
            upload_notify: Notify::new(),
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"GET /not-a-websocket\r\n\r\n")
                .await
                .unwrap();
            let _ = stream.shutdown().await;
        });
        let (stream, _) = listener.accept().await.unwrap();
        forward_connection(stream, gate, tx).await;
        client.await.unwrap();
        match rx.recv().await {
            Some(Incoming::Failed(err)) => assert_ne!(err.to_string(), ""),
            _ => panic!("expected a handshake error"),
        }
    }

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
        assert_eq!(
            query_param(
                "POST /test_maps/fragments?run_id=run-1&run_attempt=1 HTTP/1.1",
                "run_id"
            ),
            Some("run-1")
        );
        assert_eq!(
            query_param("POST /test_maps/fragments?run_id=run-10 HTTP/1.1", "run_id"),
            Some("run-10")
        );
    }

    #[test]
    fn fixture_map_rewrites_scenario_ids_and_the_manifest() {
        let fixtures = FixtureIds::parse(
            r#"{"pass":"spec.ok","fail":"spec.bad","pending":"spec.skip","flaky":"spec.flake"}"#,
        )
        .unwrap();
        let mut scenario: Scenario = serde_json::from_str(include_str!(
            "../../../conformance/scenarios/happy_two_batches.json"
        ))
        .unwrap();
        apply_fixtures(&mut scenario, &fixtures).unwrap();
        let encoded = serde_json::to_string(&scenario).unwrap();
        assert!(encoded.contains("spec.ok"));
        assert!(encoded.contains("spec.skip"));
        assert!(!encoded.contains("case-pass"));
        assert!(!encoded.contains("case-pend"));
        let frame = serde_json::json!({
            "type": "test_manifest",
            "data": {
                "test_cases": [
                    {"id": "spec.ok"},
                    {"id": "spec.bad"},
                    {"id": "spec.skip"},
                    {"id": "spec.flake"}
                ]
            }
        });
        expect_manifest(&frame, false, &fixtures.manifest_ids()).unwrap();
        let defaults = FixtureIds::default();
        expect_manifest(
            &serde_json::json!({
                "type": "test_manifest",
                "data": {
                    "test_cases": [
                        {"id": "case-pass"},
                        {"id": "case-fail"},
                        {"id": "case-pend"},
                        {"id": "case-flake"}
                    ]
                }
            }),
            false,
            &defaults.manifest_ids(),
        )
        .unwrap();
    }

    #[test]
    fn client_command_is_split_without_adding_via_args() {
        assert_eq!(
            shell_words(r"/opt/bin/transport conformance-client").unwrap(),
            vec!["/opt/bin/transport", "conformance-client"]
        );
        assert_eq!(
            shell_words(r#"say "two words" 'and more'"#).unwrap(),
            vec!["say", "two words", "and more"]
        );
        assert_eq!(shell_words(r"'a\b'").unwrap(), vec![r"a\b"]);
        assert!(shell_words("  ").is_err());
        assert!(shell_words("'unterminated").is_err());
        assert!(idle_duration(Duration::from_secs(20)) > Duration::from_secs(60));
        assert_eq!(
            idle_duration(Duration::from_millis(200)),
            Duration::from_millis(600)
        );
    }

    #[test]
    fn failure_backtrace_accepts_any_repo_relative_frame() {
        for frame in [
            "tests/example.py:12: in case-fail",
            "test/widget_test.rb:8",
            "spec/models/user_spec.rb:15",
            "src/test/java/com/example/Foo.java:10",
            "lib/my_app.rb",
            "./tests/foo.rb",
            r"src\test\java\com\example\Foo.java:10",
        ] {
            assert!(repo_relative_backtrace(frame), "{frame}");
            let value = serde_json::json!({
                "type": "test_case_result",
                "data": {
                    "id": "case-fail",
                    "status": "failed",
                    "failure_formatted_backtrace": [frame]
                }
            });
            expect_result(&value, "case-fail", "failed").unwrap();
        }
        for frame in [
            "",
            "   ",
            "/home/dev/proj/tests/foo.rb",
            r"C:\proj\src\test\Foo.java:10",
            "C:/proj/src/test/Foo.java:10",
            r"\\server\share\tests\foo.rb",
            "lib/python3.11/site-packages/foo.py",
            "usr/lib/python3/dist-packages/foo.py",
            "node_modules/leftpad/index.js",
            "vendor/bundle/ruby/3.2.0/gems/foo/lib/foo.rb",
            "vendor/gems/foo/lib/foo.rb",
            "home/.gem/ruby/3.2.0/gems/foo/lib/foo.rb",
            r"lib\site-packages\foo.py",
            r"vendor\bundle/x",
        ] {
            assert!(!repo_relative_backtrace(frame), "{frame}");
            let value = serde_json::json!({
                "type": "test_case_result",
                "data": {
                    "id": "case-fail",
                    "status": "failed",
                    "failure_formatted_backtrace": [frame]
                }
            });
            let err = expect_result(&value, "case-fail", "failed").unwrap_err();
            assert!(err.to_string().contains("repo-relative"), "{frame}: {err}");
        }
        expect_result(
            &serde_json::json!({
                "type": "test_case_result",
                "data": {"id": "case-pass", "status": "passed"}
            }),
            "case-pass",
            "passed",
        )
        .unwrap();
    }

    #[test]
    fn an_uncollected_id_reports_no_frame() {
        let result = |frames: Option<Value>| {
            let mut data = serde_json::json!({"id": "not-a-case", "status": "failed"});
            if let Some(frames) = frames {
                data["failure_formatted_backtrace"] = frames;
            }
            serde_json::json!({"type": "test_case_result", "data": data})
        };
        expect_uncollected(&result(None), "not-a-case").unwrap();
        expect_uncollected(&result(Some(serde_json::json!([]))), "not-a-case").unwrap();
        for frames in [
            serde_json::json!(["tests/uncollected.py:1"]),
            serde_json::json!(["/home/dev/proj/tests/foo.py:1"]),
            serde_json::json!("tests/uncollected.py:1"),
        ] {
            let err = expect_uncollected(&result(Some(frames.clone())), "not-a-case").unwrap_err();
            assert!(err.to_string().contains("no frame"), "{frames}: {err}");
        }
    }
}
