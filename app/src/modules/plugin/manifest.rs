//! Plugin manifest: the model, its validation and the launch-args rules live
//! in `flwrs_plugin::package` (shared with `flwrs-trellis`); the server adds
//! its own check of `runtime_defaults` against its runtime settings.

pub(crate) use flwrs_plugin::package::{
    LaunchSpec, Manifest, PluginType, current_platform, is_valid_id, validate_launch_args,
};

/// The server's extra manifest check (`package::ExtraCheck`): `runtime_defaults`
/// uses the same keys and rules as a node's `config.runtime`.
pub(crate) fn check_runtime_defaults(manifest: &Manifest) -> Vec<String> {
    let Some(defaults) = &manifest.runtime_defaults else {
        return vec![];
    };
    if !defaults.is_object() {
        return vec![]; // reported by Manifest::validate
    }
    match crate::modules::node_config::RuntimeConfig::parse(defaults) {
        Ok(runtime) => runtime.validate("runtime_defaults"),
        Err(e) => vec![format!("runtime_defaults: {e}")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flwrs_plugin::package::{LaunchArgsStyle, PayloadSchemas, SchemaSource};
    use serde_json::json;

    #[test]
    fn runtime_defaults_follow_the_node_runtime_rules() {
        let mut m = Manifest {
            manifest_version: 1,
            id: "http-sink".into(),
            name: "HTTP sink".into(),
            version: "0.1.0".into(),
            plugin_type: PluginType::Sink,
            description: None,
            executable: "bin/plugin".into(),
            platforms: None,
            launch_args_schema: None,
            launch_args_style: LaunchArgsStyle::Flags,
            payload_schemas: PayloadSchemas {
                input: Some(SchemaSource::Path("schema/in.json".into())),
                output: None,
            },
            runtime_defaults: None,
            ui: None,
            configurator: None,
            threads: None,
            extensions: Default::default(),
        };
        assert!(check_runtime_defaults(&m).is_empty());
        m.runtime_defaults = Some(json!({"init_timeout_ms": 5000}));
        assert!(check_runtime_defaults(&m).is_empty());
        m.runtime_defaults = Some(json!({"init_timeout_ms": 0, "typo_ms": 1}));
        let errors = check_runtime_defaults(&m);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("runtime_defaults: unknown field `typo_ms`")),
            "{errors:?}"
        );
    }
}
