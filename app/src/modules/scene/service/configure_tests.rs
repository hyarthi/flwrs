//! Configurators (manifest `configurator`) at config time: run on save, kept
//! when nothing changed, refused values, failures, remote nodes, uploads,
//! returned files, `check`, re-running, and plugin upgrades. The
//! configurator is a shell script (Unix only).

use super::tests::{Fixture, node_input};
use super::*;
use crate::modules::plugin::package::tests::{Item, PLATFORM, build_zip};
use serde_json::json;

const PLUGIN: &str = "test-conf";

const LAUNCH_SCHEMA: &str = r#"{"type": "object", "additionalProperties": false, "properties": {
    "spec": {"type": "string"},
    "doc": {"type": "string", "x-flwrs-launch": {"via": "file", "source": "upload"}},
    "out_schema": {"type": "object"},
    "descriptor": {"type": "string", "x-flwrs-launch": {"via": "file", "source": "upload"}}}}"#;

/// The configurator: `spec` "fail…" fails; "warn…" adds a warning; "files"
/// also returns a file. The out schema has one field named after `spec`, described as
/// `<version>|<the uploaded doc>|<its pid>` (each run differs).
fn script(version: &str) -> String {
    format!(
        r#"#!/bin/sh
spec=$(sed -n 's/.*"spec": "\([^"]*\)".*/\1/p' request.json)
case "$spec" in fail*) echo '{{"errors": ["spec says fail"]}}' > response.json; exit 1;; esac
doc=""
[ -f config/doc.yaml ] && doc=$(cat config/doc.yaml)
files='{{}}'
if [ "$spec" = files ]; then printf 'bin' > descriptor.pb; files='{{"descriptor": "descriptor.pb"}}'; fi
warnings='[]'
case "$spec" in warn*) warnings='["spec says warn"]';; esac
printf '{{"values": {{"out_schema": {{"fields": [{{"key": "%s", "type": "STRING", "description": "{version}|%s|%s"}}]}}}}, "files": %s, "warnings": %s}}' "$spec" "$doc" "$$" "$files" "$warnings" > response.json
"#
    )
}

fn package(f: &Fixture, version: &str, configurator: &str) -> std::path::PathBuf {
    let path = f.plugins.service.new_upload_path();
    let manifest = format!(
        r#"{{"manifest_version": 2, "id": "{PLUGIN}", "name": "Conf", "version": "{version}", "type": "SOURCE",
            "executable": "bin/run", "platforms": ["{PLATFORM}"], "launch_args_schema": "schema/launch.json",
            "payload_schemas": {{"out": {{"from_launch_args": "/out_schema"}}}},
            "configurator": {{"executable": "bin/configure", "fields": ["out_schema", "descriptor"]}}}}"#
    );
    build_zip(
        &path,
        &[
            Item::File("manifest.json", manifest.as_bytes()),
            Item::Dir("bin/"),
            Item::File("bin/run", b"#!/bin/sh\n"),
            Item::File("bin/configure", configurator.as_bytes()),
            Item::File("schema/launch.json", LAUNCH_SCHEMA.as_bytes()),
        ],
    );
    path
}

async fn setup() -> (Fixture, Scene) {
    setup_with(|_| {}).await
}

async fn setup_with(
    config: impl FnOnce(&mut crate::modules::plugin::config::PluginsConfig),
) -> (Fixture, Scene) {
    let f = super::tests::fixture_with(config).await;
    f.plugins.service.set_configure_env(configurator::Env {
        files_dir: f.service.files().files_dir.clone(),
        limits: f.service.configure_limits(),
    });
    f.plugins
        .service
        .install(&package(&f, "1.0.0", &script("1.0.0")))
        .await
        .unwrap();
    let scene = f.scene().await;
    (f, scene)
}

/// The configured field's description: `<version>|<doc>|<pid>`.
fn marker(node: &Node) -> String {
    node.config.launch_args["out_schema"]["fields"][0]["description"]
        .as_str()
        .unwrap()
        .to_string()
}

fn invalid(e: ServiceError) -> Vec<String> {
    match e {
        ServiceError::Invalid(errors) => errors,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[tokio::test]
async fn runs_on_save_and_only_when_the_settings_change() {
    let (f, scene) = setup().await;
    let node = f
        .node(&scene.id, PLUGIN, json!({"launch_args": {"spec": "a"}}))
        .await;
    assert_eq!(
        node.config.launch_args["out_schema"]["fields"][0]["key"],
        "a"
    );
    assert!(marker(&node).starts_with("1.0.0||"));
    // The same settings again (the stored value echoed back): not run again.
    let same = f
        .service
        .update_node(
            &scene.id,
            &node.id,
            node_input(
                PLUGIN,
                json!({"launch_args": node.config.launch_args.clone()}),
            ),
        )
        .await
        .unwrap();
    assert_eq!(marker(&same), marker(&node));
    let omitted = f
        .service
        .update_node(
            &scene.id,
            &node.id,
            node_input(PLUGIN, json!({"launch_args": {"spec": "a"}})),
        )
        .await
        .unwrap();
    assert_eq!(marker(&omitted), marker(&node));
    // Other settings: run again.
    let changed = f
        .service
        .update_node(
            &scene.id,
            &node.id,
            node_input(PLUGIN, json!({"launch_args": {"spec": "b"}})),
        )
        .await
        .unwrap();
    assert_eq!(
        changed.config.launch_args["out_schema"]["fields"][0]["key"],
        "b"
    );
    // A value of its own for a configured field: refused.
    let mut args = changed.config.launch_args.clone();
    args["out_schema"] = json!({"fields": []});
    let e = invalid(
        f.service
            .update_node(
                &scene.id,
                &node.id,
                node_input(PLUGIN, json!({"launch_args": args})),
            )
            .await
            .unwrap_err(),
    );
    assert!(e[0].contains("set by the plugin's configurator"), "{e:?}");
}

#[tokio::test]
async fn check_shows_the_warnings_of_the_run_behind_the_settings() {
    let (f, scene) = setup().await;
    let warned = |c: SceneCheck| {
        c.warnings
            .into_iter()
            .filter(|w| w.contains("configurator (run "))
            .collect::<Vec<_>>()
    };
    let node = f
        .node(&scene.id, PLUGIN, json!({"launch_args": {"spec": "warn"}}))
        .await;
    let found = warned(f.service.check(&scene.id).await.unwrap());
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("spec says warn"), "{found:?}");
    // Kept settings keep their run's warnings.
    f.service
        .update_node(
            &scene.id,
            &node.id,
            node_input(PLUGIN, json!({"launch_args": {"spec": "warn"}})),
        )
        .await
        .unwrap();
    assert_eq!(warned(f.service.check(&scene.id).await.unwrap()).len(), 1);
    // A draft run: the draft shows its own run, live still its own.
    f.service
        .reconfigure_node_in(Store::Draft, &scene.id, &node.id)
        .await
        .unwrap();
    assert_eq!(
        warned(f.service.check_draft(&scene.id).await.unwrap()).len(),
        1
    );
    assert_eq!(warned(f.service.check(&scene.id).await.unwrap()).len(), 1);
    f.service.discard_draft(&scene.id).await.unwrap();
    // A run without warnings: none shown.
    f.service
        .update_node(
            &scene.id,
            &node.id,
            node_input(PLUGIN, json!({"launch_args": {"spec": "b"}})),
        )
        .await
        .unwrap();
    assert!(warned(f.service.check(&scene.id).await.unwrap()).is_empty());
    // Gone with the node.
    f.service.delete_node(&scene.id, &node.id).await.unwrap();
    let mut conn = f.service.conn().await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM scene_node_configure_warnings")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn a_draft_keeps_settings_its_configurator_refuses() {
    let (f, scene) = setup().await;
    let node = f
        .service
        .create_node(
            &scene.id,
            node_input(PLUGIN, json!({"launch_args": {"spec": "good"}})),
        )
        .await
        .unwrap();
    assert!(node.config.launch_args.get("out_schema").is_some());
    let live_args = node.config.launch_args.clone();

    // Live still refuses.
    let e = f
        .service
        .update_node(
            &scene.id,
            &node.id,
            node_input(PLUGIN, json!({"launch_args": {"spec": "fail live"}})),
        )
        .await;
    assert!(e.is_err());

    // A draft keeps it, without the values of the earlier settings.
    let saved = f
        .service
        .save_node(
            Store::Draft,
            &scene.id,
            Some(&node.id),
            node_input(PLUGIN, json!({"launch_args": {"spec": "fail now"}})),
        )
        .await
        .unwrap();
    assert_eq!(saved.config.launch_args, json!({"spec": "fail now"}));
    let check = f.service.check_draft(&scene.id).await.unwrap();
    let said: Vec<_> = check
        .problems
        .iter()
        .filter(|p| p.contains("spec says fail"))
        .collect();
    assert_eq!(said.len(), 1, "{:?}", check.problems);
    assert!(said[0].contains("aren't set"), "{said:?}");
    assert!(
        !check.problems.iter().any(|p| p.contains("hasn't set")),
        "said once"
    );
    assert_eq!(
        f.service
            .get_node(&scene.id, &node.id)
            .await
            .unwrap()
            .config
            .launch_args,
        live_args
    );

    // Saved again unchanged, it's still reported.
    f.service
        .save_node(
            Store::Draft,
            &scene.id,
            Some(&node.id),
            node_input(PLUGIN, json!({"launch_args": {"spec": "fail now"}})),
        )
        .await
        .unwrap();
    assert!(
        f.service
            .check_draft(&scene.id)
            .await
            .unwrap()
            .problems
            .iter()
            .any(|p| p.contains("spec says fail"))
    );

    // A value of its own for a configured field is still refused.
    assert!(
        f.service
            .save_node(
                Store::Draft,
                &scene.id,
                Some(&node.id),
                node_input(
                    PLUGIN,
                    json!({"launch_args": {"spec": "x", "out_schema": {"fields": []}}})
                ),
            )
            .await
            .is_err()
    );

    // Fixed: configured again, and the problem is gone.
    let fixed = f
        .service
        .save_node(
            Store::Draft,
            &scene.id,
            Some(&node.id),
            node_input(PLUGIN, json!({"launch_args": {"spec": "good again"}})),
        )
        .await
        .unwrap();
    assert!(fixed.config.launch_args.get("out_schema").is_some());
    assert!(
        !f.service
            .check_draft(&scene.id)
            .await
            .unwrap()
            .problems
            .iter()
            .any(|p| p.contains("spec says fail"))
    );
}

#[tokio::test]
async fn a_failing_configurator_refuses_the_node() {
    let (f, scene) = setup().await;
    let e = invalid(
        f.service
            .create_node(
                &scene.id,
                node_input(PLUGIN, json!({"launch_args": {"spec": "fail now"}})),
            )
            .await
            .unwrap_err(),
    );
    assert_eq!(e, ["configurator: spec says fail"]);
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn remote_nodes_set_the_fields_themselves() {
    let (f, scene) = setup().await;
    let out = json!({"fields": [{"key": "manual", "type": "STRING"}]});
    let node = f
        .node(&scene.id, PLUGIN, json!({"launch": "remote", "token": "0123456789abcdef", "launch_args": {"out_schema": out}}))
        .await;
    assert_eq!(node.config.launch_args["out_schema"], out);
    let e = invalid(
        f.service
            .reconfigure_node(&scene.id, &node.id)
            .await
            .unwrap_err(),
    );
    assert!(e[0].contains("without config.configurator"), "{e:?}");
}

#[tokio::test]
async fn returned_files_become_uploads() {
    let (f, scene) = setup().await;
    let node = f
        .node(&scene.id, PLUGIN, json!({"launch_args": {"spec": "files"}}))
        .await;
    assert_eq!(node.config.launch_args["descriptor"], "descriptor.pb");
    let uploads = f.service.list_uploads(&scene.id, &node.id).await.unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].name, "descriptor.pb");
    assert_eq!(uploads[0].size, 3);
    let (_, path) = f
        .service
        .upload_content(&scene.id, &node.id, "descriptor.pb")
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"bin");
}

#[tokio::test]
async fn a_named_upload_runs_it_when_it_arrives_and_when_it_changes() {
    let (f, scene) = setup().await;
    // The file isn't there yet (a new node has no uploads): saved without
    // its configured settings, which `check` reports.
    let node = f
        .node(
            &scene.id,
            PLUGIN,
            json!({"launch_args": {"spec": "a", "doc": "doc.yaml"}}),
        )
        .await;
    assert!(node.config.launch_args.get("out_schema").is_none());
    let check = f.service.check(&scene.id).await.unwrap();
    assert!(
        check
            .problems
            .iter()
            .any(|p| p.contains("hasn't set its configured settings")),
        "{check:?}"
    );
    let put = |text: &'static str| {
        let (f, scene, node) = (&f, &scene, &node);
        async move {
            let incoming = f.service.incoming_path().await.unwrap();
            std::fs::write(&incoming, text).unwrap();
            f.service
                .put_upload(&scene.id, &node.id, "doc.yaml", &incoming)
                .await
        }
    };
    put("v1").await.unwrap();
    let node = f.service.get_node(&scene.id, &node.id).await.unwrap();
    assert!(marker(&node).starts_with("1.0.0|v1|"));
    put("v2").await.unwrap();
    let node = f.service.get_node(&scene.id, &node.id).await.unwrap();
    assert!(marker(&node).starts_with("1.0.0|v2|"));
    assert!(
        f.service
            .check(&scene.id)
            .await
            .unwrap()
            .problems
            .is_empty()
    );
    // A file the configurator refuses (here: the settings make it fail) isn't stored.
    let failing = f
        .service
        .update_node(
            &scene.id,
            &node.id,
            node_input(
                PLUGIN,
                json!({"launch_args": {"spec": "fail", "doc": "doc.yaml"}}),
            ),
        )
        .await;
    assert!(failing.is_err());
}

#[tokio::test]
async fn check_reports_a_stale_configuration_and_configure_fixes_it() {
    let (f, scene) = setup().await;
    let node = f
        .node(&scene.id, PLUGIN, json!({"launch_args": {"spec": "a"}}))
        .await;
    // As if imported from a server with another version of the plugin.
    let Database::SQLite(db) = f.plugins.service.db();
    let mut conn = db.get_connection().await.unwrap();
    sqlx::query("UPDATE scene_nodes SET plugin_version = '0.9.0' WHERE id = $1")
        .bind(&node.id)
        .execute(&mut *conn)
        .await
        .unwrap();
    let check = f.service.check(&scene.id).await.unwrap();
    assert!(
        check
            .problems
            .iter()
            .any(|p| p.contains("another version") && p.contains("/configure")),
        "{check:?}"
    );
    let again = f
        .service
        .reconfigure_node(&scene.id, &node.id)
        .await
        .unwrap();
    assert_ne!(marker(&again), marker(&node));
    assert_eq!(again.plugin_version.as_deref(), Some("1.0.0"));
    assert!(
        f.service
            .check(&scene.id)
            .await
            .unwrap()
            .problems
            .is_empty()
    );
}

#[tokio::test]
async fn a_draft_node_is_configured_again_in_the_draft_only() {
    let (f, scene) = setup().await;
    let node = f
        .node(&scene.id, PLUGIN, json!({"launch_args": {"spec": "a"}}))
        .await;
    let revision = f.service.get_scene(&scene.id).await.unwrap().revision;
    let draft = f
        .service
        .reconfigure_node_in(Store::Draft, &scene.id, &node.id)
        .await
        .unwrap();
    assert_ne!(marker(&draft), marker(&node), "the configurator ran again");
    // Live is as it was; the draft has the new configuration.
    assert_eq!(
        f.service.get_scene(&scene.id).await.unwrap().revision,
        revision
    );
    assert_eq!(
        marker(&f.service.get_node(&scene.id, &node.id).await.unwrap()),
        marker(&node)
    );
    let in_draft = f
        .service
        .get_node_in(Store::Draft, &scene.id, &node.id)
        .await
        .unwrap();
    assert_eq!(marker(&in_draft), marker(&draft));
}

#[tokio::test]
async fn an_upgrade_skips_nodes_whose_configurator_already_refused() {
    let (f, scene) = setup().await;
    let good = f
        .node(&scene.id, PLUGIN, json!({"launch_args": {"spec": "a"}}))
        .await;
    // Unfinished work, taken by the (disabled) scene from a draft.
    let unfinished = f
        .service
        .save_node(
            Store::Draft,
            &scene.id,
            None,
            node_input(PLUGIN, json!({"launch_args": {"spec": "fail later"}})),
        )
        .await
        .unwrap();
    f.service
        .submit_draft(&scene.id, None, false)
        .await
        .unwrap();
    let refused = |c: &SceneCheck| {
        c.problems
            .iter()
            .any(|p| p.contains(&unfinished.id) && p.contains("spec says fail"))
    };
    assert!(refused(&f.service.check(&scene.id).await.unwrap()));

    // A version that breaks the good node is still refused, naming only it.
    let breaking = package(
        &f,
        "2.0.0",
        "#!/bin/sh\necho '{\"errors\": [\"no\"]}' > response.json\n",
    );
    let e = format!(
        "{:?}",
        f.plugins
            .service
            .upgrade(PLUGIN, &breaking)
            .await
            .unwrap_err()
    );
    assert!(
        e.contains(&good.id) && e.contains("configurator: no"),
        "{e}"
    );
    assert!(!e.contains(&unfinished.id), "{e}");

    // One that works: the good node is re-configured, the unfinished one
    // skipped, and both take the new version.
    f.plugins
        .service
        .upgrade(PLUGIN, &package(&f, "2.0.0", &script("2.0.0")))
        .await
        .unwrap();
    let upgraded = f.service.get_node(&scene.id, &good.id).await.unwrap();
    assert!(
        marker(&upgraded).starts_with("2.0.0||"),
        "{}",
        marker(&upgraded)
    );
    let skipped = f.service.get_node(&scene.id, &unfinished.id).await.unwrap();
    assert_eq!(skipped.plugin_version.as_deref(), Some("2.0.0"));
    assert_eq!(skipped.config.launch_args, json!({"spec": "fail later"}));
    let check = f.service.check(&scene.id).await.unwrap();
    assert!(refused(&check), "{check:?}");
    assert!(
        !check.problems.iter().any(|p| p.contains("another version")),
        "{check:?}"
    );
}

#[tokio::test]
async fn an_upgrade_runs_the_new_configurator() {
    let (f, scene) = setup().await;
    let node = f
        .node(&scene.id, PLUGIN, json!({"launch_args": {"spec": "a"}}))
        .await;
    // A version whose configurator fails: the upgrade is refused.
    let failing = package(
        &f,
        "2.0.0",
        "#!/bin/sh\necho '{\"errors\": [\"no\"]}' > response.json\n",
    );
    let e = f
        .plugins
        .service
        .upgrade(PLUGIN, &failing)
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("configurator: no") || format!("{e:?}").contains("configurator: no"),
        "{e:?}"
    );
    // One that works: the node is re-configured with it.
    f.plugins
        .service
        .upgrade(PLUGIN, &package(&f, "2.0.0", &script("2.0.0")))
        .await
        .unwrap();
    let upgraded = f.service.get_node(&scene.id, &node.id).await.unwrap();
    assert!(
        marker(&upgraded).starts_with("2.0.0||"),
        "{}",
        marker(&upgraded)
    );
    assert_eq!(upgraded.plugin_version.as_deref(), Some("2.0.0"));
    assert!(
        f.service
            .check(&scene.id)
            .await
            .unwrap()
            .problems
            .is_empty()
    );
}

/// Remote nodes' configurator endpoints (`config.configurator`): an HTTP
/// server in the test answers the contract as JSON.
#[cfg(feature = "remote")]
mod remote {
    use super::*;
    use axum::Json;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Seen {
        calls: AtomicUsize,
        last: std::sync::Mutex<Option<(Option<String>, Value)>>,
    }

    /// `/configure` answers by `spec` ("files": a file by URL, "fail": a
    /// 422); `/files/api.pb` serves the file. Only `expected` auth passes.
    async fn endpoint(expected: Option<&'static str>) -> (String, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(expected, format!("{base}/files/api.pb"), seen.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, seen)
    }

    fn router(expected: Option<&'static str>, file_url: String, seen: Arc<Seen>) -> axum::Router {
        axum::Router::new()
            .route(
                "/configure",
                post(move |State(seen): State<Arc<Seen>>, headers: HeaderMap, Json(request): Json<Value>| {
                    let file_url = file_url.clone();
                    async move {
                        let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
                        if expected.is_some() && auth.as_deref() != expected {
                            return (StatusCode::UNAUTHORIZED, Json(json!({}))).into_response();
                        }
                        seen.calls.fetch_add(1, Ordering::SeqCst);
                        *seen.last.lock().unwrap() = Some((auth, request.clone()));
                        let spec = request["launch_args"]["spec"].as_str().unwrap_or_default().to_string();
                        if spec == "fail" {
                            return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"errors": ["no"]}))).into_response();
                        }
                        let mut answer = json!({"values": {"out_schema": {"fields": [{"key": spec, "type": "STRING"}]}}});
                        if spec == "files" {
                            answer["files"] = json!({"descriptor": {"name": "api.pb", "url": file_url}});
                        }
                        Json(answer).into_response()
                    }
                }),
            )
            .route("/files/api.pb", get(|| async { "remote-bin" }))
            .with_state(seen)
    }

    use axum::response::IntoResponse;

    fn remote_node(url: &str, spec: &str, auth: Value) -> Value {
        json!({"launch": "remote", "token": "0123456789abcdef",
               "launch_args": {"spec": spec},
               "configurator": {"url": format!("{url}/configure"), "auth": auth}})
    }

    #[tokio::test]
    async fn the_endpoint_configures_a_remote_node() {
        let (url, seen) = endpoint(Some("Bearer t0ken")).await;
        let allow = url.clone();
        let (f, scene) = setup_with(move |c| c.remote_configurator_urls = vec![allow]).await;
        let node = f
            .node(
                &scene.id,
                PLUGIN,
                remote_node(&url, "a", json!({"type": "bearer", "token": "t0ken"})),
            )
            .await;
        assert_eq!(
            node.config.launch_args["out_schema"]["fields"][0]["key"],
            "a"
        );
        let (_, request) = seen.last.lock().unwrap().clone().unwrap();
        assert_eq!(request["contract"], 1);
        assert_eq!(request["fields"], json!(["out_schema", "descriptor"]));
        // The token never comes back.
        let shown = node.config.redacted();
        assert!(!shown.to_string().contains("t0ken"), "{shown}");
        // Saved again unchanged (the token left out: kept): not run again.
        let mut same = node.config.redacted();
        same["token"] = json!("0123456789abcdef");
        f.service
            .update_node(&scene.id, &node.id, node_input(PLUGIN, same))
            .await
            .unwrap();
        assert_eq!(seen.calls.load(Ordering::SeqCst), 1);
        // Other settings: run again.
        let changed = f
            .service
            .update_node(
                &scene.id,
                &node.id,
                node_input(
                    PLUGIN,
                    remote_node(&url, "b", json!({"type": "bearer", "token": "t0ken"})),
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            changed.config.launch_args["out_schema"]["fields"][0]["key"],
            "b"
        );
        assert_eq!(seen.calls.load(Ordering::SeqCst), 2);
        // A refusal and a wrong token: the change is refused.
        let e = f
            .service
            .update_node(
                &scene.id,
                &node.id,
                node_input(
                    PLUGIN,
                    remote_node(&url, "fail", json!({"type": "bearer", "token": "t0ken"})),
                ),
            )
            .await
            .unwrap_err();
        assert!(format!("{e:?}").contains("422"), "{e:?}");
        let e = f
            .service
            .update_node(
                &scene.id,
                &node.id,
                node_input(
                    PLUGIN,
                    remote_node(&url, "c", json!({"type": "bearer", "token": "wrong"})),
                ),
            )
            .await
            .unwrap_err();
        assert!(format!("{e:?}").contains("401"), "{e:?}");
        // Plain http: a warning in check.
        let check = f.service.check(&scene.id).await.unwrap();
        assert!(
            check.warnings.iter().any(|w| w.contains("plain http")),
            "{check:?}"
        );
    }

    #[tokio::test]
    async fn files_come_by_url_and_urls_must_be_allowed() {
        let (url, _) = endpoint(Some("Basic dTpw")).await; // u:p
        let allow = url.clone();
        let (f, scene) = setup_with(move |c| {
            c.remote_configurator_urls =
                vec![format!("{allow}/configure"), format!("{allow}/files/")]
        })
        .await;
        let basic = json!({"type": "basic", "username": "u", "password": "p"});
        let node = f
            .node(&scene.id, PLUGIN, remote_node(&url, "files", basic.clone()))
            .await;
        assert_eq!(node.config.launch_args["descriptor"], "api.pb");
        let (_, path) = f
            .service
            .upload_content(&scene.id, &node.id, "api.pb")
            .await
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"remote-bin");
        // Not allowed: the endpoint (and, here, its files).
        let (other, _) =
            setup_with(|c| c.remote_configurator_urls = vec!["https://elsewhere.example".into()])
                .await;
        let e = other
            .service
            .create_node(
                &other.scene().await.id,
                node_input(PLUGIN, remote_node(&url, "a", basic)),
            )
            .await
            .unwrap_err();
        assert!(
            format!("{e:?}").contains("remote_configurator_urls"),
            "{e:?}"
        );
    }

    #[tokio::test]
    async fn without_an_endpoint_a_remote_node_sets_the_fields_itself() {
        let (f, scene) = setup().await;
        let out = json!({"fields": [{"key": "manual", "type": "STRING"}]});
        let node = f
            .node(&scene.id, PLUGIN, json!({"launch": "remote", "token": "0123456789abcdef", "launch_args": {"out_schema": out}}))
            .await;
        assert_eq!(node.config.launch_args["out_schema"], out);
    }

    #[tokio::test]
    async fn an_upgrade_runs_the_endpoint_again() {
        let (url, seen) = endpoint(None).await;
        let allow = url.clone();
        let (f, scene) = setup_with(move |c| c.remote_configurator_urls = vec![allow]).await;
        let node = f
            .node(&scene.id, PLUGIN, remote_node(&url, "a", Value::Null))
            .await;
        assert_eq!(seen.calls.load(Ordering::SeqCst), 1);
        f.plugins
            .service
            .upgrade(PLUGIN, &package(&f, "2.0.0", &script("2.0.0")))
            .await
            .unwrap();
        assert_eq!(seen.calls.load(Ordering::SeqCst), 2);
        let (_, request) = seen.last.lock().unwrap().clone().unwrap();
        assert_eq!(request["plugin"]["version"], "2.0.0");
        let upgraded = f.service.get_node(&scene.id, &node.id).await.unwrap();
        assert_eq!(upgraded.plugin_version.as_deref(), Some("2.0.0"));
    }

    /// mTLS: the endpoint (the kit's server core over TLS) requires a client
    /// certificate. The node's CA, certificate and key are uploads, so a new
    /// node waits for them; the endpoint runs once the last one arrives.
    #[tokio::test]
    async fn mtls_with_uploaded_certificates() {
        use flwrs_plugin_kit::server::{Auth, Server, ServerSettings, tls};
        use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
        let dir = tempfile::tempdir().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
        let leaf = |names: &[&str]| {
            let key = KeyPair::generate().unwrap();
            let cert =
                CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
                    .unwrap()
                    .signed_by(&key, &ca)
                    .unwrap();
            (cert.pem(), key.serialize_pem())
        };
        let (server_cert, server_key) = leaf(&["localhost"]);
        let (client_cert, client_key) = leaf(&["flwrs-server"]);
        for (name, text) in [
            ("ca.pem", ca.pem()),
            ("server.pem", server_cert),
            ("server.key", server_key),
        ] {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        let config = tls::server_config(&tls::TlsFiles {
            cert: &dir.path().join("server.pem"),
            key: &dir.path().join("server.key"),
            client_ca: Some(&dir.path().join("ca.pem")),
            client_auth: tls::ClientAuth::Required,
        })
        .unwrap();
        let seen = Arc::new(Seen::default());
        let settings = ServerSettings {
            tls: Some(config),
            auth: Arc::new(Auth::Mtls {
                fingerprints: vec![],
                names: vec!["flwrs-server".into()],
            }),
            ..ServerSettings::local(0)
        };
        let server = Server::start(
            settings,
            router(None, String::new(), seen.clone()),
            Arc::new(|| true),
        )
        .await
        .unwrap();
        let url = format!("https://localhost:{}", server.local_addr().port());
        let allow = url.clone();
        let (f, scene) = setup_with(move |c| c.remote_configurator_urls = vec![allow]).await;
        let mut config = remote_node(
            &url,
            "a",
            json!({"type": "mtls", "cert": "client.pem", "key": "client.key"}),
        );
        config["configurator"]["ca"] = json!("ca.pem");
        let node = f.node(&scene.id, PLUGIN, config).await;
        assert!(
            node.config.launch_args.get("out_schema").is_none(),
            "waits for its files"
        );
        let put = |name: &'static str, text: String| {
            let (f, scene, node) = (&f, &scene, &node);
            async move {
                let incoming = f.service.incoming_path().await.unwrap();
                std::fs::write(&incoming, text).unwrap();
                f.service
                    .put_upload(&scene.id, &node.id, name, &incoming)
                    .await
                    .unwrap();
            }
        };
        put("ca.pem", ca.pem()).await;
        put("client.pem", client_cert).await;
        assert_eq!(seen.calls.load(Ordering::SeqCst), 0);
        put("client.key", client_key).await;
        assert_eq!(seen.calls.load(Ordering::SeqCst), 1);
        let node = f.service.get_node(&scene.id, &node.id).await.unwrap();
        assert_eq!(
            node.config.launch_args["out_schema"]["fields"][0]["key"],
            "a"
        );
        // The key can't be downloaded, nor deleted while the endpoint names it.
        assert!(matches!(
            f.service
                .upload_content(&scene.id, &node.id, "client.key")
                .await,
            Err(ServiceError::Forbidden(_))
        ));
        assert!(matches!(
            f.service
                .delete_upload(&scene.id, &node.id, "client.key")
                .await,
            Err(ServiceError::Conflict(..))
        ));
        server.shutdown(std::time::Duration::from_secs(1)).await;
    }
}

#[tokio::test]
async fn existing_configurators_are_never_run_in_preflight_mode() {
    // This package declares no pre-flight: its script would treat the mode's
    // arguments as a normal run, so it must not be started at all.
    let (f, scene) = setup().await;
    let node = f
        .node(&scene.id, PLUGIN, json!({"launch_args": {"spec": "a"}}))
        .await;
    let run = f.service.preflight(&scene.id, None, false).await.unwrap();
    assert!(run.ok);
    assert!(!run.nodes[0].checked);
    assert_eq!(
        run.nodes[0].skipped.as_deref(),
        Some("its plugin has no pre-flight check")
    );
    // Enabling doesn't run it either: the stored settings stay.
    f.service.set_enabled(&scene.id, true).await.unwrap();
    let after = f.service.get_node(&scene.id, &node.id).await.unwrap();
    assert_eq!(marker(&after), marker(&node));
}
