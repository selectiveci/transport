use std::io::Write;
use std::time::Duration;

use flate2::write::GzEncoder;
use flate2::Compression;
use url::Url;

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

pub fn gzip_bytes(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

pub fn http_origin(websocket_url: &str) -> Result<String, Error> {
    let converted = ws_to_http(websocket_url);
    let url = Url::parse(&converted)?;
    let host = url
        .host_str()
        .ok_or_else(|| Error::new("websocket url has no host"))?;
    let mut origin = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Ok(origin)
}

pub fn fragment_endpoint(
    host: &str,
    run_id: &str,
    run_attempt: &str,
    runner_id: &str,
) -> Result<String, Error> {
    let base = if host.starts_with("ws://") || host.starts_with("wss://") {
        http_origin(host)?
    } else {
        ws_to_http(host).trim_end_matches('/').to_owned()
    };
    let mut url = Url::parse(&format!("{base}/test_maps/fragments"))?;
    url.query_pairs_mut()
        .append_pair("run_id", run_id)
        .append_pair("run_attempt", run_attempt)
        .append_pair("runner_id", runner_id);
    Ok(url.to_string())
}

pub async fn upload_fragment(upload: &FragmentUpload) -> Result<(), Error> {
    upload_fragment_retry(upload, Duration::from_secs(1)).await
}

pub async fn upload_fragment_retry(
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
        .timeout(Duration::from_secs(120))
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
    fn origin_and_endpoint_follow_the_websocket_host() {
        let origin = http_origin("wss://app.selective.ci/transport/websocket?api_key=k").unwrap();
        assert_eq!(origin, "https://app.selective.ci");
        let endpoint = fragment_endpoint("ws://127.0.0.1:9", "run", "2", "shard").unwrap();
        assert!(endpoint.starts_with("http://127.0.0.1:9/test_maps/fragments?"));
        assert!(endpoint.contains("run_id=run"));
        assert!(endpoint.contains("run_attempt=2"));
        assert!(endpoint.contains("runner_id=shard"));
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
}
