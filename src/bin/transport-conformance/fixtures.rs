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

#[derive(Clone, Debug, PartialEq)]
pub struct FixtureIds {
    roles: BTreeMap<String, String>,
}

impl Default for FixtureIds {
    fn default() -> Self {
        Self {
            roles: ROLES
                .iter()
                .map(|(role, id)| ((*role).to_owned(), (*id).to_owned()))
                .collect(),
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
}
