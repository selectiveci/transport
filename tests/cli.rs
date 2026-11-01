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
