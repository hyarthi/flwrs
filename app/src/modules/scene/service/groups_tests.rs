//! Scene groups and device claims, on the service.

use super::apply::BatchOp;
use super::draft::{Change, Keep, Target};
use super::tests::{Fixture, fixture, node_input};
use super::*;
use crate::modules::scene::groups::ThreadsSource;
use crate::modules::scene::scene_file::{self, pb};
use serde_json::json;

fn group(v: Value) -> Group {
    serde_json::from_value(v).unwrap()
}

fn budget(id: &str, cpu: u32) -> Group {
    group(json!({"id": id, "family": "resources", "policy": {"limits": {"cpu": cpu}}}))
}

fn placement(id: &str) -> Group {
    group(json!({"id": id, "family": "placement", "policy": {"strategy": "pack"}}))
}

fn input(name: &str, groups: Option<Vec<Group>>) -> SceneInput {
    SceneInput {
        name: name.into(),
        groups,
        ..Default::default()
    }
}

fn details(e: ServiceError) -> String {
    match e {
        ServiceError::Invalid(d) | ServiceError::Conflict(_, d) => d.join(" | "),
        other => panic!("{other:?}"),
    }
}

/// `test-threaded` (takes a core count) and `test-camera` (claims a camera).
async fn register(f: &Fixture) {
    let Database::SQLite(db) = f.plugins.service.db();
    let mut conn = db.get_connection().await.unwrap();
    for (id, extra, schema) in [
        (
            "test-threaded",
            r#","threads":{"via":"arg","name":"threads"}"#,
            r#"{"type":"object","additionalProperties":false,"properties":{}}"#,
        ),
        (
            "test-camera",
            "",
            r#"{"type":"object","additionalProperties":false,"properties":{
                "input":{"type":"object","x-flwrs-device":{"kind":"camera","field":"device","when":{"type":"camera"},"default":"default"}}}}"#,
        ),
    ] {
        let manifest = format!(
            r#"{{"manifest_version":2,"id":"{id}","name":"T","version":"1.0.0","type":"SINK","executable":"run","launch_args_schema":"l.json","payload_schemas":{{"in":"i.json"}}{extra}}}"#
        );
        let install = f.plugins.dir.path().join(format!("installed-{id}"));
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(install.join("run"), "").unwrap();
        sqlx::query("INSERT INTO plugins (id, name, version, \"type\", manifest, launch_args_schema, in_schema, install_path, archive_sha256) VALUES ($1, 'T', '1.0.0', 'SINK', $2, $3, $4, $5, 'x')")
            .bind(id)
            .bind(manifest)
            .bind(schema)
            .bind(r#"{"fields":[{"key":"text","type":"STRING"}]}"#)
            .bind(install.to_string_lossy().into_owned())
            .execute(&mut *conn)
            .await
            .unwrap();
    }
}

async fn scene_with(f: &Fixture, groups: Vec<Group>) -> Scene {
    f.service
        .create_scene(&input("groups", Some(groups)))
        .await
        .unwrap()
}

async fn set_enabled_in_db(f: &Fixture, scene: &str) {
    let Database::SQLite(db) = f.plugins.service.db();
    let mut conn = db.get_connection().await.unwrap();
    sqlx::query("UPDATE scenes SET enabled = TRUE WHERE id = $1")
        .bind(scene)
        .execute(&mut *conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn settings_keep_refuse_and_guard_groups() {
    let f = fixture().await;
    register(&f).await;
    let s = scene_with(&f, vec![budget("vision", 4)]).await;
    assert_eq!(s.groups, [budget("vision", 4)]);

    // Left out: kept.
    let kept = f
        .service
        .update_scene(&s.id, &input("renamed", None))
        .await
        .unwrap();
    assert_eq!(kept.groups, s.groups);

    let refused = f
        .service
        .update_scene(&s.id, &input("x", Some(vec![placement("p")])))
        .await
        .unwrap_err();
    assert!(details(refused).contains("one server"));

    // A group with members can't go; a membership must name a group.
    f.node(&s.id, "test-threaded", json!({"groups": ["vision"]}))
        .await;
    let refused = f
        .service
        .update_scene(&s.id, &input("x", Some(vec![])))
        .await
        .unwrap_err();
    assert!(details(refused).contains("no group"));
    let refused = f
        .service
        .create_node(
            &s.id,
            node_input("test-threaded", json!({"groups": ["nope"]})),
        )
        .await
        .unwrap_err();
    assert!(details(refused).contains("no group \"nope\""));
    // A config is strict about the key's type.
    let refused = f
        .service
        .create_node(
            &s.id,
            node_input("test-threaded", json!({"groups": "vision"})),
        )
        .await;
    assert!(refused.is_err());
}

#[tokio::test]
async fn the_check_reports_each_nodes_threads() {
    let f = fixture().await;
    register(&f).await;
    let s = scene_with(&f, vec![budget("b", 8)]).await;
    let own = f
        .node(
            &s.id,
            "test-threaded",
            json!({"groups": ["b"], "runtime": {"threads": 2}}),
        )
        .await;
    let a = f
        .node(&s.id, "test-threaded", json!({"groups": ["b"]}))
        .await;
    let c = f
        .node(&s.id, "test-threaded", json!({"groups": ["b"]}))
        .await;
    let outside = f.node(&s.id, "test-threaded", Value::Null).await;
    // No manifest `threads`: warned about, and not counted.
    f.node(&s.id, "test-camera", json!({"groups": ["b"]})).await;

    let check = f.service.check(&s.id).await.unwrap();
    assert_eq!(check.problems, Vec::<String>::new());
    assert_eq!(check.threads[&own.id].threads, Some(2));
    assert_eq!(check.threads[&own.id].source, ThreadsSource::Node);
    for id in [&a.id, &c.id] {
        assert_eq!(check.threads[id].threads, Some(3));
        assert_eq!(check.threads[id].source, ThreadsSource::Group);
        assert_eq!(check.threads[id].group.as_deref(), Some("b"));
    }
    assert_eq!(check.threads[&outside.id].source, ThreadsSource::Plugin);
    assert_eq!(check.threads.len(), 4, "the camera node takes no count");
    assert!(
        check
            .warnings
            .iter()
            .any(|w| w.contains("core budget b doesn't apply")),
        "{:?}",
        check.warnings
    );

    // Own values beyond the budget: a problem, and a disabled scene may hold it.
    f.service
        .update_node(
            &s.id,
            &a.id,
            node_input(
                "test-threaded",
                json!({"groups": ["b"], "runtime": {"threads": 7}}),
            ),
        )
        .await
        .unwrap();
    let check = f.service.check(&s.id).await.unwrap();
    assert!(
        check.problems.iter().any(|p| p.contains("add up to 9")),
        "{:?}",
        check.problems
    );

    // An enabled scene refuses the save instead: its next load would fail.
    f.service
        .update_node(
            &s.id,
            &a.id,
            node_input("test-threaded", json!({"groups": ["b"]})),
        )
        .await
        .unwrap();
    set_enabled_in_db(&f, &s.id).await;
    let refused = f
        .service
        .update_node(
            &s.id,
            &a.id,
            node_input(
                "test-threaded",
                json!({"groups": ["b"], "runtime": {"threads": 7}}),
            ),
        )
        .await
        .unwrap_err();
    assert!(details(refused).contains("add up to 9"));
}

#[tokio::test]
async fn a_remote_member_is_warned_about() {
    if cfg!(not(feature = "remote")) {
        return;
    }
    let f = fixture().await;
    register(&f).await;
    let s = scene_with(&f, vec![budget("b", 2)]).await;
    f.node(
        &s.id,
        "test-threaded",
        json!({"launch": "remote", "token": "a-long-enough-token", "groups": ["b"], "runtime": {"threads": 9}}),
    )
    .await;
    let check = f.service.check(&s.id).await.unwrap();
    assert!(
        !check.problems.iter().any(|p| p.contains("add up")),
        "not counted: {:?}",
        check.problems
    );
    assert!(check.warnings.iter().any(|w| w.contains("remote node")));
    assert!(check.threads.is_empty());
}

#[tokio::test]
async fn groups_go_through_drafts() {
    let f = fixture().await;
    register(&f).await;
    let s = scene_with(&f, vec![budget("a", 2)]).await;
    let (draft, _) = f.service.start_draft(&s.id).await.unwrap();
    assert_eq!(draft.row.groups, s.groups);
    assert_eq!(draft.settings_change, Change::Unchanged);

    // A setting saved without groups keeps them.
    let draft = f
        .service
        .update_draft_settings(&s.id, &input("groups", None))
        .await
        .unwrap();
    assert_eq!(draft.row.groups, s.groups);
    assert_eq!(draft.settings_change, Change::Unchanged);

    let next = vec![budget("a", 2), budget("b", 6)];
    let draft = f
        .service
        .update_draft_settings(&s.id, &input("groups", Some(next.clone())))
        .await
        .unwrap();
    assert_eq!(draft.settings_change, Change::Changed);
    assert_eq!(f.service.get_scene(&s.id).await.unwrap().groups, s.groups);
    let refused = f
        .service
        .update_draft_settings(&s.id, &input("groups", Some(vec![placement("p")])))
        .await
        .unwrap_err();
    assert!(details(refused).contains("placement"));

    // Undo and redo bring them back.
    let (undone, _) = f.service.step_draft(&s.id, true).await.unwrap();
    assert_eq!(undone.row.groups, s.groups);
    let (redone, _) = f.service.step_draft(&s.id, false).await.unwrap();
    assert_eq!(redone.row.groups, next);

    // A wrong membership is stored in a draft, and its check says so.
    f.service
        .save_node(
            Store::Draft,
            &s.id,
            None,
            node_input("test-threaded", json!({"groups": ["nope"]})),
        )
        .await
        .unwrap();
    let check = f.service.check_draft(&s.id).await.unwrap();
    assert!(check.problems.iter().any(|p| p.contains("no group")));

    let draft = f.service.get_draft(&s.id).await.unwrap();
    let node = draft.nodes[0].node.id.clone();
    f.service
        .save_node(
            Store::Draft,
            &s.id,
            Some(&node),
            node_input("test-threaded", json!({"groups": ["b"]})),
        )
        .await
        .unwrap();
    let check = f.service.check_draft(&s.id).await.unwrap();
    assert_eq!(check.problems, Vec::<String>::new());
    assert_eq!(check.threads[&node].threads, Some(6));
    let scene = f.service.submit_draft(&s.id, None, false).await.unwrap();
    assert_eq!(scene.groups, next);
    assert_eq!(
        f.service
            .get_node(&s.id, &node)
            .await
            .unwrap()
            .config
            .groups,
        ["b"]
    );
}

#[tokio::test]
async fn a_rebase_takes_or_conflicts_on_groups() {
    let f = fixture().await;
    let s = scene_with(&f, vec![budget("a", 2)]).await;
    f.service.start_draft(&s.id).await.unwrap();
    f.service
        .update_scene(&s.id, &input("groups", Some(vec![budget("a", 3)])))
        .await
        .unwrap();
    let report = f.service.rebase_draft(&s.id).await.unwrap();
    assert_eq!(report.taken_live, ["settings"]);
    let draft = f.service.get_draft(&s.id).await.unwrap();
    assert_eq!(draft.row.groups, [budget("a", 3)]);

    f.service
        .update_draft_settings(&s.id, &input("groups", Some(vec![budget("a", 5)])))
        .await
        .unwrap();
    f.service
        .update_scene(&s.id, &input("groups", Some(vec![budget("a", 4)])))
        .await
        .unwrap();
    let report = f.service.rebase_draft(&s.id).await.unwrap();
    assert_eq!(report.conflicts, ["settings"]);
    f.service
        .resolve_conflict(&s.id, Target::Settings, Keep::Live)
        .await
        .unwrap();
    let draft = f.service.get_draft(&s.id).await.unwrap();
    assert_eq!(draft.row.groups, [budget("a", 4)]);
}

#[tokio::test]
async fn a_batch_sets_groups_and_members_at_once() {
    let f = fixture().await;
    register(&f).await;
    let s = f.scene().await;
    let ops = vec![
        BatchOp::UpdateScene(input("batched", Some(vec![budget("b", 4)]))),
        BatchOp::CreateNode {
            key: Some("n".into()),
            input: node_input("test-threaded", json!({"groups": ["b"]})),
        },
    ];
    let result = f.service.batch(&s.id, None, ops).await.unwrap();
    assert_eq!(result.scene.groups, [budget("b", 4)]);
    let check = f.service.check(&s.id).await.unwrap();
    assert_eq!(check.threads[&result.keys["n"]].threads, Some(4));

    let ops = vec![BatchOp::CreateNode {
        key: None,
        input: node_input("test-threaded", json!({"groups": ["gone"]})),
    }];
    let refused = f.service.batch(&s.id, None, ops).await.unwrap_err();
    assert!(details(refused).contains("no group"));
}

#[tokio::test]
async fn groups_travel_in_scene_files() {
    let f = fixture().await;
    register(&f).await;
    let s = scene_with(&f, vec![budget("b", 4)]).await;
    f.node(&s.id, "test-threaded", json!({"groups": ["b"]}))
        .await;
    let export = f.service.export_scene(&s.id, false).await.unwrap();
    let imported = f.service.import_scene(&export.bytes).await.unwrap();
    assert_eq!(imported.scene.groups, s.groups);
    let nodes = f.service.list_nodes(&imported.scene.id).await.unwrap();
    assert_eq!(nodes[0].config.groups, ["b"]);
    assert_eq!(imported.check.problems, Vec::<String>::new());

    // A file from before groups.
    let mut old = scene_file::decode(&export.bytes).unwrap();
    old.groups_json = None;
    for n in &mut old.nodes {
        n.config_json = "{}".into();
    }
    let imported = f
        .service
        .import_scene(&scene_file::encode(&old))
        .await
        .unwrap();
    assert!(imported.scene.groups.is_empty());

    // A policy this server can't honour imports, and can't be enabled.
    let mut future: pb::SceneFile = scene_file::decode(&export.bytes).unwrap();
    future.groups_json = Some(serde_json::to_string(&[placement("p"), budget("b", 4)]).unwrap());
    let imported = f
        .service
        .import_scene(&scene_file::encode(&future))
        .await
        .unwrap();
    assert!(
        imported
            .check
            .problems
            .iter()
            .any(|p| p.contains("placement policies")),
        "{:?}",
        imported.check.problems
    );

    // A group this version can't read refuses the file.
    let mut broken = scene_file::decode(&export.bytes).unwrap();
    broken.groups_json = Some(r#"[{"id":"x","family":"quantum","policy":{}}]"#.into());
    let refused = f
        .service
        .import_scene(&scene_file::encode(&broken))
        .await
        .err()
        .expect("refused");
    assert!(details(refused).contains("groups:"));
}

#[tokio::test]
async fn two_nodes_claiming_one_device_are_a_problem() {
    let f = fixture().await;
    register(&f).await;
    let camera = |device: Option<&str>| {
        let mut input = json!({"type": "camera"});
        if let Some(d) = device {
            input["device"] = json!(d);
        }
        json!({"launch_args": {"input": input}})
    };
    let clashes = |c: &SceneCheck| {
        c.problems
            .iter()
            .filter(|p| p.contains("claim"))
            .cloned()
            .collect::<Vec<_>>()
    };
    let s = f.scene().await;
    let a = f.node(&s.id, "test-camera", camera(None)).await;
    let check = f.service.check(&s.id).await.unwrap();
    assert_eq!(check.devices[&a.id][0].device, "default");
    assert!(clashes(&check).is_empty());

    // Another name for a device isn't recognised as the same one.
    f.node(&s.id, "test-camera", camera(Some("/dev/video0")))
        .await;
    assert!(clashes(&f.service.check(&s.id).await.unwrap()).is_empty());
    // A file input claims nothing.
    f.node(
        &s.id,
        "test-camera",
        json!({"launch_args": {"input": {"type": "images", "device": "default"}}}),
    )
    .await;
    assert!(clashes(&f.service.check(&s.id).await.unwrap()).is_empty());

    let b = f.node(&s.id, "test-camera", camera(None)).await;
    let found = clashes(&f.service.check(&s.id).await.unwrap());
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains(&a.id) && found[0].contains(&b.id));
    f.service.delete_node(&s.id, &b.id).await.unwrap();

    // Against another scene, once that's enabled.
    let other = f
        .service
        .create_scene(&SceneInput::named("other"))
        .await
        .unwrap();
    f.node(&other.id, "test-camera", camera(Some("/dev/video0")))
        .await;
    assert!(clashes(&f.service.check(&s.id).await.unwrap()).is_empty());
    set_enabled_in_db(&f, &other.id).await;
    let found = clashes(&f.service.check(&s.id).await.unwrap());
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains(&other.id) && found[0].contains("/dev/video0"));
}

#[tokio::test]
async fn the_groups_migration_goes_down_and_up_again() {
    let f = fixture().await;
    register(&f).await;
    let s = scene_with(&f, vec![budget("b", 4)]).await;
    let n = f
        .node(&s.id, "test-threaded", json!({"groups": ["b"]}))
        .await;
    f.service.start_draft(&s.id).await.unwrap();
    let Database::SQLite(db) = f.plugins.service.db();
    let mut conn = db.get_connection().await.unwrap();
    let run = |sql: &'static str| sqlx::raw_sql(sqlx::AssertSqlSafe(sql));
    run(include_str!(
        "../../../../resources/migrations/sqlite/main/1.0.0-14.down.sql"
    ))
    .execute(&mut *conn)
    .await
    .unwrap();
    for (table, id) in [("scene_nodes", &n.id), ("scene_draft_nodes", &n.id)] {
        let config: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT config FROM {table} WHERE id = $1"
        )))
        .bind(id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert!(!config.contains("groups"), "{table}: {config}");
    }
    let history: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM scene_draft_history")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(history, 0, "states recorded with groups went");
    run(include_str!(
        "../../../../resources/migrations/sqlite/main/1.0.0-14.up.sql"
    ))
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let back = f.service.get_scene(&s.id).await.unwrap();
    assert!(back.groups.is_empty());
    assert!(
        f.service
            .get_draft(&s.id)
            .await
            .unwrap()
            .row
            .groups
            .is_empty()
    );
}

#[test]
fn a_new_membership_alone_needs_no_pre_flight() {
    assert!(!draft::submit::may_restart("name"));
    assert!(!draft::submit::may_restart("config.groups"));
    assert!(draft::submit::may_restart("config.runtime"));
    assert!(draft::submit::may_restart("plugin"));
}
