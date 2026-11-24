//! A resources group's core budget reaching real plugin processes. The
//! plugin is the test sink behind a script that first notes the core count
//! it was launched with, so every launch leaves a line.

use super::*;
use crate::modules::scene::service::SceneInput;
use flwrs_types::groups::Group;

const PLUGIN: &str = "t-threaded-sink";

fn group(v: serde_json::Value) -> Group {
    serde_json::from_value(v).unwrap()
}

fn budget(id: &str, cpu: u32) -> Group {
    group(json!({"id": id, "family": "resources", "policy": {"limits": {"cpu": cpu}}}))
}

/// Installs [`PLUGIN`]; each launch appends `$FLWRS_TEST_CORES` to
/// `<dir>/cores-<node>`.
async fn register(s: &Setup) {
    use std::os::unix::fs::PermissionsExt;
    let root = s.dir.path();
    let install = root.join("threaded");
    std::fs::create_dir_all(&install).unwrap();
    let script = install.join("run.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$FLWRS_TEST_CORES\" >> '{}/cores-'\"$(basename \"$PWD\")\"\nexec '{}' \"$@\"\n",
            root.display(),
            plugin_scripts().join("sink.sh").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let manifest = format!(
        r#"{{"manifest_version":2,"id":"{PLUGIN}","name":"T","version":"1.0.0","type":"SINK","executable":"run.sh","payload_schemas":{{"in":"i.json"}},"threads":{{"via":"env","name":"FLWRS_TEST_CORES"}}}}"#
    );
    let Database::SQLite(db) = s.f.plugins.service.db();
    let mut conn = db.get_connection().await.unwrap();
    sqlx::query(
        "INSERT INTO plugins (id, name, version, \"type\", manifest, install_path, archive_sha256, in_schema) \
         VALUES ($1, $1, '1.0.0', 'SINK', $2, $3, 'x', $4)",
    )
    .bind(PLUGIN)
    .bind(manifest)
    .bind(install.to_string_lossy().into_owned())
    .bind(TEXT_SCHEMA_JSON)
    .execute(&mut *conn)
    .await
    .unwrap();
}

/// The core counts `node` has been launched with.
fn launches(s: &Setup, node: &str) -> Vec<String> {
    std::fs::read_to_string(s.dir.path().join(format!("cores-{node}")))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

async fn wait_launches(s: &Setup, node: &str, want: &[&str]) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let got = launches(s, node);
        if got == want {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{node}: launched with {got:?}, expected {want:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn set_groups(s: &Setup, scene: &str, groups: Vec<Group>) {
    s.f.service
        .update_scene(
            scene,
            &SceneInput {
                name: "scene".into(),
                groups: Some(groups),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    s.director.reload(scene).await.unwrap();
}

/// Waits until every node of the scene runs.
async fn wait_running(s: &Setup, scene: &str, nodes: usize) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let status = s.status(scene).await;
        if status.nodes.len() == nodes
            && status
                .nodes
                .iter()
                .all(|n| n.status.state == crate::modules::supervisor::NodeState::Running)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "not running: {:?}",
            status.nodes
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_budget_reaches_plugins_and_only_a_new_share_restarts_them() {
    let mut s = setup().await;
    register(&s).await;
    s.start_director(32).await;
    let scene = s.enabled_scene().await;
    set_groups(
        &s,
        &scene,
        vec![
            budget("b", 4),
            group(json!({"id": "tag", "family": "resources", "policy": {"limits": {}}})),
        ],
    )
    .await;
    let a = s.node_with(&scene, PLUGIN, json!({"groups": ["b"]})).await;
    let c = s.node_with(&scene, PLUGIN, json!({"groups": ["b"]})).await;
    s.director.reload(&scene).await.unwrap();
    wait_running(&s, &scene, 2).await;
    wait_launches(&s, &a, &["2"]).await;
    wait_launches(&s, &c, &["2"]).await;
    let before = s.processes(&scene).await;

    // Membership alone (a group that changes nobody's share): nothing restarts.
    s.f.service
        .update_node(
            &scene,
            &a,
            node_input(PLUGIN, json!({"groups": ["b", "tag"]})),
        )
        .await
        .unwrap();
    s.director.reload(&scene).await.unwrap();
    assert_eq!(s.processes(&scene).await, before, "nothing restarted");

    // An explicit value equal to the share: the plugin gets the same count.
    s.f.service
        .update_node(
            &scene,
            &a,
            node_input(
                PLUGIN,
                json!({"groups": ["b", "tag"], "runtime": {"threads": 2}}),
            ),
        )
        .await
        .unwrap();
    s.director.reload(&scene).await.unwrap();
    assert_eq!(s.processes(&scene).await, before, "the same count");

    // A bigger budget: the member without its own value gets a new share and
    // restarts; the one with its own value doesn't.
    set_groups(
        &s,
        &scene,
        vec![
            budget("b", 6),
            group(json!({"id": "tag", "family": "resources", "policy": {"limits": {}}})),
        ],
    )
    .await;
    wait_launches(&s, &c, &["2", "4"]).await;
    wait_running(&s, &scene, 2).await;
    assert_eq!(launches(&s, &a), ["2"]);
    assert_eq!(s.processes(&scene).await[&a], before[&a]);
    s.stop_director().await;
}
