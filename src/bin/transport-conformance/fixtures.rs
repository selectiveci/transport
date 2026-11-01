use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use transport::Error;

const ROLES: &[(&str, &str)] = &[
    ("pass", "case-pass"),
    ("fail", "case-fail"),
    ("pending", "case-pend"),
    ("flaky", "case-flake"),
];

const BEFORE_ALL_KEY: &str = "before_all_seconds";

#[derive(Clone, Debug, PartialEq)]
pub struct FixtureIds {
    roles: BTreeMap<String, String>,
    before_all_seconds: Option<f64>,
}

impl Default for FixtureIds {
    fn default() -> Self {
        Self {
            roles: ROLES
                .iter()
                .map(|(role, id)| ((*role).to_owned(), (*id).to_owned()))
                .collect(),
            before_all_seconds: None,
        }
    }
}

impl FixtureIds {
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }

    pub(crate) fn parse(text: &str) -> Result<Self, Error> {
        let value: Value = serde_json::from_str(text)?;
        let object = value
            .as_object()
            .ok_or_else(|| Error::new("fixture id file must be a JSON object"))?;
        let mut ids = Self::default();
        for (role, item) in object {
            if role == BEFORE_ALL_KEY {
                let seconds = item
                    .as_f64()
                    .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                    .ok_or_else(|| {
                        Error::new(format!("{BEFORE_ALL_KEY} must be a positive number"))
                    })?;
                ids.before_all_seconds = Some(seconds);
                continue;
            }
            let id = match item {
                Value::String(id) if !id.is_empty() => id.clone(),
                _ => {
                    return Err(Error::new(format!(
                        "fixture role {role} must be a non-empty string"
                    )));
                }
            };
            ids.roles.insert(role.clone(), id);
        }
        Ok(ids)
    }

    pub(crate) fn id(&self, role: &str) -> String {
        self.roles
            .get(role)
            .cloned()
            .unwrap_or_else(|| role.to_owned())
    }

    pub(crate) fn before_all_seconds(&self) -> Option<f64> {
        self.before_all_seconds
    }

    pub(crate) fn manifest_ids(&self) -> Vec<String> {
        ROLES.iter().map(|(role, _)| self.id(role)).collect()
    }

    pub(crate) fn rewrite(&self, text: &str) -> Option<String> {
        for (role, literal) in ROLES {
            if text == *literal {
                let mapped = self.id(role);
                if mapped == *literal {
                    return None;
                }
                return Some(mapped);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_map_keeps_the_other_literals() {
        let ids = FixtureIds::parse(r#"{"pass":"spec.ok","extra":"spec.other"}"#).unwrap();
        assert_eq!(ids.id("pass"), "spec.ok");
        assert_eq!(ids.id("fail"), "case-fail");
        assert_eq!(ids.id("extra"), "spec.other");
        assert_eq!(
            ids.manifest_ids(),
            vec!["spec.ok", "case-fail", "case-pend", "case-flake"]
        );
        assert_eq!(ids.rewrite("case-pass").as_deref(), Some("spec.ok"));
        assert_eq!(ids.rewrite("case-fail"), None);
        assert_eq!(ids.rewrite("{{extra}}"), None);
        assert!(FixtureIds::parse(r#"{"pass":""}"#).is_err());
        assert!(FixtureIds::parse("[]").is_err());
    }

    #[test]
    fn before_all_seconds_is_a_number_not_a_role() {
        let ids = FixtureIds::parse(r#"{"pass":"spec.ok","before_all_seconds":0.25}"#).unwrap();
        assert_eq!(ids.before_all_seconds(), Some(0.25));
        assert_eq!(ids.id("pass"), "spec.ok");
        assert_eq!(
            ids.manifest_ids(),
            vec!["spec.ok", "case-fail", "case-pend", "case-flake"]
        );
        assert_eq!(FixtureIds::default().before_all_seconds(), None);
        assert_eq!(
            FixtureIds::parse(r#"{"pass":"spec.ok"}"#)
                .unwrap()
                .before_all_seconds(),
            None
        );
        for bad in [
            r#"{"before_all_seconds":"0.2"}"#,
            r#"{"before_all_seconds":0}"#,
            r#"{"before_all_seconds":-1}"#,
        ] {
            assert!(FixtureIds::parse(bad).is_err(), "{bad}");
        }
    }
}
