use std::io::Write;
use std::time::Duration;

use flate2::write::GzEncoder;
use flate2::Compression;
use url::Url;

use serde_json::Value;

use crate::build_env::BuildEnv;
use crate::error::Error;

#[derive(Debug, Clone)]
pub struct FragmentUpload {
    pub host: String,
    pub api_key: String,
    pub run_id: String,
    pub run_attempt: String,
    pub runner_id: String,
    pub gzip_body: Vec<u8>,
}

/// # Errors
/// Propagates the gzip encoder's I/O error.
pub fn gzip_bytes(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

pub(crate) fn fragment_endpoint(
    host: &str,
    run_id: &str,
    run_attempt: &str,
    runner_id: &str,
) -> Result<String, Error> {
    let mut url = Url::parse(&ws_to_http(host))?;
    let path = format!("{}/test_maps/fragments", url.path().trim_end_matches('/'));
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    url.query_pairs_mut()
        .append_pair("run_id", run_id)
        .append_pair("run_attempt", run_attempt)
        .append_pair("runner_id", runner_id);
    Ok(url.to_string())
}

/// # Errors
/// Fails when the endpoint is invalid or all three attempts fail.
pub async fn upload_fragment(upload: &FragmentUpload) -> Result<(), Error> {
    upload_fragment_retry(upload, Duration::from_secs(1)).await
}

pub(crate) async fn upload_fragment_retry(
    upload: &FragmentUpload,
    base_delay: Duration,
) -> Result<(), Error> {
    let endpoint = fragment_endpoint(
        &upload.host,
        &upload.run_id,
        &upload.run_attempt,
        &upload.runner_id,
    )?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(120))
        .build()?;
    let mut last = Error::new("test map upload failed");
    for attempt in 1..=3 {
        match post_once(&client, &endpoint, upload).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                last = err;
                if attempt < 3 {
                    tokio::time::sleep(base_delay.saturating_mul(attempt)).await;
                }
            }
        }
    }
    Err(last)
}

async fn post_once(
    client: &reqwest::Client,
    endpoint: &str,
    upload: &FragmentUpload,
) -> Result<(), Error> {
    let response = client
        .post(endpoint)
        .header("authorization", &upload.api_key)
        .header("content-type", "application/gzip")
        .body(upload.gzip_body.clone())
        .send()
        .await?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        let body = response.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(200).collect();
        Err(Error::new(format!("HTTP {status}: {snippet}")))
    }
}

#[must_use]
pub fn resolve_run_identity(
    flag_run_id: &str,
    flag_run_attempt: &str,
    flag_runner_id: &str,
    payload: Option<&Value>,
    detected: &BuildEnv,
) -> (String, String, String) {
    (
        pick(flag_run_id, payload, "run_id", &detected.run_id),
        pick(
            flag_run_attempt,
            payload,
            "run_attempt",
            &detected.run_attempt,
        ),
        pick(flag_runner_id, payload, "runner_id", &detected.runner_id),
    )
}

/// # Errors
/// Fails when a leading `{` line isn't JSON or no gzip body follows it.
pub fn split_fragment_stdin(raw: &[u8]) -> Result<(Option<Value>, Vec<u8>), Error> {
    if raw.first() != Some(&b'{') {
        return Ok((None, raw.to_vec()));
    }
    let newline = raw.iter().position(|byte| *byte == b'\n');
    let (line, rest) = match newline {
        Some(index) => (&raw[..index], &raw[index + 1..]),
        None => (raw, &[][..]),
    };
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let payload: Value = serde_json::from_slice(line)
        .map_err(|err| Error::new(format!("upload command payload is not JSON: {err}")))?;
    if rest.is_empty() {
        return Err(Error::new(
            "--upload-fragment expected a gzip body after the command payload",
        ));
    }
    Ok((Some(payload), rest.to_vec()))
}

fn pick(flag: &str, payload: Option<&Value>, key: &str, detected: &str) -> String {
    if !flag.is_empty() {
        return flag.to_owned();
    }
    if let Some(value) = payload_string(payload, key) {
        if !value.is_empty() {
            return value;
        }
    }
    detected.to_owned()
}

fn payload_string(payload: Option<&Value>, key: &str) -> Option<String> {
    let payload = payload?;
    json_text(payload.get(key))
        .or_else(|| json_text(payload.get("data").and_then(|data| data.get(key))))
}

fn json_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn ws_to_http(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = value.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn endpoint_keeps_the_host_path() {
        for (host, want) in [
            (
                "ws://127.0.0.1:9",
                "http://127.0.0.1:9/test_maps/fragments?",
            ),
            (
                "wss://app.selective.ci",
                "https://app.selective.ci/test_maps/fragments?",
            ),
            (
                "wss://app.selective.ci/",
                "https://app.selective.ci/test_maps/fragments?",
            ),
            ("ws://h:9/prefix", "http://h:9/prefix/test_maps/fragments?"),
            ("wss://h/prefix/", "https://h/prefix/test_maps/fragments?"),
            ("https://h/a/b?x=1", "https://h/a/b/test_maps/fragments?"),
        ] {
            let endpoint = fragment_endpoint(host, "run", "2", "shard").unwrap();
            assert!(endpoint.starts_with(want), "{host}: {endpoint}");
            assert!(endpoint.contains("run_id=run"), "{endpoint}");
            assert!(endpoint.contains("run_attempt=2"), "{endpoint}");
            assert!(endpoint.contains("runner_id=shard"), "{endpoint}");
            assert!(!endpoint.contains("x=1"), "{endpoint}");
        }
    }

    #[tokio::test]
    async fn upload_posts_gzip_and_retries() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut hits = 0;
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap();
                hits += 1;
                let request = String::from_utf8_lossy(&buf[..n]);
                assert!(request.starts_with("POST /test_maps/fragments?"));
                assert!(request
                    .to_ascii_lowercase()
                    .contains("authorization: secret-key"));
                assert!(request
                    .to_ascii_lowercase()
                    .contains("content-type: application/gzip"));
                let status = if hits < 2 {
                    "500 Internal Server Error"
                } else {
                    "200 OK"
                };
                let response =
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                sock.write_all(response.as_bytes()).await.unwrap();
                if hits >= 2 {
                    break;
                }
            }
        });

        let body = gzip_bytes(br#"{"version":1,"files":[],"tests":[]}"#).unwrap();
        let mut decoder = flate2::read::GzDecoder::new(body.as_slice());
        let mut plain = String::new();
        decoder.read_to_string(&mut plain).unwrap();
        assert!(plain.contains("\"version\":1"));

        let upload = FragmentUpload {
            host: format!("ws://{addr}"),
            api_key: "secret-key".to_owned(),
            run_id: "run-1".to_owned(),
            run_attempt: "1".to_owned(),
            runner_id: "runner-1".to_owned(),
            gzip_body: body,
        };
        upload_fragment_retry(&upload, Duration::from_millis(0))
            .await
            .unwrap();
        server.await.unwrap();
    }

    fn detected() -> BuildEnv {
        BuildEnv {
            api_key: String::new(),
            host: "ws://detected".to_owned(),
            platform: String::new(),
            branch: String::new(),
            pr_title: String::new(),
            target_branch: String::new(),
            actor: String::new(),
            sha: String::new(),
            base_sha: String::new(),
            run_id: "detected-run".to_owned(),
            run_attempt: "9".to_owned(),
            runner_id: "detected-runner".to_owned(),
            commit_message: String::new(),
            committer_name: String::new(),
            committer_email: String::new(),
            git_repo_full_name: String::new(),
            git_provider: String::new(),
        }
    }

    #[test]
    fn explicit_ids_beat_the_payload_and_detection_fills_the_gaps() {
        let payload = serde_json::json!({
            "type": "upload_fragment",
            "run_id": "payload-run",
            "data": {"run_attempt": 4, "runner_id": "payload-runner"}
        });
        let (run_id, run_attempt, runner_id) =
            resolve_run_identity("session-run", "", "", Some(&payload), &detected());
        assert_eq!(run_id, "session-run");
        assert_eq!(run_attempt, "4");
        assert_eq!(runner_id, "payload-runner");

        let (run_id, run_attempt, runner_id) = resolve_run_identity("", "", "", None, &detected());
        assert_eq!(run_id, "detected-run");
        assert_eq!(run_attempt, "9");
        assert_eq!(runner_id, "detected-runner");
    }

    #[test]
    fn stdin_json_line_is_the_command_payload_and_the_rest_is_gzip() {
        let gzip = gzip_bytes(br#"{"version":1}"#).unwrap();
        let mut raw =
            br#"{"run_id":"payload-run","run_attempt":"4","runner_id":"payload-runner"}"#.to_vec();
        raw.push(b'\n');
        raw.extend_from_slice(&gzip);
        let (payload, body) = split_fragment_stdin(&raw).unwrap();
        assert_eq!(payload.unwrap()["run_id"], "payload-run");
        assert_eq!(body, gzip);
        let (payload, body) = split_fragment_stdin(&gzip).unwrap();
        assert!(payload.is_none());
        assert_eq!(body, gzip);
        assert!(split_fragment_stdin(br#"{"run_id":"only"}"#).is_err());
    }
}
