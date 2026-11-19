use serde::Deserialize;
use serde_json::Value;
use url::Url;

const MANIFEST_COMMAND: &str = "test_manifest";
const UPLOAD_KEY: &str = "upload";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadBlock {
    pub url: String,
    pub token: String,
    pub max_chunk_bytes: usize,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let usable = json!({"url":"/m","token":"t","max_chunk_bytes":5,"max_inflated_bytes":9});
        assert!(UploadBlock::parse(&usable).is_some());
        for (key, value) in [
            ("url", json!("//evil.test/m")),
            ("url", json!("http://evil.test/m")),
            ("url", json!("m")),
            ("url", json!(7)),
            ("token", json!(null)),
            ("max_chunk_bytes", json!(0)),
            ("max_chunk_bytes", json!(-1)),
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
}
