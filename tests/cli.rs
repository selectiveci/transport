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
    let (sock, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut ws = accept_async(sock).await.unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"{\"type\":\"test_case_result\"}\n")
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let frame = timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match frame {
        Message::Text(text) => assert_eq!(text, "{\"type\":\"test_case_result\"}"),
        other => panic!("unexpected {other:?}"),
    }
    ws.send(Message::Text("{\"command\":\"connected\"}".into()))
        .await
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    timeout(Duration::from_secs(5), stdout.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(line, "{\"command\":\"connected\"}\n");
    stdin.write_all(b"exit\n").await.unwrap();
    stdin.flush().await.unwrap();
    let extra = timeout(Duration::from_millis(300), ws.next()).await;
    if let Ok(Some(Ok(Message::Text(text)))) = extra {
        assert_ne!(text.trim(), "exit");
    }
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
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
    let (sock, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut ws = accept_async(sock).await.unwrap();
    let frame = timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match frame {
        Message::Text(text) => assert_eq!(text, "{\"type\":\"hello\"}"),
        other => panic!("unexpected {other:?}"),
    }
    stdin.write_all(b"exit\n").await.unwrap();
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
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
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(1));
}

#[cfg(unix)]
#[tokio::test]
async fn fifo_round_trip_keeps_the_legacy_framing() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
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
    let mut child = Command::new(bin())
        .arg(format!("ws://{addr}"))
        .arg(&id)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let input_path = input.clone();
    let output_path = output.clone();
    let writer = tokio::spawn(async move {
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(input_path)
            .await
            .unwrap()
    });
    let reader = tokio::spawn(async move {
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .open(output_path)
            .await
            .unwrap();
        BufReader::new(file)
    });
    let (sock, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut ws = accept_async(sock).await.unwrap();
    let mut to_transport = writer.await.unwrap();
    let mut from_transport = reader.await.unwrap();
    to_transport
        .write_all(b"{\"type\":\"ping\"}\n")
        .await
        .unwrap();
    to_transport.flush().await.unwrap();
    let frame = timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match frame {
        Message::Text(text) => assert_eq!(text, "{\"type\":\"ping\"}\n"),
        other => panic!("unexpected {other:?}"),
    }
    ws.send(Message::Text("{\"command\":\"connected\"}".into()))
        .await
        .unwrap();
    let mut line = String::new();
    timeout(Duration::from_secs(5), from_transport.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(line, "{\"command\":\"connected\"}\n");
    to_transport.write_all(b"  exit  \n").await.unwrap();
    to_transport.flush().await.unwrap();
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
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
async fn fifo_exits_on_close_while_the_input_pipe_stays_open() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (id, input, output) = fifo_paths();
    let mut child = Command::new(bin())
        .arg(format!("ws://{addr}"))
        .arg(&id)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let input_path = input.clone();
    let output_path = output.clone();
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
    let (sock, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut ws = accept_async(sock).await.unwrap();
    let _held_input = writer.await.unwrap();
    let _held_output = reader.await.unwrap();
    ws.send(Message::Close(None)).await.unwrap();
    let status = timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("fifo close waited on the input pipe")
        .unwrap();
    assert!(status.success());
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
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
    let input_path = input.clone();
    let output_path = output.clone();
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
    let _held_input = timeout(Duration::from_secs(5), writer)
        .await
        .unwrap()
        .unwrap();
    let _held_output = timeout(Duration::from_secs(5), reader)
        .await
        .unwrap()
        .unwrap();
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
async fn fifo_input_eof_sends_close_and_exits() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (id, input, output) = fifo_paths();
    let mut child = Command::new(bin())
        .arg(format!("ws://{addr}"))
        .arg(&id)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let input_path = input.clone();
    let output_path = output.clone();
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
    let (sock, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut ws = accept_async(sock).await.unwrap();
    let input_writer = writer.await.unwrap();
    let _held_output = reader.await.unwrap();
    drop(input_writer);
    let frame = timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("fifo input EOF did not close the socket")
        .unwrap()
        .unwrap();
    assert!(
        matches!(frame, Message::Close(_)),
        "expected Close, got {frame:?}"
    );
    let status = timeout(Duration::from_secs(2), child.wait())
        .await
        .expect("fifo input EOF did not exit")
        .unwrap();
    assert!(status.success());
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
}

#[cfg(unix)]
#[tokio::test]
async fn fifo_stalled_output_is_not_a_pong_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (id, input, output) = fifo_paths();
    let mut child = Command::new(bin())
        .arg(format!("ws://{addr}"))
        .arg(&id)
        .env("SELECTIVE_WS_PING_INTERVAL_MS", "200")
        .env("SELECTIVE_WS_PONG_TIMEOUT_MS", "1000")
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let input_path = input.clone();
    let output_path = output.clone();
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
    let (sock, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut ws = accept_async(sock).await.unwrap();
    let _held_input = writer.await.unwrap();
    let _stalled_output = reader.await.unwrap();
    let payload = "x".repeat(300 * 1024);
    let send = tokio::spawn(async move {
        let _ = ws.send(Message::Text(payload)).await;
        let _hold = ws;
        std::future::pending::<()>().await;
    });
    tokio::time::sleep(Duration::from_secs(3)).await;
    if let Some(status) = child.try_wait().unwrap() {
        let mut err = Vec::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = timeout(Duration::from_secs(1), stderr.read_to_end(&mut err)).await;
        }
        panic!(
            "fifo exited {status} while _2 was stalled: {}",
            String::from_utf8_lossy(&err)
        );
    }
    send.abort();
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
        vec!["conformance", "run", "--scenario", "no_such_scenario"],
        vec![
            "conformance",
            "run",
            "--scenario",
            "no_such_scenario",
            "--client",
            "/bin/false",
        ],
    ] {
        let output = Command::new(bin()).args(&args).output().await.unwrap();
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
        &format!(
            "#!/bin/sh\nprintf '%s\\n' invoked >> \"$1\"\nprintf '%s\\n' \"$SELECTIVE_TRANSPORT_URL\" >> \"$1\"\nprintf '%s\\n' \"$SELECTIVE_API_KEY\" >> \"$1\"\nprintf '%s\\n' \"$SELECTIVE_HOST\" >> \"$1\"\nprintf '%s\\n' \"$SELECTIVE_RUN_ID\" >> \"$1\"\nprintf '%s\\n' \"$SELECTIVE_RUN_ATTEMPT\" >> \"$1\"\nprintf '%s\\n' \"$SELECTIVE_RUNNER_ID\" >> \"$1\"\nprintf '%s\\n' \"$SELECTIVE_TEST_MAP_UPLOAD_KEY\" >> \"$1\"\nexec \"$2\" conformance-client\n",
        ),
    );
    let command = format!("{} {} {}", script.display(), marker.display(), bin());
    let output = Command::new(bin())
        .args([
            "conformance",
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
    let output = Command::new(bin())
        .args([
            "conformance",
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
        transport::Keepalive {
            ping_interval: Duration::from_millis(150),
            pong_timeout: Duration::from_millis(500),
        },
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
        transport::Keepalive {
            ping_interval: Duration::from_millis(80),
            pong_timeout: Duration::from_millis(200),
        },
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
