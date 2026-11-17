//! End-to-end tests of Lua in mappings (the sandbox is `flwrs_types::lua`).

#[cfg(test)]
mod tests {
    use super::super::compile::{Schemas, compile};
    use super::super::{MappingSpec, path::Path};
    use flwrs_plugin::schema::payload::PluginPayload;
    use flwrs_plugin::schema::payload::SchemaDefinition;
    use flwrs_plugin::schema::protocol::EventMetadata;
    use flwrs_plugin::schema::rules::parse_json;
    use flwrs_types::value::Value;
    use pbjson_types::Timestamp;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn spec(json: serde_json::Value) -> MappingSpec {
        MappingSpec::parse(&json).unwrap().unwrap()
    }

    fn target() -> SchemaDefinition {
        parse_json(
            r#"{"fields": [
                {"key": "text", "type": "STRING"},
                {"key": "n", "type": "I64"},
                {"key": "big", "type": "U64"},
                {"key": "when", "type": "DATETIME"},
                {"key": "raw", "type": "BYTES"},
                {"key": "list", "type": "ARRAY", "nested_type_definition": {"type": "I32"}},
                {"key": "tags", "type": "MAP", "nested_type_definition": {"type": "STRING"}}
            ]}"#,
        )
        .unwrap()
    }

    fn payload(fields: Vec<(&str, Value)>) -> PluginPayload {
        Value::Object(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
        .into_payload()
        .unwrap()
    }

    fn input() -> PluginPayload {
        payload(vec![
            ("text", Value::String("hello".into())),
            ("n", Value::I64(41)),
            (
                "when",
                Value::DateTime(Timestamp {
                    seconds: 1_700_000_000,
                    nanos: 123_000_000,
                }),
            ),
            ("list", Value::Array(vec![Value::I32(1), Value::I32(2)])),
        ])
    }

    /// Runs one Lua rule targeting `target` and returns the result there.
    fn rule(code: &str, target_field: &str) -> Result<Option<Value>, String> {
        let t = target();
        let mapping = compile(
            &spec(json!({"rules": [{"target": target_field, "lua": code}]})),
            Schemas {
                source: None,
                target: Some(&t),
            },
        )
        .map_err(|e| e.join("; "))?;
        let out = mapping.apply(&input(), None)?.expect("not filtered");
        Ok(Path::parse(target_field)
            .unwrap()
            .get(&Value::from_payload(&out))
            .cloned())
    }

    #[test]
    fn rules_see_the_payload_and_set_typed_values() {
        assert_eq!(
            rule("return payload.text:upper()", "text"),
            Ok(Some(Value::String("HELLO".into())))
        );
        assert_eq!(rule("return payload.n + 1", "n"), Ok(Some(Value::I64(42))));
        assert_eq!(rule("return #payload.list", "n"), Ok(Some(Value::I64(2))));
        assert_eq!(
            rule(
                "local s = 0 for _, v in ipairs(payload.list) do s = s + v end return s",
                "n"
            ),
            Ok(Some(Value::I64(3)))
        );
        // nil removes the field, like `delete`.
        assert_eq!(rule("return nil", "text"), Ok(None));
    }

    #[test]
    fn values_follow_the_target_schema() {
        // Datetimes are epoch milliseconds, both ways.
        assert_eq!(
            rule("return payload.when", "when"),
            Ok(Some(Value::DateTime(Timestamp {
                seconds: 1_700_000_000,
                nanos: 123_000_000
            })))
        );
        assert_eq!(
            rule("return payload.when // 1000", "n"),
            Ok(Some(Value::I64(1_700_000_000)))
        );
        // An empty table is whatever the target field is.
        assert_eq!(rule("return {}", "list"), Ok(Some(Value::Array(vec![]))));
        assert_eq!(
            rule("return {}", "tags"),
            Ok(Some(Value::Map(BTreeMap::new())))
        );
        assert_eq!(
            rule("return {a = 'x'}", "tags"),
            Ok(Some(Value::Map(BTreeMap::from([(
                "a".into(),
                Value::String("x".into())
            )]))))
        );
        // Lua strings are bytes for a BYTES field; not-UTF-8 isn't a STRING.
        assert_eq!(
            rule("return '\\xff\\x00'", "raw"),
            Ok(Some(Value::Bytes(vec![0xff, 0])))
        );
        assert!(
            rule("return '\\xff'", "text")
                .unwrap_err()
                .contains("isn't UTF-8")
        );
        assert!(
            rule("return {1, 2}", "text")
                .unwrap_err()
                .contains("can't be a STRING")
        );
        assert!(rule("return 1.5", "n").is_err(), "not a whole number");
    }

    #[test]
    fn u64_above_lua_integers_is_an_error() {
        let mapping = compile(
            &spec(json!({"rules": [{"target": "big", "lua": "return payload.big"}]})),
            Schemas::default(),
        )
        .unwrap();
        let input = payload(vec![("big", Value::U64(u64::MAX))]);
        assert!(
            mapping
                .apply(&input, None)
                .unwrap_err()
                .contains("above Lua's integer range")
        );
        let fits = payload(vec![("big", Value::U64(i64::MAX as u64))]);
        assert!(mapping.apply(&fits, None).is_ok());
    }

    #[test]
    fn script_and_filter() {
        let t = target();
        let mapping = compile(
            &spec(json!({
                "filter": {"lua": "return payload.n > 40"},
                "script": {"lua": "if payload.text == 'skip' then return nil end\nreturn { text = payload.text .. '!', n = payload.n * 2, extra = 1 }"}
            })),
            Schemas { source: None, target: Some(&t) },
        )
        .unwrap();
        let out = mapping.apply(&input(), None).unwrap().unwrap();
        let out = Value::from_payload(&out);
        assert_eq!(
            out,
            Value::Object(vec![
                ("text".into(), Value::String("hello!".into())),
                ("n".into(), Value::I64(82)),
            ]),
            "shaped to the schema (order, types; `extra` dropped)"
        );
        // The filter drops it.
        let low = payload(vec![
            ("n", Value::I64(1)),
            ("text", Value::String("x".into())),
        ]);
        assert_eq!(mapping.apply(&low, None), Ok(None));
        // `return nil` drops it too.
        let skip = payload(vec![
            ("n", Value::I64(50)),
            ("text", Value::String("skip".into())),
        ]);
        assert_eq!(mapping.apply(&skip, None), Ok(None));
    }

    #[test]
    fn meta_is_visible() {
        let mapping = compile(
            &spec(json!({"rules": [{"target": "text", "lua": "return meta.trace_id .. ':' .. meta.hops"}]})),
            Schemas::default(),
        )
        .unwrap();
        let meta = EventMetadata {
            trace_id: "t1".into(),
            hops: 3,
            ..Default::default()
        };
        let out = mapping.apply(&input(), Some(&meta)).unwrap().unwrap();
        assert_eq!(
            Path::parse("text").unwrap().get(&Value::from_payload(&out)),
            Some(&Value::String("t1:3".into()))
        );
    }

    #[test]
    fn the_sandbox_keeps_scripts_in() {
        for (code, expected) in [
            (
                "return io.open('/etc/passwd')",
                "attempt to index a nil value",
            ),
            ("return os.execute('true')", "attempt to index a nil value"),
            ("return require('os')", "attempt to call a nil value"),
            ("return load('return 1')()", "attempt to call a nil value"),
            (
                "return dofile('/etc/passwd')",
                "attempt to call a nil value",
            ),
            (
                "return loadfile('/etc/passwd')",
                "attempt to call a nil value",
            ),
            (
                "return collectgarbage('count')",
                "attempt to call a nil value",
            ),
            ("return debug.getinfo(1)", "attempt to index a nil value"),
            ("x = 1 return 'x'", "assigning globals isn't allowed"),
            ("payload.text = 'changed' return 'x'", "read-only"),
            ("payload.list[1] = 5 return 'x'", "read-only"),
            ("string.upper = nil return 'x'", "read-only"),
            (
                "setmetatable(payload, {}) return 'x'",
                "protected metatable",
            ),
        ] {
            let err = rule(code, "text").unwrap_err();
            assert!(err.contains(expected), "{code:?}: {err:?}");
        }
        // What's meant to be there works.
        assert_eq!(
            rule(
                "return string.format('%d', math.max(1, 2)) .. utf8.char(65)",
                "text"
            ),
            Ok(Some(Value::String("2A".into())))
        );
        assert_eq!(
            rule("return tostring(getmetatable(payload))", "text"),
            Ok(Some(Value::String("false".into())))
        );
    }

    #[test]
    fn limits_stop_runaway_scripts() {
        let run = |limits: serde_json::Value, code: &str| {
            let mapping = compile(
                &spec(json!({"lua": limits, "rules": [{"target": "text", "lua": code}]})),
                Schemas::default(),
            )
            .unwrap();
            mapping.apply(&input(), None).unwrap_err()
        };
        let err = run(json!({}), "while true do end");
        assert!(err.contains("stopped after 1000000 instructions"), "{err}");
        let err = run(
            json!({"max_instructions": 1_000_000_000, "timeout_ms": 20}),
            "while true do end",
        );
        assert!(err.contains("stopped after 20 ms"), "{err}");
        let err = run(
            json!({"max_memory_bytes": 262_144}),
            "local t = {} for i = 1, 1e7 do t[i] = i end return 'x'",
        );
        assert!(err.contains("more than 262144 bytes of memory"), "{err}");
        // The state stays usable after a limit was hit.
        let mapping = compile(
            &spec(json!({"rules": [{"target": "text", "lua": "if payload.n > 100 then while true do end end return 'ok'"}]})),
            Schemas::default(),
        )
        .unwrap();
        let big = payload(vec![("n", Value::I64(101))]);
        assert!(mapping.apply(&big, None).is_err());
        assert!(mapping.apply(&input(), None).unwrap().is_some());
    }

    #[test]
    fn syntax_errors_and_limits_are_reported_at_compile_time() {
        let errors = compile(
            &spec(json!({"filter": {"lua": "return ("}, "rules": [{"target": "a", "lua": "return 1 +"}]})),
            Schemas::default(),
        )
        .unwrap_err();
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors[0].starts_with("mapping.filter.lua: mapping.filter:1:"),
            "{errors:?}"
        );
        assert!(errors[1].starts_with("mapping.rules[0].lua:"), "{errors:?}");
        let bad_limits =
            MappingSpec::parse(&json!({"lua": {"timeout_ms": 0}, "rules": []})).unwrap_err();
        assert!(bad_limits[0].contains("mapping.lua.timeout_ms must be between 1 and 10000"));
    }
}
