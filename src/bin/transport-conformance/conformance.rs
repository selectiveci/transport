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
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::fixtures::FixtureIds;
use crate::reference_client::Via;
use transport::Error;

const API_KEY: &str = "conformance-key";
const RUN_ID: &str = "run-1";
const RUN_ATTEMPT: &str = "1";
const RUNNER_ID: &str = "runner-1";

#[derive(Debug, Deserialize, Serialize)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub requires_record: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_for_languages: Vec<String>,
    #[serde(default)]
    pub requires_hook_time: bool,
    #[serde(default)]
    pub no_keepalive_env: bool,
    pub steps: Vec<Step>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Step {
    Send {
        message: Value,
    },
    SendRaw {
        text: String,
    },
    ExpectManifest {
        tree: bool,
    },
    ExpectResult {
        id: String,
        status: String,
    },
    ExpectUncollected {
        id: String,
    },
    ExpectTerminating {
        signal: String,
    },
    Drop,
    ExpectReconnect,
    ExpectFragment {
        ids: Vec<String>,
    },
    ExpectReport {
        connection_retries: u64,
        #[serde(default)]
        hook_time: bool,
    },
    ExpectNoFrame {
        ms: u64,
    },
    ArmHoldUpload,
    WaitUpload,
    SignalTerm,
    ExpectExit {
        code: i32,
    },
    RejectConnections {
        times: u32,
    },
    Idle,
    IgnorePongs,
    ExpectOutput {
        text: String,
    },
    Busy {
        ms: u64,
    },
    CloseSocket {
        #[serde(default)]
        code: Option<u16>,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum SuiteVia {
    Direct,
    Stdio,
    Both,
    All,
}

impl SuiteVia {
    fn list(self) -> Vec<Via> {
        match self {
            Self::Direct => vec![Via::Direct],
            Self::Stdio => vec![Via::Stdio],
            Self::Both | Self::All => vec![Via::Direct, Via::Stdio],
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
        include_str!("../../../conformance/scenarios/print_commands.json"),
        include_str!("../../../conformance/scenarios/busy_server.json"),
        include_str!("../../../conformance/scenarios/close_frame.json"),
        include_str!("../../../conformance/scenarios/reconnect_then_going_away.json"),
        include_str!("../../../conformance/scenarios/retries_across_reconnects.json"),
        include_str!("../../../conformance/scenarios/retries_after_rejected_connects.json"),
        include_str!("../../../conformance/scenarios/group_hook_time.json"),
        include_str!("../../../conformance/scenarios/unparseable_frame.json"),
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
    let before_all_seconds = run
        .fixtures
        .before_all_seconds()
        .or(run.client.is_none().then_some(BUILTIN_BEFORE_ALL_SECONDS));
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
            let trial = Trial {
                bin,
                scenario: &scenario,
                via,
                client: run.client,
                fixture_file: run.fixture_file,
                transport: run.transport,
                fixtures: &run.fixtures,
                before_all_seconds,
                pace,
            };
            let label = if run.client.is_some() {
                "client"
            } else {
                via_name(via)
            };
            match run_one(&trial).await {
                Ok(Played::Passed) => {
                    trials += 1;
                    eprintln!("ok {} {label}", scenario.name);
                }
                Ok(Played::NotApplicable(why)) => {
                    eprintln!("n/a {} {label}: {why}", scenario.name);
                }
                Err(err) => {
                    trials += 1;
                    failures.push(format!("{} {label}: {err}", scenario.name));
                }
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
    before_all_seconds: Option<f64>,
    transport: Option<&'a Path>,
    pace: Pace,
}

const BUILTIN_BEFORE_ALL_SECONDS: f64 = 0.2;

const RUNNER_ENV: &[(&str, &str)] = &[
    ("SELECTIVE_PLATFORM", "conformance"),
    ("SELECTIVE_BRANCH", "conformance"),
    ("SELECTIVE_SHA", "0123456789abcdef0123456789abcdef01234567"),
    ("SELECTIVE_GIT_REPO", "selectiveci/conformance"),
    ("SELECTIVE_GIT_PROVIDER", "github"),
    ("SELECTIVE_PR_TITLE", "conformance"),
];

fn runner_env_defaults(var: impl Fn(&str) -> Option<String>) -> Vec<(&'static str, &'static str)> {
    RUNNER_ENV
        .iter()
        .copied()
        .filter(|(key, _)| var(key).is_none_or(|value| value.is_empty()))
        .collect()
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

fn idle_duration(ping_interval: Duration, pong_timeout: Duration) -> Duration {
    ping_interval.saturating_mul(2).saturating_add(pong_timeout)
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
    match trial.via {
        Via::Direct => {}
        Via::Stdio => {
            command.arg("--via-stdio");
        }
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

fn not_applicable(trial: &Trial<'_>) -> Option<&'static str> {
    if trial.scenario.requires_hook_time && trial.before_all_seconds.is_none() {
        return Some("the fixture file declares no before_all_seconds");
    }
    None
}

fn client_env(command: &mut Command, trial: &Trial<'_>, url: &str, host: &str) {
    command
        .env("SELECTIVE_TRANSPORT_URL", url)
        .env("SELECTIVE_API_KEY", API_KEY)
        .env("SELECTIVE_HOST", host)
        .env("SELECTIVE_RUN_ID", RUN_ID)
        .env("SELECTIVE_RUN_ATTEMPT", RUN_ATTEMPT)
        .env("SELECTIVE_RUNNER_ID", RUNNER_ID)
        .env("SELECTIVE_TEST_MAP_UPLOAD_KEY", "conformance-key")
        .env("SELECTIVE_CONFORMANCE_FAST", "1");
    if trial.scenario.no_keepalive_env {
        command
            .env_remove("SELECTIVE_WS_PING_INTERVAL_MS")
            .env_remove("SELECTIVE_WS_PONG_TIMEOUT_MS");
    } else {
        command
            .env(
                "SELECTIVE_WS_PING_INTERVAL_MS",
                trial.pace.ping.as_millis().to_string(),
            )
            .env(
                "SELECTIVE_WS_PONG_TIMEOUT_MS",
                trial.pace.pong.as_millis().to_string(),
            );
    }
    if let Some(path) = trial.fixture_file {
        command.env("SELECTIVE_CONFORMANCE_FIXTURE_IDS", path);
    }
    if let (None, Some(seconds)) = (trial.client, trial.before_all_seconds) {
        command.env(
            "SELECTIVE_CONFORMANCE_BEFORE_ALL_SECONDS",
            seconds.to_string(),
        );
    }
    if let Some(path) = trial.transport {
        command.env("SELECTIVE_TRANSPORT", path);
    }
    command.envs(runner_env_defaults(|key| std::env::var(key).ok()));
}

async fn run_one(trial: &Trial<'_>) -> Result<Played, Error> {
    if let Some(why) = not_applicable(trial) {
        return Ok(Played::NotApplicable(why.to_owned()));
    }
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let url = format!(
        "ws://{addr}/transport/websocket?api_key={API_KEY}&run_id={RUN_ID}&run_attempt={RUN_ATTEMPT}&runner_id={RUNNER_ID}&metadata=%7B%22ci%22%3A%22conformance%22%7D"
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
    client_env(&mut command, trial, &url, &format!("ws://{addr}"));
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
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
        logs: Arc::clone(&logs),
        saw_connection: false,
        attempts: 0,
        test_map: None,
        language: None,
        gate,
        fixtures: trial.fixtures.clone(),
        before_all_seconds: trial.before_all_seconds,
        connections: 0,
        ping_interval: trial.pace.ping,
        pong_timeout: trial.pace.pong,
        reading: Reading::Open,
        pings: 0,
        frames_end_in_newline: trial.client.is_none() && !matches!(trial.via, Via::Direct),
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
    Rejected(String),
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

enum Reading {
    Open,
    UntilReconnect,
    Busy,
}

struct Player {
    incoming: tokio::sync::mpsc::UnboundedReceiver<Incoming>,
    current: Option<Live>,
    fragments: Vec<CapturedHttp>,
    child: Option<Child>,
    logs: Arc<Mutex<String>>,
    saw_connection: bool,
    attempts: u32,
    test_map: Option<String>,
    language: Option<String>,
    gate: Arc<Gate>,
    fixtures: FixtureIds,
    before_all_seconds: Option<f64>,
    connections: u32,
    ping_interval: Duration,
    pong_timeout: Duration,
    reading: Reading,
    pings: u32,
    frames_end_in_newline: bool,
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
        if !scenario.not_for_languages.is_empty() {
            self.ensure_connection().await?;
            let language = self.language.as_deref().unwrap_or("");
            if scenario
                .not_for_languages
                .iter()
                .any(|excluded| excluded == language)
            {
                return Ok(Played::NotApplicable(format!(
                    "not run for language={language} yet"
                )));
            }
        }
        let wants_tree = scenario
            .steps
            .iter()
            .any(|step| matches!(step, Step::ExpectManifest { tree: true }));
        if wants_tree && !inside_git_checkout() {
            return Err(Error::new(format!(
                "{} needs a git checkout: run transport-conformance from inside the client's repository",
                scenario.name
            )));
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
            Step::SendRaw { text } => {
                self.ensure_connection().await?;
                let live = self
                    .current
                    .as_mut()
                    .ok_or_else(|| Error::new("no websocket"))?;
                live.ws.send(Message::Text(text.clone())).await?;
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
            Step::ExpectReport {
                connection_retries,
                hook_time,
            } => {
                let frame = self.next_frame().await?;
                expect_report(&frame, *connection_retries)?;
                match (hook_time, self.before_all_seconds) {
                    (true, Some(seconds)) => expect_hook_time(&frame, seconds),
                    _ => Ok(()),
                }
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
                self.reading = Reading::UntilReconnect;
                Ok(())
            }
            Step::ExpectOutput { text } => self.expect_output(text).await,
            Step::Busy { ms } => self.busy_for(*ms).await,
            Step::CloseSocket { code } => self.close_socket(*code).await,
        }
    }

    async fn busy_for(&mut self, ms: u64) -> Result<(), Error> {
        let connections = self.connections;
        let deadline = Instant::now() + Duration::from_millis(ms);
        let before = std::mem::replace(&mut self.reading, Reading::Busy);
        let outcome = loop {
            if Instant::now() >= deadline {
                break Ok(());
            }
            if let Err(err) = self.pump(deadline).await {
                break Err(err);
            }
            if self.connections != connections {
                break Err(Error::new(
                    "the client reconnected while the server was busy",
                ));
            }
        };
        self.reading = before;
        outcome
    }

    async fn close_socket(&mut self, code: Option<u16>) -> Result<(), Error> {
        let mut live = self
            .current
            .take()
            .ok_or_else(|| Error::new("no websocket to close"))?;
        let frame = code.map(|code| CloseFrame {
            code: CloseCode::from(code),
            reason: "".into(),
        });
        live.ws.send(Message::Close(frame)).await?;
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(Ok(message)) = live.ws.next().await {
                    if matches!(message, Message::Close(_)) {
                        break;
                    }
                }
            })
            .await;
        });
        Ok(())
    }

    async fn expect_output(&mut self, text: &str) -> Result<(), Error> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let printed = self.logs.lock().is_ok_and(|logs| logs.contains(text));
            if printed {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::new(format!("client output never showed {text:?}")));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn idle(&mut self) -> Result<(), Error> {
        let before = self.pings;
        let deadline = Instant::now() + idle_duration(self.ping_interval, self.pong_timeout);
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
        let read_allowed = self.current.is_some() && matches!(self.reading, Reading::Open);
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
            Event::Accepted(Some(Incoming::Rejected(_))) => Err(Error::new(
                "handshake was refused outside reject_connections",
            )),
            Event::Accepted(Some(Incoming::Failed(err))) => Err(err),
            Event::Accepted(None) => Err(Error::new("connection listener stopped")),
            Event::Socket(Pump::Frame(value, newline)) => {
                if self.frames_end_in_newline && !newline {
                    return Err(Error::new(format!(
                        "a frame through the transport lost its trailing \\n: {value}"
                    )));
                }
                Ok(Some(value))
            }
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

    fn check_handshake(&mut self, uri: &str) -> Result<(), Error> {
        expect_identity(&handshake_query(uri))?;
        if self.attempts == 0 && uri.contains("reconnect=true") {
            return Err(Error::new(
                "the first connection must not set reconnect=true",
            ));
        }
        self.attempts += 1;
        Ok(())
    }

    fn install(&mut self, socket: Live) -> Result<(), Error> {
        self.check_handshake(&socket.uri)?;
        if !connect_metadata_is_json(&socket.uri) {
            return Err(Error::new(format!(
                "connect metadata is missing or not JSON: {}",
                socket.uri
            )));
        }
        if self.saw_connection {
            self.reading = Reading::Open;
        } else {
            self.saw_connection = true;
            self.test_map = query_value(&socket.uri, "test_map");
            self.language = query_value(&socket.uri, "language");
        }
        self.connections += 1;
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
        if !request_line.starts_with("POST /test_maps/fragments?")
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
            let Incoming::Rejected(uri) = next else {
                return Err(Error::new("expected the handshake to be refused with 503"));
            };
            self.check_handshake(&uri)?;
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
            uri = request.uri().to_string();
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
            Ok(response)
        })
        .await;
    if rejected {
        return Ok(Incoming::Rejected(uri));
    }
    let ws = handshake?;
    Ok(Incoming::Socket(Box::new(Live { uri, ws })))
}

enum Pump {
    Continue,
    Ping,
    Frame(Value, bool),
    Closed,
}

async fn recv_live(live: Option<&mut Live>) -> Pump {
    let Some(live) = live else {
        return std::future::pending().await;
    };
    match live.ws.next().await {
        Some(Ok(Message::Text(text))) => Pump::Frame(
            serde_json::from_str(text.trim())
                .unwrap_or_else(|_| Value::String(text.trim().to_owned())),
            text.ends_with('\n'),
        ),
        Some(Ok(Message::Ping(payload))) => {
            let _ = live.ws.send(Message::Pong(payload)).await;
            Pump::Ping
        }
        Some(Ok(Message::Close(_)) | Err(_)) | None => Pump::Closed,
        Some(Ok(_)) => Pump::Continue,
    }
}

fn handshake_query(uri: &str) -> Vec<(String, String)> {
    let query = uri.split_once('?').map_or("", |(_, query)| query);
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

fn expect_identity(query: &[(String, String)]) -> Result<(), Error> {
    let identity = [
        ("api_key", API_KEY),
        ("run_id", RUN_ID),
        ("run_attempt", RUN_ATTEMPT),
        ("runner_id", RUNNER_ID),
    ];
    for (name, expected) in identity {
        let values: Vec<&str> = query
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .collect();
        if values != [expected] {
            return Err(Error::new(format!(
                "the websocket url must carry {name}={expected}, got {values:?}"
            )));
        }
    }
    Ok(())
}

fn expect_manifest(frame: &Value, tree: bool, expected_ids: &[String]) -> Result<(), Error> {
    if frame.get("type").and_then(Value::as_str) != Some("test_manifest") {
        return Err(Error::new(format!("expected test_manifest, got {frame}")));
    }
    let cases = frame
        .pointer("/data/test_cases")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::new("manifest test_cases"))?;
    for case in cases {
        expect_manifest_entry(case)?;
    }
    let ids: Vec<&str> = cases
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

fn expect_manifest_entry(case: &Value) -> Result<(), Error> {
    let id = case.get("id").and_then(Value::as_str).unwrap_or("?");
    if !case.get("run_time").is_some_and(Value::is_number) {
        return Err(Error::new(format!(
            "manifest entry {id} needs a numeric run_time"
        )));
    }
    let file_path = case.get("file_path").and_then(Value::as_str).unwrap_or("");
    if file_path.trim().is_empty() || is_absolute_path(file_path) {
        return Err(Error::new(format!(
            "manifest entry {id} needs a repo-relative file_path, got {file_path:?}"
        )));
    }
    Ok(())
}

fn is_absolute_path(path: &str) -> bool {
    let path = path.trim().replace('\\', "/");
    let bytes = path.as_bytes();
    path.starts_with('/')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

fn inside_git_checkout() -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| output.status.success() && output.stdout.starts_with(b"true"))
}

fn repo_relative_backtrace(frame: &str) -> bool {
    let frame = frame.trim().replace('\\', "/");
    if frame.is_empty() || is_absolute_path(&frame) {
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

fn expect_hook_time(frame: &Value, declared: f64) -> Result<(), Error> {
    let total: f64 = frame
        .get("data")
        .and_then(Value::as_object)
        .map_or(0.0, |data| {
            data.iter()
                .filter(|(key, _)| key.starts_with("seconds_in_") && key.ends_with("_hooks"))
                .filter_map(|(_, value)| value.as_f64())
                .sum()
        });
    if total >= declared * 0.5 && total <= declared * 5.0 + 1.0 {
        Ok(())
    } else {
        Err(Error::new(format!(
            "group hooks took {total}s by the report, but the fixture's before-all hook sleeps {declared}s"
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
        let manifest = |ids: &[&str]| {
            let cases: Vec<Value> = ids
                .iter()
                .map(|id| serde_json::json!({"id": id, "file_path": "spec/a_spec.rb", "run_time": 0}))
                .collect();
            serde_json::json!({"type": "test_manifest", "data": {"test_cases": cases}})
        };
        expect_manifest(
            &manifest(&["spec.ok", "spec.bad", "spec.skip", "spec.flake"]),
            false,
            &fixtures.manifest_ids(),
        )
        .unwrap();
        expect_manifest(
            &manifest(&["case-pass", "case-fail", "case-pend", "case-flake"]),
            false,
            &FixtureIds::default().manifest_ids(),
        )
        .unwrap();
    }

    #[test]
    fn identity_must_match_what_the_client_was_given() {
        let url = format!(
            "/transport/websocket?api_key={API_KEY}&run_id={RUN_ID}&run_attempt={RUN_ATTEMPT}&runner_id={RUNNER_ID}&reconnect=true"
        );
        expect_identity(&handshake_query(&url)).unwrap();
        for dropped in ["api_key", "run_id", "run_attempt", "runner_id"] {
            let stripped: Vec<String> = url
                .split_once('?')
                .unwrap()
                .1
                .split('&')
                .filter(|pair| !pair.starts_with(dropped))
                .map(str::to_owned)
                .collect();
            let err = expect_identity(&handshake_query(&format!("/x?{}", stripped.join("&"))))
                .unwrap_err()
                .to_string();
            assert!(err.contains(dropped), "{err}");
        }
        let swapped = url.replace("run_attempt=1", "run_attempt=2");
        assert!(expect_identity(&handshake_query(&swapped)).is_err());
        let doubled = format!("{url}&runner_id=other");
        assert!(expect_identity(&handshake_query(&doubled)).is_err());
    }

    #[test]
    fn every_manifest_entry_needs_run_time_and_a_repo_relative_file_path() {
        let check = |case: Value| expect_manifest_entry(&case).map_err(|err| err.to_string());
        check(serde_json::json!({"id": "a", "file_path": "tests/a.py", "run_time": 0.0})).unwrap();
        check(serde_json::json!({"id": "a", "file_path": "./spec/a_spec.rb", "run_time": 3}))
            .unwrap();
        for (case, want) in [
            (
                serde_json::json!({"id": "a", "file_path": "tests/a.py"}),
                "run_time",
            ),
            (
                serde_json::json!({"id": "a", "file_path": "tests/a.py", "run_time": "0.1"}),
                "run_time",
            ),
            (
                serde_json::json!({"id": "a", "file_path": "tests/a.py", "run_time": null}),
                "run_time",
            ),
            (serde_json::json!({"id": "a", "run_time": 0.1}), "file_path"),
            (
                serde_json::json!({"id": "a", "file_path": " ", "run_time": 0.1}),
                "file_path",
            ),
            (
                serde_json::json!({"id": "a", "file_path": "/repo/tests/a.py", "run_time": 0.1}),
                "file_path",
            ),
            (
                serde_json::json!({"id": "a", "file_path": r"C:\repo\a.py", "run_time": 0.1}),
                "file_path",
            ),
        ] {
            let err = check(case.clone()).unwrap_err();
            assert!(err.contains(want), "{case}: {err}");
        }
    }

    #[test]
    fn the_runner_fills_only_the_build_env_the_caller_left_unset() {
        let all = runner_env_defaults(|_| None);
        let keys: Vec<&str> = all.iter().map(|(key, _)| *key).collect();
        for key in [
            "SELECTIVE_PLATFORM",
            "SELECTIVE_BRANCH",
            "SELECTIVE_SHA",
            "SELECTIVE_GIT_REPO",
            "SELECTIVE_GIT_PROVIDER",
            "SELECTIVE_PR_TITLE",
        ] {
            assert!(keys.contains(&key), "{key}");
        }
        assert!(all.iter().all(|(_, value)| !value.is_empty()));
        let kept = runner_env_defaults(|key| match key {
            "SELECTIVE_BRANCH" => Some("main".to_owned()),
            "SELECTIVE_SHA" => Some(String::new()),
            _ => None,
        });
        assert!(!kept.iter().any(|(key, _)| *key == "SELECTIVE_BRANCH"));
        assert!(kept.iter().any(|(key, _)| *key == "SELECTIVE_SHA"));
    }

    #[test]
    fn all_runs_direct_and_stdio() {
        assert_eq!(SuiteVia::All.list(), vec![Via::Direct, Via::Stdio]);
        assert_eq!(SuiteVia::Both.list(), vec![Via::Direct, Via::Stdio]);
    }

    #[test]
    fn hook_time_must_be_on_the_scale_of_the_declared_sleep() {
        let report = |data: Value| serde_json::json!({"type": "report_at_finish", "data": data});
        for data in [
            serde_json::json!({"seconds_in_before_all_hooks": 0.21}),
            serde_json::json!({"seconds_in_beforeclass_all_hooks": 0.15, "seconds_in_afterclass_all_hooks": 0.0}),
            serde_json::json!({"seconds_in_setup_all_all_hooks": 0.4, "connection_retries": 3}),
        ] {
            expect_hook_time(&report(data.clone()), 0.2)
                .unwrap_or_else(|err| panic!("{data}: {err}"));
        }
        for data in [
            serde_json::json!({}),
            serde_json::json!({"seconds_in_before_all_hooks": 0.0}),
            serde_json::json!({"seconds_in_before_all_hooks": "0.2"}),
            serde_json::json!({"seconds_in_before_all_hooks": 200}),
            serde_json::json!({"before_all": 0.2}),
        ] {
            assert!(
                expect_hook_time(&report(data.clone()), 0.2).is_err(),
                "{data}"
            );
        }
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
        assert_eq!(
            idle_duration(Duration::from_millis(200), Duration::from_secs(1)),
            Duration::from_millis(1400)
        );
        assert!(
            idle_duration(Duration::from_secs(20), Duration::from_secs(45))
                > Duration::from_secs(65)
        );
    }

    #[test]
    fn keepalive_applies_on_every_via() {
        let fixtures = FixtureIds::default();
        for name in ["idle_keepalive", "pong_timeout"] {
            let scenario = scenarios()
                .unwrap()
                .into_iter()
                .find(|scenario| scenario.name == name)
                .unwrap();
            for (via, client) in [
                (Via::Direct, None),
                (Via::Stdio, None),
                (Via::Direct, Some("ruby client.rb")),
            ] {
                let trial = Trial {
                    bin: Path::new("transport-conformance"),
                    scenario: &scenario,
                    via,
                    client,
                    fixture_file: None,
                    fixtures: &fixtures,
                    before_all_seconds: None,
                    transport: None,
                    pace: conformance_pace(),
                };
                assert!(
                    not_applicable(&trial).is_none(),
                    "{name} {via:?} {client:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_run_where_every_trial_is_not_applicable_is_an_error() {
        let run = ConformanceRun {
            filter: Some("group_hook_time"),
            via: SuiteVia::Direct,
            client: Some("true"),
            fixtures: FixtureIds::default(),
            fixture_file: None,
            transport: None,
        };
        let err = run_suite_with(Path::new("transport-conformance"), run)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no conformance scenarios ran"),
            "{err}"
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
