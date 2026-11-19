use std::time::Duration;

use reqwest::redirect;
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;

use crate::upload::gzip_bytes;

const RETRIES: u32 = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MANIFEST_COMMAND: &str = "test_manifest";
const UPLOAD_KEY: &str = "upload";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadBlock {
    pub url: String,
    pub token: String,
    pub max_chunk_bytes: usize,
    pub max_total_bytes: usize,
    pub max_inflated_bytes: usize,
}

impl UploadBlock {
    fn parse(block: &Value) -> Option<Self> {
        let text = |key: &str| block.get(key)?.as_str().map(str::to_owned);
        let size = |key: &str| {
            usize::try_from(block.get(key)?.as_u64()?)
                .ok()
                .filter(|size| *size > 0)
        };
        Some(Self {
            url: text("url").filter(|url| url.starts_with('/') && !url.starts_with("//"))?,
            token: text("token")?,
            max_chunk_bytes: size("max_chunk_bytes")?,
            max_total_bytes: size("max_total_bytes")?,
            max_inflated_bytes: size("max_inflated_bytes")?,
        })
    }
}

#[must_use]
pub fn intercept(text: &str) -> Option<(String, Option<UploadBlock>)> {
    if !text.contains(UPLOAD_KEY) {
        return None;
    }
    let mut command: Value = serde_json::from_str(text).ok()?;
    let fields = command.as_object_mut()?;
    if fields.get("command")?.as_str()? != MANIFEST_COMMAND {
        return None;
    }
    let block = fields.shift_remove(UPLOAD_KEY)?;
    Some((command.to_string(), UploadBlock::parse(&block)))
}

#[must_use]
pub fn is_manifest_line(line: &str) -> bool {
    #[derive(Deserialize)]
    struct Kind {
        #[serde(rename = "type")]
        kind: String,
    }
    serde_json::from_str::<Kind>(line).is_ok_and(|kind| kind.kind == MANIFEST_COMMAND)
}

#[derive(Debug, Clone)]
pub struct Target {
    pub origin: String,
    pub api_key: String,
}

impl Target {
    #[must_use]
    pub fn from_ws_url(url: &Url) -> Option<Self> {
        let api_key = url
            .query_pairs()
            .find_map(|(name, value)| (name == "api_key").then(|| value.into_owned()))
            .filter(|key| !key.is_empty())?;
        let mut origin = url.clone();
        origin
            .set_scheme(match url.scheme() {
                "ws" => "http",
                "wss" => "https",
                _ => return None,
            })
            .ok()?;
        origin.set_path("");
        origin.set_query(None);
        origin.set_fragment(None);
        Some(Self {
            origin: origin.as_str().trim_end_matches('/').to_owned(),
            api_key,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub backoff: Duration,
    pub chunk_timeout: Duration,
    pub commit_timeout: Duration,
    pub deadline: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            backoff: Duration::from_secs(1),
            chunk_timeout: Duration::from_secs(60),
            commit_timeout: Duration::from_secs(75),
            deadline: Duration::from_secs(180),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Delivered,
    Stopped,
    Fallback,
}

enum Reply {
    Done,
    Retry,
    Stop,
    Missing(Vec<usize>),
    Fallback,
}

impl Reply {
    fn outcome(self) -> Outcome {
        match self {
            Self::Done => Outcome::Delivered,
            Self::Stop => Outcome::Stopped,
            Self::Retry | Self::Missing(_) | Self::Fallback => Outcome::Fallback,
        }
    }
}

fn classify(status: u16, body: &[u8], accepted: &[&str]) -> Reply {
    match status {
        200 if accepted.is_empty() || has_status(body, accepted) => Reply::Done,
        408 | 500 | 503 => Reply::Retry,
        410 => Reply::Stop,
        422 => missing_chunks(body).map_or(Reply::Fallback, Reply::Missing),
        _ => Reply::Fallback,
    }
}

fn has_status(body: &[u8], accepted: &[&str]) -> bool {
    serde_json::from_slice::<Value>(body).ok().and_then(|body| {
        body.get("status")?
            .as_str()
            .map(|status| accepted.contains(&status))
    }) == Some(true)
}

fn missing_chunks(body: &[u8]) -> Option<Vec<usize>> {
    let body: Value = serde_json::from_slice(body).ok()?;
    if body.get("error")?.as_str()? != "missing_chunks" {
        return None;
    }
    body.get("missing")?
        .as_array()?
        .iter()
        .map(|index| usize::try_from(index.as_u64()?).ok())
        .collect()
}

struct Request<'a> {
    client: reqwest::Client,
    target: &'a Target,
    block: &'a UploadBlock,
    policy: Policy,
}

impl Request<'_> {
    async fn post(
        &self,
        path: String,
        content_type: &str,
        body: &[u8],
        timeout: Duration,
        accepted: &[&str],
    ) -> Reply {
        let url = format!("{}{}{path}", self.target.origin, self.block.url);
        let mut backoff = self.policy.backoff;
        let mut timed_out = false;
        for attempt in 0..=RETRIES {
            let sent = self
                .client
                .post(&url)
                .header("authorization", &self.target.api_key)
                .header("x-selective-upload-token", &self.block.token)
                .header("content-type", content_type)
                .timeout(timeout)
                .body(body.to_vec())
                .send()
                .await;
            let reply = match sent {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let body = if matches!(status, 200 | 422) {
                        response.bytes().await.unwrap_or_default().to_vec()
                    } else {
                        Vec::new()
                    };
                    classify(status, &body, accepted)
                }
                Err(err) if err.is_timeout() => {
                    if std::mem::replace(&mut timed_out, true) {
                        Reply::Fallback
                    } else {
                        Reply::Retry
                    }
                }
                Err(_) => Reply::Fallback,
            };
            if !matches!(reply, Reply::Retry) {
                return reply;
            }
            if attempt < RETRIES {
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
        }
        Reply::Fallback
    }

    async fn chunk(&self, index: usize, bytes: &[u8]) -> Reply {
        self.post(
            format!("/chunks/{index}"),
            "application/octet-stream",
            bytes,
            self.policy.chunk_timeout,
            &[],
        )
        .await
    }

    async fn commit(&self, chunks: usize, bytes: usize) -> Reply {
        let body = json!({ "chunks": chunks, "bytes": bytes }).to_string();
        self.post(
            "/commit".to_owned(),
            "application/json",
            body.as_bytes(),
            self.policy.commit_timeout,
            &["applied", "already_applied"],
        )
        .await
    }
}

pub async fn upload(target: &Target, block: &UploadBlock, line: &str, policy: Policy) -> Outcome {
    let raw = line.trim_end_matches(['\r', '\n']).as_bytes().to_vec();
    if raw.len() > block.max_inflated_bytes || Url::parse(&target.origin).is_err() {
        return Outcome::Fallback;
    }
    let Ok(Ok(gzipped)) = tokio::task::spawn_blocking(move || gzip_bytes(&raw)).await else {
        return Outcome::Fallback;
    };
    if gzipped.len() > block.max_total_bytes {
        return Outcome::Fallback;
    }
    let Ok(client) = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(redirect::Policy::none())
        .build()
    else {
        return Outcome::Fallback;
    };
    let request = Request {
        client,
        target,
        block,
        policy,
    };
    tokio::time::timeout(policy.deadline, deliver(&request, &gzipped))
        .await
        .unwrap_or(Outcome::Fallback)
}

async fn deliver(request: &Request<'_>, gzipped: &[u8]) -> Outcome {
    let chunks: Vec<&[u8]> = gzipped.chunks(request.block.max_chunk_bytes).collect();
    for (index, bytes) in chunks.iter().enumerate() {
        match request.chunk(index, bytes).await {
            Reply::Done => {}
            other => return other.outcome(),
        }
    }
    let total = gzipped.len();
    let first = request.commit(chunks.len(), total).await;
    let Reply::Missing(missing) = first else {
        return first.outcome();
    };
    if missing.iter().any(|index| *index >= chunks.len()) {
        return Outcome::Fallback;
    }
    for index in missing {
        match request.chunk(index, chunks[index]).await {
            Reply::Done => {}
            other => return other.outcome(),
        }
    }
    request.commit(chunks.len(), total).await.outcome()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const KEY: &str = "suite-key";
    const TOKEN: &str = "upload-token";

    #[derive(Clone, Debug)]
    struct Seen {
        path: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
        at: Instant,
    }

    type Canned = (u16, String, u64);
    type Script = Arc<dyn Fn(&Seen, usize) -> Canned + Send + Sync>;
    type Log = Arc<Mutex<Vec<Seen>>>;

    fn status(code: u16) -> Canned {
        (code, String::new(), 0)
    }

    async fn read_request(stream: &mut TcpStream) -> Option<Seen> {
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 65536];
        let header_end = loop {
            if let Some(at) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                break at;
            }
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let mut lines = head.lines();
        let path = lines.next()?.split(' ').nth(1)?.to_owned();
        let headers: HashMap<String, String> = lines
            .filter_map(|line| line.split_once(": "))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
            .collect();
        let length: usize = headers
            .get("content-length")
            .map_or(Some(0), |v| v.parse().ok())?;
        let mut body = buf[header_end + 4..].to_vec();
        while body.len() < length {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        Some(Seen {
            path,
            headers,
            body,
            at: Instant::now(),
        })
    }

    async fn serve(script: Script) -> (Target, Log) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = target_for(listener.local_addr().unwrap().port());
        let log: Log = Arc::default();
        let seen_log = Arc::clone(&log);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let script = Arc::clone(&script);
                let log = Arc::clone(&seen_log);
                tokio::spawn(async move {
                    let Some(seen) = read_request(&mut stream).await else {
                        return;
                    };
                    let index = {
                        let mut log = log.lock().unwrap();
                        log.push(seen.clone());
                        log.len() - 1
                    };
                    let (code, body, delay_ms) = script(&seen, index);
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    let location = if code == 302 {
                        "Location: /elsewhere\r\n"
                    } else {
                        ""
                    };
                    let body = if code == 200 && body.is_empty() {
                        r#"{"status":"applied"}"#.to_owned()
                    } else {
                        body
                    };
                    let response = format!(
                        "HTTP/1.1 {code} X\r\n{location}Content-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (target, log)
    }

    fn target_for(port: u16) -> Target {
        let url = Url::parse(&format!(
            "ws://127.0.0.1:{port}/transport/websocket?api_key={KEY}&run_id=1"
        ))
        .unwrap();
        Target::from_ws_url(&url).unwrap()
    }

    fn always(code: u16) -> Script {
        Arc::new(move |_, _| status(code))
    }

    fn block(max_chunk_bytes: usize) -> UploadBlock {
        UploadBlock {
            url: "/manifest_uploads".to_owned(),
            token: TOKEN.to_owned(),
            max_chunk_bytes,
            max_total_bytes: 64_000_000,
            max_inflated_bytes: 64_000_000,
        }
    }

    fn fast() -> Policy {
        Policy {
            backoff: Duration::from_millis(1),
            chunk_timeout: Duration::from_secs(5),
            commit_timeout: Duration::from_secs(5),
            deadline: Duration::from_secs(30),
        }
    }

    fn manifest_line(payload_bytes: usize) -> String {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let payload: String = (0..payload_bytes)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                char::from(b'a' + u8::try_from((state >> 59) % 26).unwrap())
            })
            .collect();
        format!(r#"{{"type":"test_manifest","data":"{payload}"}}"#) + "\n"
    }

    fn gunzip(bytes: &[u8]) -> String {
        let mut text = String::new();
        flate2::read::GzDecoder::new(bytes)
            .read_to_string(&mut text)
            .unwrap();
        text
    }

    fn chunk_paths(log: &Log) -> Vec<String> {
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.path.contains("/chunks/"))
            .map(|seen| seen.path.clone())
            .collect()
    }

    fn requests(log: &Log) -> usize {
        log.lock().unwrap().len()
    }

    fn missing_reply(missing: &[usize]) -> Canned {
        (
            422,
            json!({"error": "missing_chunks", "missing": missing}).to_string(),
            0,
        )
    }

    #[test]
    fn the_line_type_is_parsed_not_sniffed() {
        for line in [
            r#"{"type":"test_manifest","data":{}}"#,
            r#"{"data":{"test_cases":[]},"type":"test_manifest"}"#,
            "{ \"data\" : 1 , \"type\" : \"test_manifest\" }\n",
        ] {
            assert!(is_manifest_line(line), "{line}");
        }
        for line in [
            r#"{"type":"test_case_result","data":{"type":"test_manifest"}}"#,
            r#"{"data":"{\"type\":\"test_manifest\"}"}"#,
            r#"{"type":"test_manifest_extra"}"#,
            r#"{"type":7}"#,
            "exit",
            "",
        ] {
            assert!(!is_manifest_line(line), "{line}");
        }
    }

    #[test]
    fn intercept_strips_the_upload_key_and_keeps_everything_else_in_order() {
        let text = r#"{"command":"test_manifest","upload":{"url":"/manifest_uploads","token":"t","max_chunk_bytes":8000000,"max_total_bytes":64000000,"max_inflated_bytes":64000000},"extra":[1,2],"tail":true}"#;
        let (forwarded, block) = intercept(text).unwrap();
        assert_eq!(
            forwarded,
            r#"{"command":"test_manifest","extra":[1,2],"tail":true}"#
        );
        assert_eq!(
            block,
            Some(UploadBlock {
                url: "/manifest_uploads".to_owned(),
                token: "t".to_owned(),
                max_chunk_bytes: 8_000_000,
                max_total_bytes: 64_000_000,
                max_inflated_bytes: 64_000_000,
            })
        );
    }

    #[test]
    fn intercept_leaves_other_text_alone() {
        for text in [
            r#"{"command":"test_manifest"}"#,
            r#"{"command":"run_test_cases","upload":{"url":"/x"}}"#,
            r#"{"command":"print_notice","message":"upload"}"#,
            r#"{"upload":{"url":"/x"}}"#,
            "not json upload",
        ] {
            assert_eq!(intercept(text), None, "{text}");
        }
    }

    #[test]
    fn an_unusable_block_is_stripped_and_ignored() {
        let usable = json!({"url":"/m","token":"t","max_chunk_bytes":5,"max_total_bytes":7,"max_inflated_bytes":9});
        assert!(UploadBlock::parse(&usable).is_some());
        for (key, value) in [
            ("url", json!("//evil.test/m")),
            ("url", json!("http://evil.test/m")),
            ("url", json!("m")),
            ("url", json!(7)),
            ("token", json!(null)),
            ("max_chunk_bytes", json!(0)),
            ("max_chunk_bytes", json!(-1)),
            ("max_total_bytes", json!(0)),
            ("max_total_bytes", json!(null)),
            ("max_inflated_bytes", json!("9")),
        ] {
            let mut block = usable.clone();
            block[key] = value.clone();
            assert!(UploadBlock::parse(&block).is_none(), "{key}={value}");
            let text = json!({"command":"test_manifest","upload":block}).to_string();
            let (forwarded, parsed) = intercept(&text).unwrap();
            assert_eq!(forwarded, r#"{"command":"test_manifest"}"#);
            assert_eq!(parsed, None);
        }
    }

    #[test]
    fn the_target_is_the_origin_of_the_connect_url_never_the_socket_path() {
        let target = |url: &str| Target::from_ws_url(&Url::parse(url).unwrap());
        let plain = target("ws://h.test:4000/transport/websocket?api_key=k&run_id=1").unwrap();
        assert_eq!(plain.origin, "http://h.test:4000");
        assert_eq!(plain.api_key, "k");
        let secure = target("wss://h.test/transport/websocket?run_id=1&api_key=k%2B").unwrap();
        assert_eq!(secure.origin, "https://h.test");
        assert_eq!(secure.api_key, "k+");
        assert!(target("wss://h.test/transport/websocket?run_id=1").is_none());
        assert!(target("wss://h.test/transport/websocket?api_key=").is_none());
    }

    #[tokio::test]
    async fn posts_one_gzip_stream_in_slices_with_the_contract_headers() {
        let (target, log) = serve(Arc::new(|_, _| status(200))).await;
        let line = manifest_line(3000);
        let outcome = upload(&target, &block(100), &line, fast()).await;
        assert_eq!(outcome, Outcome::Delivered);

        let seen = log.lock().unwrap().clone();
        let (commit, chunks) = seen.split_last().unwrap();
        assert!(chunks.len() > 2, "{} chunks", chunks.len());
        for (index, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.path, format!("/manifest_uploads/chunks/{index}"));
            assert_eq!(chunk.headers["content-type"], "application/octet-stream");
            assert!(chunk.body.len() <= 100);
        }
        assert!(chunks[..chunks.len() - 1]
            .iter()
            .all(|chunk| chunk.body.len() == 100));
        for request in &seen {
            assert_eq!(request.headers["authorization"], KEY);
            assert_eq!(request.headers["x-selective-upload-token"], TOKEN);
        }
        let joined: Vec<u8> = chunks.iter().flat_map(|c| c.body.clone()).collect();
        assert_eq!(gunzip(&joined), line.trim_end());
        assert_eq!(commit.path, "/manifest_uploads/commit");
        assert_eq!(commit.headers["content-type"], "application/json");
        let body: Value = serde_json::from_slice(&commit.body).unwrap();
        assert_eq!(body, json!({"chunks": chunks.len(), "bytes": joined.len()}));
    }

    #[tokio::test]
    async fn a_stream_exactly_one_slice_long_is_one_chunk_and_one_byte_more_is_two() {
        let line = manifest_line(2000);
        let length = gzip_bytes(line.trim_end().as_bytes()).unwrap().len();
        for (slice, want) in [(length, 1), (length - 1, 2), (length + 1, 1)] {
            let (target, log) = serve(Arc::new(|_, _| status(200))).await;
            upload(&target, &block(slice), &line, fast()).await;
            assert_eq!(chunk_paths(&log).len(), want, "slice {slice} of {length}");
        }
    }

    #[tokio::test]
    async fn the_eight_million_byte_slice_boundary() {
        const SLICE: usize = 8_000_000;
        const PAYLOAD_FOR_ONE_FULL_SLICE: usize = 12_469_342;
        let line = manifest_line(PAYLOAD_FOR_ONE_FULL_SLICE);
        let sizes = |log: &Log| -> Vec<usize> {
            log.lock()
                .unwrap()
                .iter()
                .filter(|seen| seen.path.contains("/chunks/"))
                .map(|seen| seen.body.len())
                .collect()
        };
        let (target, log) = serve(Arc::new(|_, _| status(200))).await;
        upload(&target, &block(SLICE), &line, fast()).await;
        assert_eq!(sizes(&log), [SLICE]);
        let (target, log) = serve(Arc::new(|_, _| status(200))).await;
        upload(&target, &block(SLICE - 1), &line, fast()).await;
        assert_eq!(sizes(&log), [SLICE - 1, 1]);
    }

    #[tokio::test]
    async fn retry_statuses_are_retried_on_the_same_request_and_then_delivered() {
        for code in [503, 408, 500] {
            let script: Script =
                Arc::new(move |_, index| status(if index == 0 { code } else { 200 }));
            let (target, log) = serve(script).await;
            let outcome = upload(&target, &block(1_000_000), &manifest_line(50), fast()).await;
            assert_eq!(outcome, Outcome::Delivered, "{code}");
            let paths: Vec<String> = log.lock().unwrap().iter().map(|s| s.path.clone()).collect();
            assert_eq!(
                paths,
                [
                    "/manifest_uploads/chunks/0",
                    "/manifest_uploads/chunks/0",
                    "/manifest_uploads/commit"
                ],
                "{code}"
            );
        }
    }

    #[tokio::test]
    async fn a_410_stops_the_upload_with_no_further_requests() {
        let (target, log) = serve(always(410)).await;
        let outcome = upload(&target, &block(100), &manifest_line(3000), fast()).await;
        assert_eq!(outcome, Outcome::Stopped);
        assert_eq!(requests(&log), 1);

        let commit_only: Script = Arc::new(|seen, _| {
            status(if seen.path.ends_with("/commit") {
                410
            } else {
                200
            })
        });
        let (target, log) = serve(commit_only).await;
        let outcome = upload(&target, &block(1_000_000), &manifest_line(50), fast()).await;
        assert_eq!(outcome, Outcome::Stopped);
        assert_eq!(requests(&log), 2);
    }

    #[tokio::test]
    async fn a_410_stops_only_the_upload_that_carried_the_token() {
        let by_token: Script = Arc::new(|seen, _| {
            status(if seen.headers["x-selective-upload-token"] == "old" {
                410
            } else {
                200
            })
        });
        let (target, log) = serve(by_token).await;
        let line = manifest_line(50);
        let mut old = block(1_000_000);
        old.token = "old".to_owned();
        assert_eq!(upload(&target, &old, &line, fast()).await, Outcome::Stopped);
        let mut fresh = block(1_000_000);
        fresh.token = "fresh".to_owned();
        assert_eq!(
            upload(&target, &fresh, &line, fast()).await,
            Outcome::Delivered
        );
        let seen = log.lock().unwrap().clone();
        assert_eq!(
            seen.last().unwrap().headers["x-selective-upload-token"],
            "fresh"
        );
        assert_eq!(seen.len(), 3);
    }

    #[tokio::test]
    async fn every_other_status_falls_back_after_one_request() {
        for code in [302, 400, 401, 403, 404, 409, 413, 422, 502, 504] {
            let (target, log) = serve(always(code)).await;
            let outcome = upload(&target, &block(1_000_000), &manifest_line(50), fast()).await;
            assert_eq!(outcome, Outcome::Fallback, "{code}");
            assert_eq!(requests(&log), 1, "{code}");
        }
    }

    #[tokio::test]
    async fn a_connection_error_falls_back_without_retrying() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let policy = Policy {
            backoff: Duration::from_secs(2),
            ..fast()
        };
        let started = Instant::now();
        let outcome = upload(
            &target_for(port),
            &block(1_000_000),
            &manifest_line(50),
            policy,
        )
        .await;
        assert_eq!(outcome, Outcome::Fallback);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_server_that_closes_every_connection_costs_one_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = target_for(listener.local_addr().unwrap().port());
        let accepted = Arc::new(Mutex::new(0_usize));
        let counter = Arc::clone(&accepted);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                *counter.lock().unwrap() += 1;
                drop(stream);
            }
        });
        let policy = Policy {
            chunk_timeout: Duration::from_millis(300),
            ..fast()
        };
        let started = Instant::now();
        let outcome = upload(&target, &block(1_000_000), &manifest_line(50), policy).await;
        assert_eq!(outcome, Outcome::Fallback);
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(
            *accepted.lock().unwrap() <= 2,
            "{} connections",
            *accepted.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn a_request_that_spends_three_retries_falls_back_after_four_attempts() {
        let base = Duration::from_millis(40);
        let policy = Policy {
            backoff: base,
            ..fast()
        };
        let (target, log) = serve(always(503)).await;
        let outcome = upload(&target, &block(1_000_000), &manifest_line(50), policy).await;
        assert_eq!(outcome, Outcome::Fallback);
        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 4);
        for (attempt, pair) in seen.windows(2).enumerate() {
            let want = base * 2u32.pow(u32::try_from(attempt).unwrap());
            assert!(pair[1].at - pair[0].at >= want, "gap {attempt}");
        }
    }

    #[tokio::test]
    async fn three_failures_then_success_is_still_delivered() {
        let script: Script = Arc::new(|_, index| status(if index < 3 { 503 } else { 200 }));
        let (target, log) = serve(script).await;
        let outcome = upload(&target, &block(1_000_000), &manifest_line(50), fast()).await;
        assert_eq!(outcome, Outcome::Delivered);
        assert_eq!(requests(&log), 5);
    }

    #[tokio::test]
    async fn missing_chunks_are_resent_once_and_the_commit_repeated() {
        let commits = Arc::new(Mutex::new(0));
        let counter = Arc::clone(&commits);
        let script: Script = Arc::new(move |seen, _| {
            if !seen.path.ends_with("/commit") {
                return status(200);
            }
            let mut count = counter.lock().unwrap();
            *count += 1;
            if *count == 1 {
                missing_reply(&[1])
            } else {
                status(200)
            }
        });
        let (target, log) = serve(script).await;
        let outcome = upload(&target, &block(100), &manifest_line(3000), fast()).await;
        assert_eq!(outcome, Outcome::Delivered);
        let paths: Vec<String> = log.lock().unwrap().iter().map(|s| s.path.clone()).collect();
        let tail = &paths[paths.len() - 3..];
        assert_eq!(
            tail,
            [
                "/manifest_uploads/commit",
                "/manifest_uploads/chunks/1",
                "/manifest_uploads/commit"
            ]
        );
        assert_eq!(
            chunk_paths(&log)
                .iter()
                .filter(|p| p.ends_with("/chunks/1"))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn a_second_missing_chunks_or_an_unknown_index_falls_back() {
        let always_missing: Script = Arc::new(|seen, _| {
            if seen.path.ends_with("/commit") {
                missing_reply(&[0])
            } else {
                status(200)
            }
        });
        let (target, log) = serve(always_missing).await;
        let outcome = upload(&target, &block(1_000_000), &manifest_line(50), fast()).await;
        assert_eq!(outcome, Outcome::Fallback);
        assert_eq!(requests(&log), 4);

        let out_of_range: Script = Arc::new(|seen, _| {
            if seen.path.ends_with("/commit") {
                missing_reply(&[9])
            } else {
                status(200)
            }
        });
        let (target, log) = serve(out_of_range).await;
        let outcome = upload(&target, &block(1_000_000), &manifest_line(50), fast()).await;
        assert_eq!(outcome, Outcome::Fallback);
        assert_eq!(requests(&log), 2);
    }

    #[tokio::test]
    async fn a_line_over_the_inflated_limit_never_touches_the_network() {
        let line = manifest_line(500);
        let raw = line.trim_end().len();
        let mut limited = block(1_000_000);
        limited.max_inflated_bytes = raw - 1;
        let (target, log) = serve(always(200)).await;
        assert_eq!(
            upload(&target, &limited, &line, fast()).await,
            Outcome::Fallback
        );
        assert_eq!(requests(&log), 0);
        limited.max_inflated_bytes = raw;
        assert_eq!(
            upload(&target, &limited, &line, fast()).await,
            Outcome::Delivered
        );
        assert_eq!(requests(&log), 2);
    }

    #[tokio::test]
    async fn a_gzip_over_the_total_limit_never_touches_the_network() {
        let line = manifest_line(500);
        let compressed = gzip_bytes(line.trim_end().as_bytes()).unwrap().len();
        let mut limited = block(1_000_000);
        limited.max_total_bytes = compressed - 1;
        let (target, log) = serve(always(200)).await;
        assert_eq!(
            upload(&target, &limited, &line, fast()).await,
            Outcome::Fallback
        );
        assert_eq!(requests(&log), 0);
        limited.max_total_bytes = compressed;
        assert_eq!(
            upload(&target, &limited, &line, fast()).await,
            Outcome::Delivered
        );
        assert_eq!(requests(&log), 2);
    }

    #[tokio::test]
    async fn a_commit_is_delivered_only_when_the_body_says_applied() {
        for (body, want) in [
            (r#"{"status":"applied"}"#, Outcome::Delivered),
            (r#"{"status":"already_applied"}"#, Outcome::Delivered),
            (r#"{"status":"queued"}"#, Outcome::Fallback),
            ("<html>maintenance</html>", Outcome::Fallback),
            (r#"{"error":"applied"}"#, Outcome::Fallback),
            (r#"{"status":7}"#, Outcome::Fallback),
        ] {
            let script: Script = Arc::new(move |seen, _| {
                if seen.path.ends_with("/commit") {
                    (200, body.to_owned(), 0)
                } else {
                    status(200)
                }
            });
            let (target, log) = serve(script).await;
            let outcome = upload(&target, &block(1_000_000), &manifest_line(50), fast()).await;
            assert_eq!(outcome, want, "{body}");
            assert_eq!(requests(&log), 2, "{body}");
        }
    }

    #[tokio::test]
    async fn a_chunk_200_needs_no_particular_body() {
        let script: Script = Arc::new(|seen, _| {
            if seen.path.contains("/chunks/") {
                (200, "ok".to_owned(), 0)
            } else {
                status(200)
            }
        });
        let (target, _log) = serve(script).await;
        let outcome = upload(&target, &block(1_000_000), &manifest_line(50), fast()).await;
        assert_eq!(outcome, Outcome::Delivered);
    }

    #[tokio::test]
    async fn the_whole_upload_falls_back_at_its_deadline() {
        let deadline = Duration::from_millis(400);
        let policy = Policy { deadline, ..fast() };
        let (target, _log) = serve(Arc::new(|_, _| (200, String::new(), 5000))).await;
        let started = Instant::now();
        let outcome = upload(&target, &block(1_000_000), &manifest_line(50), policy).await;
        let elapsed = started.elapsed();
        assert_eq!(outcome, Outcome::Fallback);
        assert!(elapsed >= deadline, "{elapsed:?}");
        assert!(
            elapsed < deadline + Duration::from_millis(400),
            "{elapsed:?}"
        );
    }

    #[tokio::test]
    async fn requests_that_each_succeed_still_stop_at_the_deadline() {
        let slow: Script = Arc::new(|_, _| (200, String::new(), 150));
        let line = manifest_line(3000);
        let (target, log) = serve(Arc::clone(&slow)).await;
        let tight = Policy {
            deadline: Duration::from_millis(400),
            ..fast()
        };
        assert_eq!(
            upload(&target, &block(500), &line, tight).await,
            Outcome::Fallback
        );
        assert!(requests(&log) < 6, "{}", requests(&log));
        let (target, _log) = serve(slow).await;
        assert_eq!(
            upload(&target, &block(500), &line, fast()).await,
            Outcome::Delivered
        );
    }

    #[tokio::test]
    async fn a_commit_that_stalls_past_its_timeout_is_retried() {
        let commits = Arc::new(Mutex::new(0));
        let counter = Arc::clone(&commits);
        let script: Script = Arc::new(move |seen, _| {
            let mut stall = 0;
            if seen.path.ends_with("/commit") {
                let mut count = counter.lock().unwrap();
                *count += 1;
                if *count == 1 {
                    stall = 600;
                }
            }
            (200, String::new(), stall)
        });
        let (target, log) = serve(script).await;
        let policy = Policy {
            commit_timeout: Duration::from_millis(150),
            ..fast()
        };
        let outcome = upload(&target, &block(1_000_000), &manifest_line(50), policy).await;
        assert_eq!(outcome, Outcome::Delivered);
        assert_eq!(*commits.lock().unwrap(), 2);
        assert_eq!(requests(&log), 3);
    }

    fn stall_script(stalls: Arc<Mutex<HashMap<String, usize>>>, pattern: &'static [u16]) -> Script {
        Arc::new(move |seen, _| {
            let attempt = {
                let mut stalls = stalls.lock().unwrap();
                let count = stalls.entry(seen.path.clone()).or_insert(0);
                *count += 1;
                *count - 1
            };
            match pattern.get(attempt) {
                Some(0) => (200, String::new(), 2000),
                Some(code) => (*code, String::new(), 0),
                None => (200, String::new(), 0),
            }
        })
    }

    fn timing_policy() -> Policy {
        Policy {
            backoff: Duration::from_millis(20),
            chunk_timeout: Duration::from_millis(80),
            commit_timeout: Duration::from_millis(120),
            deadline: Duration::from_secs(30),
        }
    }

    #[tokio::test]
    async fn a_request_that_times_out_twice_falls_back_after_two_attempts() {
        let script = stall_script(Arc::default(), &[0, 0, 0, 0]);
        let (target, log) = serve(script).await;
        let started = Instant::now();
        let outcome = upload(
            &target,
            &block(1_000_000),
            &manifest_line(50),
            timing_policy(),
        )
        .await;
        let elapsed = started.elapsed();
        assert_eq!(outcome, Outcome::Fallback);
        assert_eq!(requests(&log), 2);
        assert!(elapsed >= Duration::from_millis(2 * 80 + 20), "{elapsed:?}");
        assert!(
            elapsed < Duration::from_millis(2 * 80 + 20 + 400),
            "{elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_timeout_and_status_retries_share_four_attempts_and_the_longest_wait() {
        let script = stall_script(Arc::default(), &[0, 503, 503, 0]);
        let (target, log) = serve(script).await;
        let started = Instant::now();
        let outcome = upload(
            &target,
            &block(1_000_000),
            &manifest_line(50),
            timing_policy(),
        )
        .await;
        let elapsed = started.elapsed();
        assert_eq!(outcome, Outcome::Fallback);
        assert_eq!(requests(&log), 4);
        let longest = Duration::from_millis(2 * 80 + 7 * 20);
        assert!(elapsed >= longest, "{elapsed:?}");
        assert!(
            elapsed < longest + Duration::from_millis(400),
            "{elapsed:?}"
        );
    }

    #[tokio::test]
    async fn an_upload_whose_every_request_degrades_then_succeeds_stays_inside_its_budget() {
        let script = stall_script(Arc::default(), &[0, 503, 503]);
        let (target, log) = serve(script).await;
        let line = manifest_line(3000);
        let started = Instant::now();
        let outcome = upload(&target, &block(100), &line, timing_policy()).await;
        let elapsed = started.elapsed();
        assert_eq!(outcome, Outcome::Delivered);
        let chunks = chunk_paths(&log).len() / 4;
        assert!(chunks > 2, "{chunks} chunks");
        assert_eq!(requests(&log), 4 * (chunks + 1));
        let budget = u32::try_from(chunks).unwrap() * Duration::from_millis(80 + 7 * 20)
            + Duration::from_millis(120 + 7 * 20);
        assert!(elapsed >= budget, "{elapsed:?} < {budget:?}");
        assert!(
            elapsed < budget + Duration::from_millis(600),
            "{elapsed:?} vs {budget:?}"
        );
    }

    #[test]
    fn the_default_budget_is_pinned() {
        let policy = Policy::default();
        let worst = |timeout: Duration| timeout * 2 + policy.backoff * 7;
        assert_eq!(policy.chunk_timeout, Duration::from_secs(60));
        assert_eq!(policy.commit_timeout, Duration::from_secs(75));
        assert_eq!(policy.deadline, Duration::from_secs(180));
        assert_eq!(worst(policy.chunk_timeout), Duration::from_secs(127));
        assert_eq!(worst(policy.commit_timeout), Duration::from_secs(157));
    }

    #[test]
    fn the_default_commit_timeout_covers_the_servers_worst_case() {
        assert!(Policy::default().commit_timeout >= Duration::from_secs(70));
    }
}
