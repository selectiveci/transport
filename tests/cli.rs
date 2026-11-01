use std::time::{Duration, Instant};

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

fn conformance_bin() -> &'static str {
    env!("CARGO_BIN_EXE_transport-conformance")
}

async fn within<F: std::future::Future>(future: F) -> F::Output {
    timeout(Duration::from_secs(5), future)
        .await
        .expect("timed out after 5s")
}

#[tokio::test]
async fn stdio_uses_the_env_url_and_does_not_forward_exit() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut child = Command::new(bin())
        .arg("--stdio")
        .env(
            "SELECTIVE_TRANSPORT_URL",
            format!("ws://{addr}/transport/websocket?api_key=secret"),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (sock, _) = within(listener.accept()).await.unwrap();
    let mut ws = accept_async(sock).await.unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"{\"type\":\"test_case_result\"}\n")
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let frame = within(ws.next()).await.unwrap().unwrap();
    match frame {
        Message::Text(text) => assert_eq!(text, "{\"type\":\"test_case_result\"}\n"),
        other => panic!("unexpected {other:?}"),
    }
    ws.send(Message::Text("{\"command\":\"connected\"}".into()))
        .await
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    within(stdout.read_line(&mut line)).await.unwrap();
    assert_eq!(line, "{\"command\":\"connected\"}\n");
    stdin.write_all(b"exit\n").await.unwrap();
    stdin.flush().await.unwrap();
    let extra = timeout(Duration::from_millis(300), ws.next()).await;
    if let Ok(Some(Ok(Message::Text(text)))) = extra {
        assert_ne!(text.trim(), "exit");
    }
    let status = within(child.wait()).await.unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn stdio_reads_the_url_from_stdin_when_env_is_absent() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut child = Command::new(bin())
        .arg("--stdio")
        .env_remove("SELECTIVE_TRANSPORT_URL")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let url = format!("ws://{addr}/transport/websocket?api_key=from-stdin\n");
    stdin.write_all(url.as_bytes()).await.unwrap();
    stdin.write_all(b"{\"type\":\"hello\"}\n").await.unwrap();
    stdin.flush().await.unwrap();
    let (sock, _) = within(listener.accept()).await.unwrap();
    let mut ws = accept_async(sock).await.unwrap();
    let frame = within(ws.next()).await.unwrap().unwrap();
    match frame {
        Message::Text(text) => assert_eq!(text, "{\"type\":\"hello\"}\n"),
        other => panic!("unexpected {other:?}"),
    }
    stdin.write_all(b"exit\n").await.unwrap();
    let status = within(child.wait()).await.unwrap();
    assert!(status.success());
}

async fn stdio_ending(
    env: &[(&str, &str)],
    server: impl FnOnce(
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> tokio::task::JoinHandle<()>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut command = Command::new(bin());
    command
        .arg("--stdio")
        .env("SELECTIVE_TRANSPORT_URL", format!("ws://{addr}"))
        .env_remove("SELECTIVE_WS_PING_INTERVAL_MS")
        .env_remove("SELECTIVE_WS_PONG_TIMEOUT_MS")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    let _stdin = child.stdin.take();
    let (sock, _) = within(listener.accept()).await.unwrap();
    let handle = server(accept_async(sock).await.unwrap());
    let status = timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("the transport did not exit")
        .unwrap();
    handle.abort();
    assert_eq!(status.code(), Some(0));
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .await
        .unwrap();
    stderr
}

#[tokio::test]
async fn stdio_reports_each_way_a_connection_ends() {
    let closed = stdio_ending(&[], |mut ws| {
        tokio::spawn(async move {
            ws.send(Message::Close(None)).await.unwrap();
            let _ = ws.next().await;
        })
    })
    .await;
    assert!(
        closed.contains("websocket closed by the server"),
        "{closed}"
    );

    let dropped = stdio_ending(&[], |ws| tokio::spawn(async move { drop(ws) })).await;
    assert!(
        dropped.contains("websocket connection dropped"),
        "{dropped}"
    );

    let silent = stdio_ending(
        &[
            ("SELECTIVE_WS_PING_INTERVAL_MS", "100"),
            ("SELECTIVE_WS_PONG_TIMEOUT_MS", "100"),
        ],
        |ws| {
            tokio::spawn(async move {
                let _held = ws;
                std::future::pending::<()>().await;
            })
        },
    )
    .await;
    assert!(silent.contains("websocket keepalive timeout"), "{silent}");
}

#[tokio::test]
async fn stdio_sends_no_ping_unless_the_client_asks() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut child = Command::new(bin())
        .arg("--stdio")
        .env("SELECTIVE_TRANSPORT_URL", format!("ws://{addr}"))
        .env_remove("SELECTIVE_WS_PING_INTERVAL_MS")
        .env_remove("SELECTIVE_WS_PONG_TIMEOUT_MS")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (sock, _) = within(listener.accept()).await.unwrap();
    let mut ws = accept_async(sock).await.unwrap();
    let early = timeout(Duration::from_secs(1), ws.next()).await;
    assert!(early.is_err(), "expected nothing, got {early:?}");
    assert!(child.try_wait().unwrap().is_none());
}

#[tokio::test]
async fn missing_stdio_url_exits_1() {
    let mut child = Command::new(bin())
        .arg("--stdio")
        .env_remove("SELECTIVE_TRANSPORT_URL")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    let status = within(child.wait()).await.unwrap();
    assert_eq!(status.code(), Some(1));
}

async fn stdio_failure(env_url: Option<&str>, stdin: &[u8]) -> (Option<i32>, String) {
    let mut command = Command::new(bin());
    command
        .arg("--stdio")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    match env_url {
        Some(url) => command.env("SELECTIVE_TRANSPORT_URL", url),
        None => command.env_remove("SELECTIVE_TRANSPORT_URL"),
    };
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(stdin).await.unwrap();
    drop(input);
    let output = within(child.wait_with_output()).await.unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn stdio_never_prints_the_api_key_when_the_url_is_unusable() {
    let key = "KEY-5f3c9a17";
    let bad_port = format!("ws://127.0.0.1:${{PORT}}/transport/websocket?api_key={key}");
    let mut invalid_utf8 = format!("ws://127.0.0.1:1/?api_key={key}").into_bytes();
    invalid_utf8.extend_from_slice(&[0xff, b'\n']);

    let cases = [
        ("env bad port", stdio_failure(Some(&bad_port), b"").await),
        (
            "stdin bad port",
            stdio_failure(None, format!("{bad_port}\n").as_bytes()).await,
        ),
        (
            "stdin invalid utf-8",
            stdio_failure(None, &invalid_utf8).await,
        ),
    ];
    let mut problems = Vec::new();
    for (name, (code, stderr)) in cases {
        if code != Some(1) || stderr.trim().is_empty() {
            problems.push(format!("{name}: exit {code:?}, stderr {stderr:?}"));
        }
        if stderr.contains(key) || stderr.contains("[119") {
            problems.push(format!("{name} leaked the key: {stderr}"));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[tokio::test]
async fn stdio_exits_1_when_the_handshake_is_never_answered() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let held = tokio::spawn(async move {
        let mut sockets = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            sockets.push(socket);
        }
    });
    let mut child = Command::new(bin())
        .arg("--stdio")
        .env(
            "SELECTIVE_TRANSPORT_URL",
            format!("ws://{addr}/?api_key=secret"),
        )
        .env_remove("SELECTIVE_WS_PING_INTERVAL_MS")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take();
    let started = Instant::now();
    let status = timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("the transport is still waiting for the handshake")
        .unwrap();
    held.abort();
    assert_eq!(status.code(), Some(1));
    assert!(started.elapsed() >= Duration::from_secs(9));
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

#[cfg(unix)]
#[tokio::test]
async fn fifo_never_pings_even_when_keepalive_is_configured() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (id, input, output) = fifo_paths();
    let mut child = Command::new(bin())
        .arg(format!("ws://{addr}"))
        .arg(&id)
        .env("SELECTIVE_WS_PING_INTERVAL_MS", "100")
        .env("SELECTIVE_WS_PONG_TIMEOUT_MS", "100")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (writer, reader) = open_fifos(&input, &output);
    let (sock, _) = within(listener.accept()).await.unwrap();
    let mut ws = accept_async(sock).await.unwrap();
    let mut to_transport = writer.await.unwrap();
    let _held_output = reader.await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "fifo exited while the server was silent"
    );
    to_transport
        .write_all(b"{\"type\":\"after\"}\n")
        .await
        .unwrap();
    let frame = within(ws.next()).await.unwrap().unwrap();
    assert_eq!(frame, Message::Text("{\"type\":\"after\"}\n".into()));
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
async fn blocking_session_round_trip() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(sock).await.unwrap();
        let frame = ws.next().await.unwrap().unwrap();
        match frame {
            Message::Text(text) => assert_eq!(text, "hello"),
            other => panic!("{other:?}"),
        }
        ws.send(Message::Ping(b"xyz".to_vec())).await.unwrap();
        let pong = ws.next().await.unwrap().unwrap();
        match pong {
            Message::Pong(payload) => assert_eq!(payload, b"xyz"),
            other => panic!("{other:?}"),
        }
        ws.send(Message::Text("{\"command\":\"connected\"}".into()))
            .await
            .unwrap();
        let _ = ws.next().await;
    });
    let session = tokio::task::spawn_blocking(move || {
        let session = transport::BlockingSession::connect(&format!("ws://{addr}")).unwrap();
        session.send("hello").unwrap();
        let frame = session
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(frame, "{\"command\":\"connected\"}");
        session.close().unwrap();
    });
    session.await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn blocking_session_connect_returns_when_the_handshake_stalls() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(sock);
    });
    let started = Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        transport::BlockingSession::connect(&format!("ws://{addr}"))
    })
    .await
    .unwrap();
    match result {
        Ok(_) => panic!("connect returned a session"),
        Err(err) => assert!(err.to_string().contains("timed out"), "{err}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "connect blocked for {:?}",
        started.elapsed()
    );
    server.abort();
}

#[tokio::test]
async fn blocking_session_drop_returns_when_the_peer_stops_reading() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let ws = accept_async(sock).await.unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(ws);
    });
    let client = tokio::task::spawn_blocking(move || {
        let session = transport::BlockingSession::connect(&format!("ws://{addr}")).unwrap();
        let chunk = "x".repeat(256 * 1024);
        let send_started = Instant::now();
        let mut blocked_send = false;
        for _ in 0..64 {
            if session.send(&chunk).is_err() {
                blocked_send = true;
                break;
            }
        }
        let send_elapsed = send_started.elapsed();
        let drop_started = Instant::now();
        drop(session);
        (blocked_send, send_elapsed, drop_started.elapsed())
    });
    let (blocked_send, send_elapsed, drop_elapsed) = timeout(Duration::from_secs(45), client)
        .await
        .expect("drop did not return")
        .unwrap();
    assert!(
        blocked_send,
        "peer never filled the socket window ({send_elapsed:?})"
    );
    assert!(
        send_elapsed < Duration::from_secs(20),
        "send blocked for {send_elapsed:?}"
    );
    assert!(
        drop_elapsed < Duration::from_secs(20),
        "drop blocked for {drop_elapsed:?}"
    );
    server.abort();
}

fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "transport-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_executable(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}

#[tokio::test]
async fn conformance_unknown_scenario_exits_nonzero() {
    for args in [
        vec!["run", "--scenario", "no_such_scenario"],
        vec![
            "run",
            "--scenario",
            "no_such_scenario",
            "--client",
            "/bin/false",
        ],
    ] {
        let output = Command::new(conformance_bin())
            .args(&args)
            .output()
            .await
            .unwrap();
        assert!(
            !output.status.success(),
            "args {args:?} stdout {}\nstderr {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("no conformance scenarios ran") && stderr.contains("no_such_scenario"),
            "args {args:?} stderr {stderr}"
        );
    }
}

#[tokio::test]
async fn conformance_run_client_drives_an_external_command() {
    let dir = temp_dir("external-client");
    let marker = dir.join("marker");
    let script = dir.join("client.sh");
    write_executable(
        &script,
        "#!/bin/sh\nprintf '%s\\n' invoked >> \"$1\"\nprintf '%s\\n' \"$SELECTIVE_TRANSPORT_URL\" >> \"$1\"\nprintf '%s\\n' \"$SELECTIVE_API_KEY\" >> \"$1\"\nprintf '%s\\n' \"$SELECTIVE_HOST\" >> \"$1\"\nprintf '%s\\n' \"$SELECTIVE_RUN_ID\" >> \"$1\"\nprintf '%s\\n' \"$SELECTIVE_RUN_ATTEMPT\" >> \"$1\"\nprintf '%s\\n' \"$SELECTIVE_RUNNER_ID\" >> \"$1\"\nprintf '%s\\n' \"$SELECTIVE_TEST_MAP_UPLOAD_KEY\" >> \"$1\"\nexec \"$2\" client\n",
    );
    let command = format!(
        "{} {} {}",
        script.display(),
        marker.display(),
        conformance_bin()
    );
    let output = Command::new(conformance_bin())
        .args([
            "run",
            "--scenario",
            "happy_two_batches",
            "--via",
            "both",
            "--client",
            &command,
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "stdout {}\nstderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let marker_text = std::fs::read_to_string(&marker).unwrap();
    let lines: Vec<&str> = marker_text.lines().collect();
    assert_eq!(lines.iter().filter(|line| **line == "invoked").count(), 1);
    assert!(lines
        .iter()
        .any(|line| line.starts_with("ws://") && line.contains("api_key=conformance-key")));
    assert!(lines.contains(&"conformance-key"));
    assert!(lines.contains(&"run-1"));
    assert!(lines.contains(&"1"));
    assert!(lines.contains(&"runner-1"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn conformance_fixture_ids_round_trip_through_the_builtin_client() {
    let dir = temp_dir("fixture-ids");
    let map = dir.join("ids.json");
    std::fs::write(
        &map,
        r#"{"pass":"spec.ok","fail":"spec.bad","pending":"spec.skip","flaky":"spec.flake"}"#,
    )
    .unwrap();
    let output = Command::new(conformance_bin())
        .args([
            "run",
            "--scenario",
            "happy_two_batches",
            "--via",
            "direct",
            "--fixture-ids",
            map.to_str().unwrap(),
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "stdout {}\nstderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
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

#[tokio::test]
async fn session_pings_while_idle_and_closes_without_a_pong() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(sock).await.unwrap();
        let mut pings = 0u32;
        let deadline = Instant::now() + Duration::from_millis(700);
        while Instant::now() < deadline {
            match timeout(Duration::from_millis(300), ws.next()).await {
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    pings += 1;
                    ws.send(Message::Pong(payload)).await.unwrap();
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                _ => {}
            }
        }
        ws.send(Message::Text("still-open".into())).await.unwrap();
        let _ = ws.next().await;
        pings
    });
    let mut session = transport::Session::connect_with(
        &format!("ws://{addr}"),
        Some(transport::Keepalive {
            ping_interval: Duration::from_millis(150),
            pong_timeout: Duration::from_millis(500),
        }),
    )
    .await
    .unwrap();
    let frame = timeout(Duration::from_secs(3), session.recv())
        .await
        .expect("session closed before the idle text frame")
        .unwrap();
    assert_eq!(frame.as_deref(), Some("still-open"));
    let pings = server.await.unwrap();
    assert!(pings >= 1, "pings {pings}");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let ws = accept_async(sock).await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(ws);
    });
    let started = Instant::now();
    let mut session = transport::Session::connect_with(
        &format!("ws://{addr}"),
        Some(transport::Keepalive {
            ping_interval: Duration::from_millis(80),
            pong_timeout: Duration::from_millis(200),
        }),
    )
    .await
    .unwrap();
    let frame = timeout(Duration::from_secs(2), session.recv())
        .await
        .expect("pong timeout did not close the session")
        .unwrap();
    assert!(frame.is_none());
    assert!(started.elapsed() < Duration::from_secs(2));
    server.abort();
}

#[tokio::test]
async fn session_stays_open_while_the_server_sends_without_ponging() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(sock).await.unwrap();
        for index in 0..10 {
            ws.send(Message::Text(format!("frame-{index}")))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        std::future::pending::<()>().await;
    });
    let mut session = transport::Session::connect_with(
        &format!("ws://{addr}"),
        Some(transport::Keepalive {
            ping_interval: Duration::from_millis(80),
            pong_timeout: Duration::from_millis(200),
        }),
    )
    .await
    .unwrap();
    for index in 0..10 {
        let frame = within(session.recv()).await.unwrap();
        assert_eq!(frame, Some(format!("frame-{index}")));
    }
    let ended = within(session.recv()).await.unwrap();
    assert_eq!(ended, None);
    server.abort();
}

#[tokio::test]
async fn an_early_pong_does_not_stretch_the_dead_peer_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(sock).await.unwrap();
        let mut answered = false;
        while !answered {
            match timeout(Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    ws.send(Message::Pong(payload.clone())).await.unwrap();
                    ws.send(Message::Pong(payload)).await.unwrap();
                    answered = true;
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => return,
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    });
    let started = Instant::now();
    let mut session = transport::Session::connect_with(
        &format!("ws://{addr}"),
        Some(transport::Keepalive {
            ping_interval: Duration::from_millis(500),
            pong_timeout: Duration::from_millis(200),
        }),
    )
    .await
    .unwrap();
    let frame = timeout(Duration::from_millis(1450), session.recv())
        .await
        .expect("stray pong stretched the dead-peer timeout")
        .unwrap();
    assert!(frame.is_none());
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(1450),
        "dead peer took {elapsed:?}"
    );
    server.abort();
}

#[test]
fn scenario_and_via_require_a_value() {
    let help = std::process::Command::new(bin())
        .arg("--help")
        .output()
        .unwrap();
    assert_eq!(help.status.code(), Some(0));
    let bare = std::process::Command::new(bin()).output().unwrap();
    assert_eq!(bare.status.code(), Some(1));
    assert_eq!(bare.stderr, help.stderr);

    let cases = [
        (vec!["run", "--scenario"], "missing value for --scenario\n"),
        (vec!["run", "--via"], "missing value for --via\n"),
        (vec!["run", "--client"], "missing value for --client\n"),
        (
            vec!["run", "--fixture-ids"],
            "missing value for --fixture-ids\n",
        ),
        (vec!["run", "--via", "sideways"], "unknown --via sideways\n"),
        (vec!["client"], "client requires SELECTIVE_TRANSPORT_URL\n"),
    ];
    for (args, stderr) in cases {
        let output = std::process::Command::new(conformance_bin())
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(String::from_utf8_lossy(&output.stderr), stderr, "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}
