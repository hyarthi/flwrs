//! Trying an edge mapping out (`POST …/mapping/test`), for the mapping
//! editor: the two nodes' payload schemas, the mapping's problems against
//! them, and what it makes of a sample payload. Nothing is saved or sent.

use super::*;
use crate::modules::mapping::{self, MappingSpec, Schemas};
use flwrs_plugin::schema::payload::{FieldDefinition, field_type::Enum as FieldType};
use flwrs_plugin::schema::protocol::EventMetadata;
use flwrs_types::{json, value};

/// A payload schema field, as the editor shows it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct SchemaField {
    pub key: String,
    /// The field type's name (`STRING`, `U64`, `OBJECT`, …).
    #[serde(rename = "type")]
    pub field_type: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub content_type: String,
    /// ARRAY elements, MAP values.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub element: Option<Box<SchemaField>>,
    /// OBJECT fields.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(no_recursion)]
    pub fields: Vec<SchemaField>,
}

impl SchemaField {
    /// A payload schema's fields as trees.
    pub(crate) fn list(schema: &flwrs_plugin::schema::payload::SchemaDefinition) -> Vec<Self> {
        schema.fields.iter().map(Self::of).collect()
    }

    fn of(def: &FieldDefinition) -> Self {
        Self {
            key: def.key.clone(),
            field_type: FieldType::try_from(def.r#type)
                .map(|t| t.as_str_name().to_string())
                .unwrap_or_else(|_| "UNDEFINED".into()),
            description: def.description.clone(),
            content_type: def.content_type.clone(),
            element: def
                .nested_type_definition
                .as_deref()
                .map(|d| Box::new(Self::of(d))),
            fields: def.object_fields.iter().map(Self::of).collect(),
        }
    }
}

/// What a sample payload became.
#[derive(Debug, Clone, PartialEq, serde::Serialize, utoipa::ToSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum TestOutcome {
    /// The payload sent along the edge, as JSON (bytes as base64,
    /// datetimes as RFC 3339).
    Output {
        #[schema(value_type = Object)]
        payload: serde_json::Value,
    },
    /// The filter (or a script / Lua rule returning nothing) dropped it.
    Filtered,
    /// The sample doesn't fit the source schema, or the mapping failed on it
    /// (counted as a mapping error when it runs).
    Error { message: String },
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct MappingTest {
    /// The source node's `out` schema; `null` = unknown (its plugin isn't
    /// installed, or its own schema is missing).
    pub source: Option<Vec<SchemaField>>,
    /// The target node's `in` schema.
    pub target: Option<Vec<SchemaField>>,
    /// Problems with the mapping (as saving it would report them).
    pub problems: Vec<String>,
    /// For a sample payload, if one was given and the mapping compiled.
    pub result: Option<TestOutcome>,
}

impl Service {
    /// Compiles `mapping` for the edge `source → target` (nodes of the live
    /// graph or the draft) and runs it on `payload` (JSON shaped by the
    /// source schema; BYTES as base64), if given.
    pub(crate) async fn test_mapping(
        &self,
        scene_id: &str,
        store: Store,
        source_id: &str,
        target_id: &str,
        mapping: &serde_json::Value,
        payload: Option<&serde_json::Value>,
    ) -> Result<MappingTest, ServiceError> {
        let source = self.get_node_in(store, scene_id, source_id).await?;
        let target = self.get_node_in(store, scene_id, target_id).await?;
        let (source_schema, target_schema) =
            self.node_schemas(Some(&source), Some(&target)).await?;
        let mut test = MappingTest {
            source: source_schema
                .as_ref()
                .map(|s| s.fields.iter().map(SchemaField::of).collect()),
            target: target_schema
                .as_ref()
                .map(|s| s.fields.iter().map(SchemaField::of).collect()),
            problems: vec![],
            result: None,
        };
        let spec = match MappingSpec::parse(mapping) {
            Ok(spec) => spec,
            Err(problems) => {
                test.problems = problems;
                return Ok(test);
            }
        };
        let compiled = match &spec {
            Some(spec) => match mapping::compile(
                spec,
                Schemas {
                    source: source_schema.as_ref(),
                    target: target_schema.as_ref(),
                },
            ) {
                Ok(compiled) => Some(compiled),
                Err(problems) => {
                    test.problems = problems;
                    return Ok(test);
                }
            },
            None => None,
        };
        let Some(payload) = payload else {
            return Ok(test);
        };

        // The sample as the source node would emit it.
        let input = match sample(payload, source_schema.as_ref()) {
            Ok(input) => input,
            Err(message) => {
                test.result = Some(TestOutcome::Error {
                    message: format!("the sample doesn't fit the source's schema: {message}"),
                });
                return Ok(test);
            }
        };
        let meta = EventMetadata {
            event_id: "test-event".into(),
            trace_id: "test-trace".into(),
            hops: 1,
            origin_node_id: source.id.clone(),
            create_time: Some(crate::modules::director::running::now()),
        };
        // Lua has its own limits, but runs on this thread: off the runtime.
        let outcome = tokio::task::spawn_blocking(move || match compiled {
            None => Ok(Some(input)),
            Some(m) => m.apply(&input, Some(&meta)),
        })
        .await
        .map_err(|e| ServiceError::Unknown(format!("mapping test: {e}")))?;
        test.result = Some(match outcome {
            Ok(Some(out)) => match json::to_json(&value::Value::from_payload(&out)) {
                Ok(payload) => TestOutcome::Output { payload },
                Err(message) => TestOutcome::Error { message },
            },
            Ok(None) => TestOutcome::Filtered,
            Err(message) => TestOutcome::Error { message },
        });
        Ok(test)
    }
}

/// A JSON sample as a payload: shaped by the schema when it's known.
fn sample(
    json: &serde_json::Value,
    schema: Option<&flwrs_plugin::schema::payload::SchemaDefinition>,
) -> Result<flwrs_plugin::schema::payload::PluginPayload, String> {
    if !json.is_object() {
        return Err("a payload is a JSON object".into());
    }
    let def = schema.map(|s| FieldDefinition {
        r#type: FieldType::Object as i32,
        object_fields: s.fields.clone(),
        ..Default::default()
    });
    let value = value::from_json_base64(json, def.as_ref())?;
    let value = match schema {
        Some(schema) => value::coerce_payload(value, schema)?,
        None => value,
    };
    value.into_payload()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::scene::service::tests::{fixture, node_input};
    use serde_json::json;

    #[tokio::test]
    async fn shows_schemas_problems_and_what_a_sample_becomes() {
        let f = fixture().await;
        let s = f.scene().await;
        // test-script takes and sends the schemas in its launch args.
        let schemas = json!({
            "in": {"fields": [{"key": "n", "type": "U64"}, {"key": "label", "type": "STRING"}]},
            "out": {"fields": [{"key": "n", "type": "U64"}, {"key": "label", "type": "STRING"}]}
        });
        let source = f
            .service
            .create_node(
                &s.id,
                node_input(
                    "test-script",
                    json!({"launch_args": {"script": "return payload", "schemas": schemas}}),
                ),
            )
            .await
            .unwrap();
        let target = f
            .service
            .create_node(
                &s.id,
                node_input(
                    "test-script",
                    json!({"launch_args": {"script": "return payload", "schemas": schemas}}),
                ),
            )
            .await
            .unwrap();
        let run = |mapping: serde_json::Value, payload: Option<serde_json::Value>| {
            let (s, a, b) = (s.id.clone(), source.id.clone(), target.id.clone());
            async move {
                f.service
                    .test_mapping(&s, Store::Live, &a, &b, &mapping, payload.as_ref())
                    .await
                    .unwrap()
            }
        };

        let schemas_only = run(serde_json::Value::Null, None).await;
        assert_eq!(schemas_only.source.as_ref().unwrap()[0].key, "n");
        assert_eq!(
            schemas_only.target.as_ref().unwrap()[1].field_type,
            "STRING"
        );
        assert!(schemas_only.result.is_none());

        let bad = run(json!({"rules": [{"target": "nope", "value": 1}]}), None).await;
        assert!(bad.problems[0].contains("nope"), "{:?}", bad.problems);

        let set = run(
            json!({"rules": [{"target": "label", "value": "set"}]}),
            Some(json!({"n": 7, "label": "x"})),
        )
        .await;
        assert_eq!(
            set.result,
            Some(TestOutcome::Output {
                payload: json!({"n": 7, "label": "set"})
            })
        );

        let unfit = run(serde_json::Value::Null, Some(json!({"n": "seven"}))).await;
        assert!(
            matches!(unfit.result, Some(TestOutcome::Error { .. })),
            "{unfit:?}"
        );

        #[cfg(feature = "mapping-lua")]
        {
            let filtered = run(
                json!({"filter": {"lua": "return payload.n > 10"}}),
                Some(json!({"n": 7, "label": "x"})),
            )
            .await;
            assert_eq!(filtered.result, Some(TestOutcome::Filtered));
        }
    }
}
