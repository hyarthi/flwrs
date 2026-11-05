//! Edge mappings (`scene_edges.mapping`). For now: the JSON shape and its
//! validation. Evaluation (dot notation, Lua) comes with the mapping phases.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const MAX_RULES: usize = 1_000;
pub(crate) const MAX_LUA_BYTES: usize = 64 * 1024;

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MappingMode {
    /// Start from the input payload and apply the rules.
    #[default]
    Merge,
    /// Start empty: only the rules' targets are sent.
    Replace,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LuaChunk {
    pub lua: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rule {
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lua: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct MappingSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<MappingMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<LuaChunk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<Rule>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<LuaChunk>,
}

impl MappingSpec {
    /// Parses and checks a mapping. `null` means pass-through (`Ok(None)`).
    pub(crate) fn parse(value: &Value) -> Result<Option<Self>, Vec<String>> {
        if value.is_null() {
            return Ok(None);
        }
        if !value.is_object() {
            return Err(vec!["mapping must be a JSON object or null".into()]);
        }
        let spec: Self =
            serde_json::from_value(value.clone()).map_err(|e| vec![format!("mapping: {e}")])?;
        let errors = spec.validate();
        if errors.is_empty() {
            Ok(Some(spec))
        } else {
            Err(errors)
        }
    }

    fn validate(&self) -> Vec<String> {
        let mut errors = vec![];
        let mut lua = |label: &str, code: &str| {
            if code.trim().is_empty() {
                errors.push(format!("{label}.lua is empty"));
            } else if code.len() > MAX_LUA_BYTES {
                errors.push(format!("{label}.lua is longer than {MAX_LUA_BYTES} bytes"));
            }
        };
        if let Some(filter) = &self.filter {
            lua("mapping.filter", &filter.lua);
        }
        if let Some(script) = &self.script {
            lua("mapping.script", &script.lua);
        }
        match (&self.rules, &self.script) {
            (Some(_), Some(_)) => {
                errors.push("mapping: use either rules or script, not both".into())
            }
            (None, Some(_)) if self.mode.is_some() => errors.push(
                "mapping.mode doesn't apply to a script (the script returns the whole output)"
                    .into(),
            ),
            _ => {}
        }
        if let Some(rules) = &self.rules {
            if rules.len() > MAX_RULES {
                errors.push(format!("mapping.rules has more than {MAX_RULES} rules"));
            }
            for (i, rule) in rules.iter().enumerate() {
                let label = format!("mapping.rules[{i}]");
                if rule.target.trim().is_empty() {
                    errors.push(format!("{label}.target is empty"));
                }
                let ops = [
                    rule.source.is_some(),
                    rule.value.is_some(),
                    rule.lua.is_some(),
                    rule.delete.is_some(),
                ]
                .iter()
                .filter(|set| **set)
                .count();
                if ops != 1 {
                    errors.push(format!(
                        "{label}: needs exactly one of source, value, lua or delete (a null value counts as unset; use delete to remove a field)"
                    ));
                }
                if rule.source.as_deref().is_some_and(|s| s.trim().is_empty()) {
                    errors.push(format!("{label}.source is empty"));
                }
                if rule.delete == Some(false) {
                    errors.push(format!(
                        "{label}.delete must be true (omit the rule to keep the field)"
                    ));
                }
                if let Some(code) = &rule.lua {
                    if code.trim().is_empty() {
                        errors.push(format!("{label}.lua is empty"));
                    } else if code.len() > MAX_LUA_BYTES {
                        errors.push(format!("{label}.lua is longer than {MAX_LUA_BYTES} bytes"));
                    }
                }
            }
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_mappings() {
        assert_eq!(MappingSpec::parse(&Value::Null).unwrap(), None);
        for ok in [
            json!({"mode": "merge", "rules": [
                {"target": "url", "source": "endpoint"},
                {"target": "method", "value": "POST"},
                {"target": "body", "lua": "return tostring(payload.t)"},
                {"target": "debug", "delete": true}
            ]}),
            json!({"filter": {"lua": "return payload.t > 30"}, "script": {"lua": "return payload"}}),
            json!({"filter": {"lua": "return true"}}),
            json!({"mode": "replace", "rules": []}),
        ] {
            assert!(MappingSpec::parse(&ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn invalid_mappings() {
        for (bad, fragment) in [
            (json!([1]), "must be a JSON object"),
            (json!({"rulez": []}), "unknown field"),
            (
                json!({"rules": [{"target": "a", "source": "b", "value": 1}]}),
                "exactly one of",
            ),
            (json!({"rules": [{"target": "a"}]}), "exactly one of"),
            (
                json!({"rules": [{"target": "a", "value": null}]}),
                "null value counts as unset",
            ),
            (
                json!({"rules": [{"target": " ", "source": "b"}]}),
                "target is empty",
            ),
            (
                json!({"rules": [{"target": "a", "delete": false}]}),
                "delete must be true",
            ),
            (
                json!({"rules": [], "script": {"lua": "return payload"}}),
                "either rules or script",
            ),
            (
                json!({"mode": "merge", "script": {"lua": "return payload"}}),
                "doesn't apply to a script",
            ),
            (json!({"filter": {"lua": "  "}}), "filter.lua is empty"),
            (
                json!({"rules": [{"target": "a", "lua": "x".repeat(MAX_LUA_BYTES + 1)}]}),
                "longer than",
            ),
            (json!({"mode": "upsert"}), "unknown variant"),
        ] {
            let errors = MappingSpec::parse(&bad).unwrap_err();
            assert!(
                errors.iter().any(|e| e.contains(fragment)),
                "{bad}: expected {fragment:?} in {errors:?}"
            );
        }
    }
}
