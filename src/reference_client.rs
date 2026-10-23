use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc::UnboundedReceiver, watch};
use url::Url;

use crate::error::Error;
use crate::fixtures::FixtureIds;
use crate::session::{poll_frames, recv_incoming, PollFrame, Session};
use crate::snapshot::snapshot_tree;
use crate::upload::{gzip_bytes, upload_fragment, FragmentUpload};

const FILE_PATH: &str = "tests/example.py";

struct Fixture {
    id: String,
    status: &'static str,
    line: i64,
    flake: bool,
}

fn load_fixtures() -> Result<Vec<Fixture>, Error> {
    let ids = match std::env::var("SELECTIVE_CONFORMANCE_FIXTURE_IDS") {
        Ok(path) if !path.is_empty() => FixtureIds::load(Path::new(&path))?,
        _ => FixtureIds::default(),
    };
    Ok([
        ("pass", "passed", 1, false),
        ("fail", "failed", 2, false),
        ("pending", "pending", 3, false),
        ("flaky", "failed", 4, true),
    ]
    .into_iter()
    .map(|(role, status, line, flake)| Fixture {
        id: ids.id(role),
        status,
        line,
        flake,
    })
    .collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    Direct,
    Stdio,
}

pub struct RunOpts {
    pub url: String,
    pub via: Via,
    pub catch_signals: bool,
}

pub async fn run(opts: RunOpts) -> i32 {
    let (tx, rx) = watch::channel(None);
    if opts.catch_signals {
        tokio::spawn(async move {
            if let Some(signal) = wait_for_signal().await {
                let _ = tx.send(Some(signal));
            }
        });
    }
    let fixtures = match load_fixtures() {
        Ok(fixtures) => fixtures,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };
    let mut state = State {
        fixtures,
        ..State::default()
    };
    let mut failures: u32 = 0;
    let mut reconnect = false;
    loop {
        if let Some(signal) = signal_now(&rx) {
            return finish_signal(&mut Link::Closed, &state, signal, None).await;
        }
        let url = if reconnect {
            match with_reconnect(&opts.url) {
                Ok(url) => url,
                Err(err) => {
                    eprintln!("{err}");
                    return 1;
                }
            }
        } else {
            opts.url.clone()
        };
        let mut link = match Link::open(&url, opts.via).await {
            Ok(link) => link,
            Err(err) => {
                failures += 1;
                eprintln!("connect failed ({failures}): {err}");
                if failures > 10 {
                    return 1;
                }
                tokio::time::sleep(retry_delay(failures, fast_retries())).await;
                reconnect = true;
                continue;
            }
        };
        match drive(&mut link, &mut state, &opts.url, &rx).await {
            Drive::Finished(code) => return code,
            Drive::Signal(signal, exit_status) => {
                return finish_signal(&mut link, &state, signal, exit_status).await;
            }
            Drive::Lost => {
                failures += 1;
                state.connection_retries = failures;
                if failures > 10 {
                    eprintln!("giving up after {failures} connection failures");
                    return 1;
                }
                tokio::time::sleep(retry_delay(failures, fast_retries())).await;
                reconnect = true;
            }
            Drive::Reconnect => {
                reconnect = true;
            }
        }
    }
}

pub fn retry_delay(failures: u32, fast: bool) -> Duration {
    if fast {
        Duration::from_millis(0)
    } else {
        Duration::from_secs(u64::from(failures.min(4)))
    }
}

pub const TERMINATION_GRACE: Duration = Duration::from_millis(250);

fn fast_retries() -> bool {
    std::env::var("SELECTIVE_CONFORMANCE_FAST").ok().as_deref() == Some("1")
}

#[derive(Default)]
struct State {
    fixtures: Vec<Fixture>,
    record: bool,
    tree: bool,
    connection_retries: u32,
    attempts: HashMap<String, u32>,
    results: HashMap<String, String>,
    traced: BTreeSet<String>,
}

enum Drive {
    Finished(i32),
    Lost,
    Reconnect,
    Signal(&'static str, Option<i32>),
}

async fn drive(
    link: &mut Link,
    state: &mut State,
    base_url: &str,
    signals: &watch::Receiver<Option<&'static str>>,
) -> Drive {
    let mut queued = VecDeque::new();
    match wait_connected(link, &mut queued, signals).await {
        Ok(()) => {}
        Err(WaitFail::Lost) => return Drive::Lost,
        Err(WaitFail::Timeout) => return Drive::Finished(1),
        Err(WaitFail::Signal(signal)) => return Drive::Signal(signal, None),
    }
    loop {
        let command = if let Some(command) = queued.pop_front() {
            command
        } else {
            match recv_frame(link, signals).await {
                FrameIn::Value(value) => value,
                FrameIn::Lost => return Drive::Lost,
                FrameIn::Signal(signal) => return Drive::Signal(signal, None),
            }
        };
        match dispatch(link, state, base_url, signals, command, &mut queued).await {
            Action::Continue => {}
            Action::Lost => return Drive::Lost,
            Action::Reconnect => return Drive::Reconnect,
            Action::Exit(code) => return Drive::Finished(code),
            Action::Signal(signal, exit_status) => return Drive::Signal(signal, exit_status),
        }
    }
}

enum WaitFail {
    Lost,
    Timeout,
    Signal(&'static str),
}

async fn wait_connected(
    link: &mut Link,
    queued: &mut VecDeque<Value>,
    signals: &watch::Receiver<Option<&'static str>>,
) -> Result<(), WaitFail> {
    let deadline = tokio::time::Instant::now() + CONNECTED_TIMEOUT;
    loop {
        if let Some(signal) = signal_now(signals) {
            return Err(WaitFail::Signal(signal));
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            eprintln!("timed out waiting for connected");
            return Err(WaitFail::Timeout);
        }
        match tokio::time::timeout(remaining, recv_frame(link, signals)).await {
            Ok(FrameIn::Value(value)) => {
                if command_name(&value) == "connected" {
                    return Ok(());
                }
                queued.push_back(value);
            }
            Ok(FrameIn::Lost) => return Err(WaitFail::Lost),
            Ok(FrameIn::Signal(signal)) => return Err(WaitFail::Signal(signal)),
            Err(_) => {
                eprintln!("timed out waiting for connected");
                return Err(WaitFail::Timeout);
            }
        }
    }
}

enum Action {
    Continue,
    Lost,
    Reconnect,
    Exit(i32),
    Signal(&'static str, Option<i32>),
}

async fn dispatch(
    link: &mut Link,
    state: &mut State,
    base_url: &str,
    signals: &watch::Receiver<Option<&'static str>>,
    command: Value,
    queued: &mut VecDeque<Value>,
) -> Action {
    match command_name(&command) {
        "configure_test_map" => {
            if command.get("record").and_then(Value::as_bool) == Some(true) {
                state.record = true;
            }
            if command.get("tree").and_then(Value::as_bool) == Some(true) {
                state.tree = true;
            }
            Action::Continue
        }
        "test_manifest" => match send_json(link, &manifest(state)).await {
            Ok(()) => Action::Continue,
            Err(_) => Action::Lost,
        },
        "run_test_cases" => {
            let ids = command
                .get("test_case_ids")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for id in ids {
                let Some(id) = id.as_str() else {
                    continue;
                };
                let result = result_message(state, id);
                if send_json(link, &result).await.is_err() {
                    return Action::Lost;
                }
                remember(
                    state,
                    id,
                    result["data"]["status"].as_str().unwrap_or("passed"),
                );
                match drain(link, queued) {
                    Action::Continue => {}
                    other => return other,
                }
            }
            Action::Continue
        }
        "remove_failed_test_case_result" => {
            if let Some(id) = command.get("test_case_id").and_then(Value::as_str) {
                state.results.remove(id);
            }
            Action::Continue
        }
        "reconnect" => Action::Reconnect,
        "close" => close_session(link, state, base_url, signals, &command).await,
        "print_notice" | "print_message" => {
            if let Some(message) = command.get("message").and_then(Value::as_str) {
                println!("{message}");
            }
            Action::Continue
        }
        _ => Action::Continue,
    }
}

fn drain(link: &mut Link, queued: &mut VecDeque<Value>) -> Action {
    drain_polled(|| link.poll(), queued)
}

fn drain_polled(
    mut poll: impl FnMut() -> Result<PollFrame, Error>,
    queued: &mut VecDeque<Value>,
) -> Action {
    loop {
        match poll() {
            Ok(PollFrame::Empty) => return Action::Continue,
            Ok(PollFrame::Closed) => return Action::Lost,
            Ok(PollFrame::Frame(frame)) => {
                if let Some(value) = parse_frame(&frame) {
                    queued.push_back(value);
                }
            }
            Err(_) => return Action::Lost,
        }
    }
}

async fn close_session(
    link: &mut Link,
    state: &mut State,
    base_url: &str,
    signals: &watch::Receiver<Option<&'static str>>,
    command: &Value,
) -> Action {
    if let Some(message) = command.get("message").and_then(Value::as_str) {
        if !message.is_empty() {
            println!("{message}");
        }
    }
    let override_status = command
        .get("exit_status")
        .and_then(Value::as_i64)
        .map(|code| code as i32);
    let report = json!({
        "type": "report_at_finish",
        "data": {
            "connection_retries": state.connection_retries
        }
    });
    if send_json(link, &report).await.is_err() {
        return Action::Lost;
    }
    if state.record && !state.traced.is_empty() {
        if let Some(signal) = signal_now(signals) {
            return Action::Signal(signal, override_status);
        }
        let mut signals = signals.clone();
        tokio::select! {
            biased;
            Some(signal) = next_signal(&mut signals) => {
                return Action::Signal(signal, override_status);
            }
            result = upload_traces(state, base_url) => {
                if let Err(err) = result {
                    eprintln!("Selective could not upload this runner's test map ({err}). Its tests will run on the next build rather than be skipped.");
                }
            }
        }
    }
    let _ = link.close().await;
    Action::Exit(override_status.unwrap_or_else(|| framework_status(state)))
}

async fn finish_signal(
    link: &mut Link,
    state: &State,
    signal: &'static str,
    exit_status: Option<i32>,
) -> i32 {
    let frame = json!({"type": "terminating", "data": {"signal": signal}});
    let _ = send_json(link, &frame).await;
    tokio::time::sleep(TERMINATION_GRACE).await;
    let _ = link.close().await;
    exit_status.unwrap_or_else(|| framework_status(state))
}

fn framework_status(state: &State) -> i32 {
    if state.results.values().any(|status| status == "failed") {
        1
    } else {
        0
    }
}

fn remember(state: &mut State, id: &str, status: &str) {
    state.results.insert(id.to_owned(), status.to_owned());
    if state.record {
        state.traced.insert(id.to_owned());
    }
}

fn manifest(state: &State) -> Value {
    let test_cases: Vec<Value> = state
        .fixtures
        .iter()
        .map(|case| {
            json!({
                "id": case.id,
                "file_path": FILE_PATH,
                "line_number": case.line,
                "description": case.id,
                "full_description": case.id,
                "run_time": 0.0
            })
        })
        .collect();
    let mut data = json!({ "test_cases": test_cases });
    if state.tree {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        if let Ok(Some(tree)) = snapshot_tree(&cwd) {
            if let Ok(value) = serde_json::to_value(tree) {
                data["test_map_tree"] = value;
            }
        }
    }
    json!({"type": "test_manifest", "data": data})
}

fn result_message(state: &mut State, id: &str) -> Value {
    let attempt = state.attempts.entry(id.to_owned()).or_insert(0);
    *attempt += 1;
    let attempt = *attempt;
    let fixture = state.fixtures.iter().find(|case| case.id == id);
    let (status, line, failure_message) = match fixture {
        Some(case) => (
            status_for(case.status, case.flake, attempt),
            case.line,
            "assertion failed",
        ),
        None => ("failed".to_owned(), 1, "not collected"),
    };
    let mut data = json!({
        "id": clean(id),
        "description": clean(id),
        "full_description": clean(id),
        "status": status,
        "file_path": FILE_PATH,
        "line_number": line,
        "run_time": 0.01,
        "pending_message": if status == "pending" { json!("skipped") } else { Value::Null }
    });
    if status == "failed" {
        data["failure_message_lines"] = json!([clean(failure_message)]);
        data["failure_formatted_backtrace"] = json!([format!("{FILE_PATH}:{line}: in {id}")]);
    }
    json!({"type": "test_case_result", "data": data})
}

pub fn status_for(base: &str, flake: bool, attempt: u32) -> String {
    if flake && attempt >= 2 {
        "passed".to_owned()
    } else {
        base.to_owned()
    }
}

fn clean(value: &str) -> String {
    value.replace('\0', "")
}

async fn upload_traces(state: &State, base_url: &str) -> Result<(), Error> {
    let fragment = json!({
        "version": 1,
        "files": [FILE_PATH],
        "tests": state.traced.iter().map(|id| json!({"id": id, "deps": [0]})).collect::<Vec<_>>()
    });
    let gzip_body = gzip_bytes(serde_json::to_vec(&fragment)?.as_slice())?;
    upload_fragment(&FragmentUpload {
        host: base_url.to_owned(),
        api_key: query_param(base_url, "api_key"),
        run_id: query_param(base_url, "run_id"),
        run_attempt: query_param(base_url, "run_attempt"),
        runner_id: query_param(base_url, "runner_id"),
        gzip_body,
    })
    .await
}

fn command_name(value: &Value) -> &str {
    value.get("command").and_then(Value::as_str).unwrap_or("")
}

fn parse_frame(frame: &str) -> Option<Value> {
    serde_json::from_str(frame.trim()).ok()
}

async fn send_json(link: &mut Link, value: &Value) -> Result<(), Error> {
    let frame = serde_json::to_string(value)?;
    link.send(&frame).await
}

enum FrameIn {
    Value(Value),
    Lost,
    Signal(&'static str),
}

async fn recv_frame(link: &mut Link, signals: &watch::Receiver<Option<&'static str>>) -> FrameIn {
    if let Some(signal) = signal_now(signals) {
        return FrameIn::Signal(signal);
    }
    let mut signals = signals.clone();
    tokio::select! {
        biased;
        Some(signal) = next_signal(&mut signals) => FrameIn::Signal(signal),
        incoming = link.recv() => match incoming {
            Ok(Some(frame)) => parse_frame(&frame).map(FrameIn::Value).unwrap_or(FrameIn::Lost),
            Ok(None) => FrameIn::Lost,
            Err(_) => FrameIn::Lost,
        },
    }
}

async fn next_signal(signals: &mut watch::Receiver<Option<&'static str>>) -> Option<&'static str> {
    signals
        .wait_for(Option::is_some)
        .await
        .ok()
        .and_then(|signal| *signal)
}

fn signal_now(signals: &watch::Receiver<Option<&'static str>>) -> Option<&'static str> {
    *signals.borrow()
}

const CONNECTED_TIMEOUT: Duration = Duration::from_secs(30);

fn with_reconnect(url: &str) -> Result<String, Error> {
    let mut parsed = Url::parse(url)?;
    let pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(key, _)| key != "reconnect")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .chain(std::iter::once(("reconnect".to_owned(), "true".to_owned())))
        .collect();
    parsed.query_pairs_mut().clear().extend_pairs(&pairs);
    Ok(parsed.into())
}

fn query_param(url: &str, key: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .query_pairs()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.into_owned())
        })
        .unwrap_or_default()
}

enum Link {
    Direct(Session),
    Stdio(StdioLink),
    Closed,
}

impl Link {
    async fn open(url: &str, via: Via) -> Result<Self, Error> {
        match via {
            Via::Direct => Ok(Self::Direct(Session::connect(url).await?)),
            Via::Stdio => Ok(Self::Stdio(StdioLink::spawn(url)?)),
        }
    }

    async fn send(&mut self, frame: &str) -> Result<(), Error> {
        match self {
            Self::Direct(session) => session.send(frame).await,
            Self::Stdio(child) => child.send(frame).await,
            Self::Closed => Err(Error::new("session closed")),
        }
    }

    async fn recv(&mut self) -> Result<Option<String>, Error> {
        match self {
            Self::Direct(session) => session.recv().await,
            Self::Stdio(child) => child.recv().await,
            Self::Closed => Ok(None),
        }
    }

    fn poll(&mut self) -> Result<PollFrame, Error> {
        match self {
            Self::Direct(session) => session.poll(),
            Self::Stdio(child) => child.poll(),
            Self::Closed => Ok(PollFrame::Closed),
        }
    }

    async fn close(&mut self) -> Result<(), Error> {
        match self {
            Self::Direct(session) => session.close().await,
            Self::Stdio(child) => child.close().await,
            Self::Closed => Ok(()),
        }
    }
}

struct StdioLink {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    incoming: UnboundedReceiver<Result<Option<String>, Error>>,
}

impl StdioLink {
    fn spawn(url: &str) -> Result<Self, Error> {
        let bin = std::env::current_exe().map_err(Error::from)?;
        let mut child = Command::new(bin)
            .arg("--stdio")
            .env("SELECTIVE_TRANSPORT_URL", url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::new("stdio transport has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::new("stdio transport has no stdout"))?;
        let (tx, incoming) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match std::io::BufRead::read_line(&mut reader, &mut line) {
                    Ok(0) => {
                        let _ = tx.send(Ok(None));
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\n', '\r']).to_owned();
                        if tx.send(Ok(Some(trimmed))).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(Error::from(err)));
                        break;
                    }
                }
            }
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            incoming,
        })
    }

    async fn send(&mut self, frame: &str) -> Result<(), Error> {
        use std::io::Write;
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| Error::new("session closed"))?;
        writeln!(stdin, "{frame}")?;
        stdin.flush()?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<String>, Error> {
        recv_incoming(&mut self.incoming).await
    }

    fn poll(&mut self) -> Result<PollFrame, Error> {
        poll_frames(&mut self.incoming)
    }

    async fn close(&mut self) -> Result<(), Error> {
        use std::io::Write;
        if let Some(mut stdin) = self.stdin.take() {
            let _ = writeln!(stdin, "exit");
            let _ = stdin.flush();
            drop(stdin);
        }
        let _ = self.child.wait();
        Ok(())
    }
}

impl Drop for StdioLink {
    fn drop(&mut self) {
        if self.stdin.is_some() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
async fn wait_for_signal() -> Option<&'static str> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).ok()?;
    let mut interrupt = signal(SignalKind::interrupt()).ok()?;
    tokio::select! {
        _ = term.recv() => Some("TERM"),
        _ = interrupt.recv() => Some("INT"),
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() -> Option<&'static str> {
    std::future::pending().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_and_flake_status_match_the_ruby_loop() {
        assert_eq!(retry_delay(1, false), Duration::from_secs(1));
        assert_eq!(retry_delay(4, false), Duration::from_secs(4));
        assert_eq!(retry_delay(9, false), Duration::from_secs(4));
        assert_eq!(retry_delay(3, true), Duration::from_millis(0));
        assert_eq!(TERMINATION_GRACE, Duration::from_millis(250));
        assert_eq!(status_for("failed", true, 1), "failed");
        assert_eq!(status_for("failed", true, 2), "passed");
        assert_eq!(status_for("failed", false, 2), "failed");
        assert_eq!(clean("a\0b"), "ab");
        let mut state = State::default();
        let message = result_message(&mut state, "missing-case");
        assert_eq!(message["data"]["status"], "failed");
        assert_eq!(message["data"]["failure_message_lines"][0], "not collected");
    }

    #[test]
    fn a_remove_drained_mid_batch_stays_queued() {
        let mut state = State::default();
        remember(&mut state, "case-fail", "failed");
        let mut frames = VecDeque::from([
            Ok(PollFrame::Frame(
                r#"{"command":"remove_failed_test_case_result","test_case_id":"case-fail"}"#
                    .to_owned(),
            )),
            Ok(PollFrame::Empty),
        ]);
        let mut queued = VecDeque::new();
        let action = drain_polled(|| frames.pop_front().unwrap(), &mut queued);
        assert!(matches!(action, Action::Continue));
        remember(&mut state, "case-fail", "failed");
        assert_eq!(state.results["case-fail"], "failed");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["test_case_id"], "case-fail");
    }
}
