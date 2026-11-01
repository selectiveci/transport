use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::timeout;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_transport")
}

async fn within<F: std::future::Future>(future: F) -> F::Output {
    timeout(Duration::from_secs(5), future)
        .await
        .expect("timed out after 5s")
}

#[cfg(unix)]
#[tokio::test]
async fn fifo_round_trip_keeps_the_legacy_framing() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (id, input, output) = fifo_paths();
    let mut child = Command::new(bin())
        .arg(format!("ws://{addr}"))
        .arg(&id)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (writer, reader) = open_fifos(&input, &output);
    let (sock, _) = within(listener.accept()).await.unwrap();
    let mut ws = accept_async(sock).await.unwrap();
    let mut to_transport = writer.await.unwrap();
    let mut from_transport = BufReader::new(reader.await.unwrap());
    to_transport
        .write_all(b"{\"type\":\"ping\"}\n")
        .await
        .unwrap();
    to_transport.flush().await.unwrap();
    let frame = within(ws.next()).await.unwrap().unwrap();
    match frame {
        Message::Text(text) => assert_eq!(text, "{\"type\":\"ping\"}\n"),
        other => panic!("unexpected {other:?}"),
    }
    ws.send(Message::Text("{\"command\":\"connected\"}".into()))
        .await
        .unwrap();
    let mut line = String::new();
    within(from_transport.read_line(&mut line)).await.unwrap();
    assert_eq!(line, "{\"command\":\"connected\"}\n");
    to_transport.write_all(b"  exit  \n").await.unwrap();
    to_transport.flush().await.unwrap();
    let status = within(child.wait()).await.unwrap();
    assert!(status.success());
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
}

#[cfg(unix)]
fn open_fifos(
    input: &str,
    output: &str,
) -> (
    tokio::task::JoinHandle<tokio::fs::File>,
    tokio::task::JoinHandle<tokio::fs::File>,
) {
    let input_path = input.to_owned();
    let output_path = output.to_owned();
    let writer = tokio::spawn(async move {
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(input_path)
            .await
            .unwrap()
    });
    let reader = tokio::spawn(async move {
        tokio::fs::OpenOptions::new()
            .read(true)
            .open(output_path)
            .await
            .unwrap()
    });
    (writer, reader)
}

#[cfg(unix)]
fn fifo_paths() -> (String, String, String) {
    let id = format!(
        "seltest{}{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let input = format!("/tmp/{id}_1");
    let output = format!("/tmp/{id}_2");
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    assert!(std::process::Command::new("mkfifo")
        .arg(&input)
        .status()
        .unwrap()
        .success());
    assert!(std::process::Command::new("mkfifo")
        .arg(&output)
        .status()
        .unwrap()
        .success());
    (id, input, output)
}

#[cfg(unix)]
#[tokio::test]
async fn fifo_exits_when_connect_is_refused_while_the_input_pipe_stays_open() {
    let (id, input, output) = fifo_paths();
    let mut child = Command::new(bin())
        .arg("ws://127.0.0.1:1")
        .arg(&id)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (writer, reader) = open_fifos(&input, &output);
    let _held_input = within(writer).await.unwrap();
    let _held_output = within(reader).await.unwrap();
    let status = timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("fifo connect failure waited on the input pipe")
        .unwrap();
    assert_eq!(status.code(), Some(1));
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
}

#[test]
fn build_env_and_snapshot_commands() {
    let dir = std::env::temp_dir().join(format!("transport-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let output = std::process::Command::new(bin())
        .arg("--build-env")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("SELECTIVE_PLATFORM", "github_actions")
        .env("SELECTIVE_GIT_REPO", "acme/widgets")
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["platform"], "github_actions");
    assert_eq!(json["git_repo_full_name"], "acme/widgets");
    assert_eq!(json["host"], "wss://app.selective.ci");

    let snap = std::process::Command::new(bin())
        .arg("--snapshot")
        .arg(&dir)
        .output()
        .unwrap();
    assert_eq!(snap.status.code(), Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn upload_fragment_posts_the_stdin_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = transport::gzip_bytes(br#"{"version":1}"#).unwrap();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: upload-key"));
        assert!(request.contains("run_id=run-9"));
        sock.write_all(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    });
    let mut child = Command::new(bin())
        .arg("--upload-fragment")
        .env("SELECTIVE_HOST", format!("ws://{addr}"))
        .env("SELECTIVE_TEST_MAP_UPLOAD_KEY", "upload-key")
        .env("SELECTIVE_RUN_ID", "run-9")
        .env("SELECTIVE_RUN_ATTEMPT", "3")
        .env("SELECTIVE_RUNNER_ID", "shard")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&body).await.unwrap();
    let status = timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "upload failed");
    server.await.unwrap();
}

#[tokio::test]
async fn upload_fragment_uses_detected_run_identity() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = transport::gzip_bytes(br#"{"version":1}"#).unwrap();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        assert!(request.contains("run_id=999"));
        assert!(request.contains("run_attempt=2"));
        assert!(request.contains("runner_id=shard"));
        sock.write_all(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    });
    let mut child = Command::new(bin())
        .arg("--upload-fragment")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_RUN_ID", "999")
        .env("GITHUB_RUN_ATTEMPT", "2")
        .env("SELECTIVE_RUNNER_ID", "shard")
        .env("SELECTIVE_HOST", format!("ws://{addr}"))
        .env("SELECTIVE_TEST_MAP_UPLOAD_KEY", "upload-key")
        .env_remove("SELECTIVE_RUN_ID")
        .env_remove("SELECTIVE_RUN_ATTEMPT")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&body).await.unwrap();
    let status = timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "upload failed");
    server.await.unwrap();
}

#[tokio::test]
async fn upload_fragment_flags_and_stdin_payload_override_detection() {
    async fn post(args: &[&str], stdin: &[u8], expect: &str) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let expect = expect.to_owned();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n = sock.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            sock.write_all(
                b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            assert!(
                request.contains(&expect),
                "request missing {expect}: {request}"
            );
            assert!(!request.contains("detected-run"), "{request}");
        });
        let mut child = Command::new(bin())
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("GITHUB_ACTIONS", "true")
            .env("GITHUB_RUN_ID", "detected-run")
            .env("GITHUB_RUN_ATTEMPT", "9")
            .env("SELECTIVE_RUNNER_ID", "detected-runner")
            .env("SELECTIVE_HOST", format!("ws://{addr}"))
            .env("SELECTIVE_TEST_MAP_UPLOAD_KEY", "upload-key")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).await.unwrap();
        let status = timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "upload failed");
        server.await.unwrap();
    }

    let gzip = transport::gzip_bytes(br#"{"version":1}"#).unwrap();
    post(
        &[
            "--upload-fragment",
            "--run-id",
            "session-run",
            "--run-attempt",
            "7",
            "--runner-id",
            "session-runner",
        ],
        &gzip,
        "run_id=session-run",
    )
    .await;
    let mut payload =
        br#"{"run_id":"payload-run","run_attempt":4,"runner_id":"payload-runner"}"#.to_vec();
    payload.push(b'\n');
    payload.extend_from_slice(&gzip);
    post(&["--upload-fragment"], &payload, "run_id=payload-run").await;
}
