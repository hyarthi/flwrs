//! Drafts and the bulk endpoint, on the service.

use super::apply::BatchOp;
use super::draft::{Change, Conflict, Keep, Target};
use super::tests::{edge_input, fixture, node_input};
use super::*;
use serde_json::json;

fn conflict(e: ServiceError) -> (String, Vec<String>) {
    match e {
        ServiceError::Conflict(message, details) => (message, details),
        other => panic!("expected Conflict, got {other:?}"),
    }
}

fn node_change(draft: &draft::Draft, id: &str) -> (Change, Option<Conflict>) {
    let n = draft
        .nodes
        .iter()
        .find(|n| n.node.id == id)
        .expect("node in draft");
    (n.change, n.conflict)
}

/// Enables the scene in the DB (the fixture's source and transform plugins
/// have no files, so `set_enabled` would refuse).
async fn enable(f: &super::tests::Fixture, scene_id: &str) {
    let Database::SQLite(db) = f.plugins.service.db();
    sqlx::query("UPDATE scenes SET enabled = TRUE WHERE id = $1")
        .bind(scene_id)
        .execute(&mut *db.get_connection().await.unwrap())
        .await
        .unwrap();
}

fn sink(timeout: i64) -> Value {
    json!({"launch_args": {"timeout": timeout}})
}

#[tokio::test]
async fn a_draft_starts_from_live_and_leaves_live_alone() {
    let f = fixture().await;
    let s = f.scene().await;
    let a = f.node(&s.id, "test-source", Value::Null).await;
    let b = f.node(&s.id, "test-sink", sink(5)).await;
    f.edge(&s.id, &a.id, &b.id).await.unwrap();
    let revision = f.service.get_scene(&s.id).await.unwrap().revision;
    assert_eq!(revision, 3, "every live change counts");

    let none = f.service.get_draft(&s.id).await.unwrap_err();
    assert!(matches!(none, ServiceError::NotFound("draft")));
    let (draft, created) = f.service.start_draft(&s.id).await.unwrap();
    assert!(created);
    assert_eq!(draft.row.base_revision, revision);
    assert!(!draft.has_changes() && !draft.behind());
    assert_eq!(draft.nodes.len(), 2);
    assert!(draft.nodes.iter().all(|n| n.change == Change::Unchanged));
    assert_eq!(draft.edges.len(), 1);
    let (_, again) = f.service.start_draft(&s.id).await.unwrap();
    assert!(!again);

    // Edits go to the draft only, with the same ids.
    f.service
        .save_node(
            Store::Draft,
            &s.id,
            Some(&b.id),
            node_input("test-sink", sink(9)),
        )
        .await
        .unwrap();
    let c = f
        .service
        .save_node(Store::Draft, &s.id, None, node_input("test-sink", sink(1)))
        .await
        .unwrap();
    f.service
        .write_edge(
            Store::Draft,
            &s.id,
            None,
            edge_input(&a.id, &c.id, Value::Null),
        )
        .await
        .unwrap();
    assert_eq!(
        f.service
            .get_node(&s.id, &b.id)
            .await
            .unwrap()
            .config
            .launch_args,
        json!({"timeout": 5})
    );
    assert!(f.service.get_node(&s.id, &c.id).await.is_err());
    assert_eq!(f.service.get_scene(&s.id).await.unwrap().revision, revision);

    let draft = f.service.get_draft(&s.id).await.unwrap();
    assert_eq!(node_change(&draft, &a.id).0, Change::Unchanged);
    assert_eq!(node_change(&draft, &b.id).0, Change::Changed);
    assert_eq!(node_change(&draft, &c.id).0, Change::Added);
    let b_row = draft.nodes.iter().find(|n| n.node.id == b.id).unwrap();
    assert_eq!(b_row.parts, vec!["config.launch_args"]);
    assert!(draft.has_changes());

    // Settings too.
    let renamed = SceneInput {
        name: "renamed".into(),
        max_hops: Some(7),
        ..Default::default()
    };
    let draft = f
        .service
        .update_draft_settings(&s.id, &renamed)
        .await
        .unwrap();
    assert_eq!(draft.settings_change, Change::Changed);
    assert_eq!(f.service.get_scene(&s.id).await.unwrap().name, "scene");

    f.service.discard_draft(&s.id).await.unwrap();
    assert!(f.service.get_draft(&s.id).await.is_err());
    assert!(
        f.commands().await.is_empty(),
        "a disabled scene: no reloads"
    );
}

#[tokio::test]
async fn drafts_take_incomplete_nodes_live_refuses_them() {
    let f = fixture().await;
    let s = f.scene().await;
    // timeout must be >= 1: invalid live, incomplete in a draft.
    let live = f
        .service
        .create_node(&s.id, node_input("test-sink", sink(0)))
        .await;
    assert!(matches!(live, Err(ServiceError::Invalid(_))));
    let node = f
        .service
        .save_node(Store::Draft, &s.id, None, node_input("test-sink", sink(0)))
        .await
        .unwrap();
    let check = f.service.check_draft(&s.id).await.unwrap();
    assert!(
        check
            .problems
            .iter()
            .any(|p| p.contains(&node.id) && p.contains("timeout")),
        "{check:?}"
    );
    // Structure is still refused.
    let unknown = f
        .service
        .save_node(
            Store::Draft,
            &s.id,
            None,
            node_input("no-such-plugin", Value::Null),
        )
        .await;
    assert!(matches!(unknown, Err(ServiceError::Invalid(_))));
    let wrong = f
        .service
        .save_node(
            Store::Draft,
            &s.id,
            None,
            node_input("test-sink", json!({"launch": "sideways"})),
        )
        .await;
    assert!(matches!(wrong, Err(ServiceError::Invalid(_))));
    // Graph rules too: a SINK sends nothing.
    let src = f
        .service
        .save_node(
            Store::Draft,
            &s.id,
            None,
            node_input("test-source", Value::Null),
        )
        .await
        .unwrap();
    let backwards = f
        .service
        .write_edge(
            Store::Draft,
            &s.id,
            None,
            edge_input(&node.id, &src.id, Value::Null),
        )
        .await;
    assert!(matches!(backwards, Err(ServiceError::Invalid(_))));
}

#[tokio::test]
async fn removing_in_a_draft_leaves_shadows_that_can_be_restored() {
    let f = fixture().await;
    let s = f.scene().await;
    let a = f.node(&s.id, "test-source", Value::Null).await;
    let b = f.node(&s.id, "test-sink", sink(5)).await;
    let e = f.edge(&s.id, &a.id, &b.id).await.unwrap();

    f.service.remove_draft_node(&s.id, &b.id).await.unwrap();
    let draft = f.service.get_draft(&s.id).await.unwrap();
    assert_eq!(node_change(&draft, &b.id).0, Change::Removed);
    assert_eq!(
        draft.edges[0].change,
        Change::Removed,
        "its edge goes with it"
    );
    assert!(
        f.service
            .get_node_in(Store::Draft, &s.id, &b.id)
            .await
            .is_err()
    );
    assert!(
        f.service.remove_draft_node(&s.id, &b.id).await.is_err(),
        "already removed"
    );

    // The edge can't come back before its node.
    let (message, _) = conflict(
        f.service
            .restore_draft_edge(&s.id, &e.id)
            .await
            .unwrap_err(),
    );
    assert!(message.contains("nodes"), "{message}");
    f.service.restore_draft_node(&s.id, &b.id).await.unwrap();
    f.service.restore_draft_edge(&s.id, &e.id).await.unwrap();
    let draft = f.service.get_draft(&s.id).await.unwrap();
    assert!(!draft.has_changes(), "back to the base");

    // A node added in the draft just goes.
    let c = f
        .service
        .save_node(Store::Draft, &s.id, None, node_input("test-sink", sink(2)))
        .await
        .unwrap();
    f.service.remove_draft_node(&s.id, &c.id).await.unwrap();
    let draft = f.service.get_draft(&s.id).await.unwrap();
    assert!(draft.nodes.iter().all(|n| n.node.id != c.id));
}

#[tokio::test]
async fn submit_applies_the_draft_at_once() {
    let f = fixture().await;
    let s = f.scene().await;
    let a = f.node(&s.id, "test-source", Value::Null).await;
    let b = f.node(&s.id, "test-sink", sink(5)).await;
    let old = f.edge(&s.id, &a.id, &b.id).await.unwrap();
    let revision = f.service.get_scene(&s.id).await.unwrap().revision;

    // In the draft: b changes, c and t are added, the edge is rerouted.
    f.service
        .save_node(
            Store::Draft,
            &s.id,
            Some(&b.id),
            node_input("test-sink", sink(9)),
        )
        .await
        .unwrap();
    let t = f
        .service
        .save_node(
            Store::Draft,
            &s.id,
            None,
            node_input("test-transform", Value::Null),
        )
        .await
        .unwrap();
    f.service.remove_draft_edge(&s.id, &old.id).await.unwrap();
    for (from, to) in [(&a.id, &t.id), (&t.id, &b.id)] {
        f.service
            .write_edge(Store::Draft, &s.id, None, edge_input(from, to, Value::Null))
            .await
            .unwrap();
    }
    let renamed = SceneInput {
        name: "renamed".into(),
        ..Default::default()
    };
    f.service
        .update_draft_settings(&s.id, &renamed)
        .await
        .unwrap();

    let scene = f
        .service
        .submit_draft(&s.id, Some(revision), false)
        .await
        .unwrap();
    assert_eq!(scene.revision, revision + 1, "one revision");
    assert_eq!(scene.name, "renamed");
    assert!(!scene.enabled);
    assert!(f.commands().await.is_empty(), "disabled: nothing to reload");
    assert!(
        f.service.get_draft(&s.id).await.is_err(),
        "the draft is gone"
    );

    let nodes = f.service.list_nodes(&s.id).await.unwrap();
    assert_eq!(nodes.len(), 3);
    assert!(
        nodes.iter().any(|n| n.id == t.id),
        "added nodes keep their draft ids"
    );
    assert_eq!(
        f.service
            .get_node(&s.id, &b.id)
            .await
            .unwrap()
            .config
            .launch_args,
        json!({"timeout": 9})
    );
    let edges = f.service.list_edges(&s.id).await.unwrap();
    assert_eq!(edges.len(), 2);
    assert!(edges.iter().all(|e| e.id != old.id));
    // An unchanged node keeps its row.
    assert_eq!(
        f.service.get_node(&s.id, &a.id).await.unwrap().update_time,
        a.update_time
    );
}

#[tokio::test]
async fn an_enabled_scene_takes_a_draft_only_without_problems() {
    let f = fixture().await;
    let s = f.scene().await;
    f.node(&s.id, "test-sink", sink(5)).await;
    f.service.set_enabled(&s.id, true).await.unwrap();
    f.commands().await;
    let broken = f
        .service
        .save_node(Store::Draft, &s.id, None, node_input("test-sink", sink(0)))
        .await
        .unwrap();

    let (message, problems) = conflict(
        f.service
            .submit_draft(&s.id, None, false)
            .await
            .unwrap_err(),
    );
    assert!(message.contains("problems"), "{message}");
    assert!(
        problems.iter().any(|p| p.contains(&broken.id)),
        "{problems:?}"
    );

    // Submit and disable: it goes in, the scene stops.
    let scene = f.service.submit_draft(&s.id, None, true).await.unwrap();
    assert!(!scene.enabled);
    assert_eq!(f.commands().await, vec![format!("reload {}", s.id)]);
    let check = f.service.check(&s.id).await.unwrap();
    assert!(
        !check.problems.is_empty(),
        "the incomplete node is live now"
    );
}

#[tokio::test]
async fn a_rebase_merges_what_changed_on_one_side_and_flags_the_rest() {
    let f = fixture().await;
    let s = f.scene().await;
    let a = f.node(&s.id, "test-source", Value::Null).await;
    let b = f.node(&s.id, "test-sink", sink(5)).await;
    let c = f.node(&s.id, "test-sink", sink(6)).await;
    let d = f.node(&s.id, "test-sink", sink(7)).await;
    f.edge(&s.id, &a.id, &d.id).await.unwrap();
    f.service.start_draft(&s.id).await.unwrap();

    // Draft: b and c change, d is removed.
    f.service
        .save_node(
            Store::Draft,
            &s.id,
            Some(&b.id),
            node_input("test-sink", sink(50)),
        )
        .await
        .unwrap();
    f.service
        .save_node(
            Store::Draft,
            &s.id,
            Some(&c.id),
            node_input("test-sink", sink(60)),
        )
        .await
        .unwrap();
    f.service.remove_draft_node(&s.id, &d.id).await.unwrap();
    // Live: a node is added, b changes too, c changes the same way, d changes.
    let e = f.node(&s.id, "test-sink", sink(8)).await;
    f.service
        .update_node(&s.id, &b.id, node_input("test-sink", sink(51)))
        .await
        .unwrap();
    f.service
        .update_node(&s.id, &c.id, node_input("test-sink", sink(60)))
        .await
        .unwrap();
    f.service
        .update_node(&s.id, &d.id, node_input("test-sink", sink(70)))
        .await
        .unwrap();

    let (message, _) = conflict(
        f.service
            .submit_draft(&s.id, None, false)
            .await
            .unwrap_err(),
    );
    assert!(message.contains("rebase"), "{message}");
    let report = f.service.rebase_draft(&s.id).await.unwrap();
    assert_eq!(
        report.to_revision,
        f.service.get_scene(&s.id).await.unwrap().revision
    );
    assert!(
        report.taken_live.contains(&format!("node {}", e.id)),
        "{report:?}"
    );
    assert_eq!(report.conflicts.len(), 2, "{report:?}");

    let draft = f.service.get_draft(&s.id).await.unwrap();
    assert!(!draft.behind());
    assert_eq!(node_change(&draft, &e.id), (Change::Unchanged, None));
    assert_eq!(
        node_change(&draft, &b.id),
        (Change::Changed, Some(Conflict::ChangedBoth))
    );
    assert_eq!(
        node_change(&draft, &c.id),
        (Change::Unchanged, None),
        "the same change on both sides"
    );
    assert_eq!(
        node_change(&draft, &d.id),
        (Change::Removed, Some(Conflict::ChangedLive))
    );

    let (message, details) = conflict(
        f.service
            .submit_draft(&s.id, None, false)
            .await
            .unwrap_err(),
    );
    assert!(message.contains("conflicts"), "{message}");
    assert_eq!(details.len(), 2);

    // b: keep the draft's; d: take live's (it comes back, changed).
    f.service
        .resolve_conflict(&s.id, Target::Node(b.id.clone()), Keep::Draft)
        .await
        .unwrap();
    let draft = f
        .service
        .resolve_conflict(&s.id, Target::Node(d.id.clone()), Keep::Live)
        .await
        .unwrap();
    assert_eq!(node_change(&draft, &d.id), (Change::Unchanged, None));
    assert!(draft.conflicts().is_empty());
    let again = f
        .service
        .resolve_conflict(&s.id, Target::Node(b.id.clone()), Keep::Draft)
        .await;
    assert!(
        matches!(again, Err(ServiceError::Conflict(..))),
        "no conflict left there"
    );

    f.service.submit_draft(&s.id, None, false).await.unwrap();
    assert_eq!(
        f.service
            .get_node(&s.id, &b.id)
            .await
            .unwrap()
            .config
            .launch_args,
        json!({"timeout": 50})
    );
    assert_eq!(
        f.service
            .get_node(&s.id, &d.id)
            .await
            .unwrap()
            .config
            .launch_args,
        json!({"timeout": 70})
    );
    assert_eq!(f.service.list_nodes(&s.id).await.unwrap().len(), 5);
}

#[tokio::test]
async fn a_rebase_drops_edges_of_nodes_removed_live() {
    let f = fixture().await;
    let s = f.scene().await;
    let a = f.node(&s.id, "test-source", Value::Null).await;
    let b = f.node(&s.id, "test-sink", sink(5)).await;
    f.service.start_draft(&s.id).await.unwrap();
    let e = f
        .service
        .write_edge(
            Store::Draft,
            &s.id,
            None,
            edge_input(&a.id, &b.id, Value::Null),
        )
        .await
        .unwrap();
    // Live removes b, which the draft didn't change.
    f.service.delete_node(&s.id, &b.id).await.unwrap();
    let report = f.service.rebase_draft(&s.id).await.unwrap();
    assert!(
        report.taken_live.contains(&format!("node {}", b.id)),
        "{report:?}"
    );
    assert!(
        report.dropped_edges.contains(&format!("edge {}", e.id)),
        "{report:?}"
    );
    let draft = f.service.get_draft(&s.id).await.unwrap();
    assert_eq!(draft.nodes.len(), 1);
    assert!(draft.edges.is_empty());
}

#[tokio::test]
async fn settings_conflicts_resolve_too() {
    let f = fixture().await;
    let s = f.scene().await;
    f.service.start_draft(&s.id).await.unwrap();
    f.service
        .update_draft_settings(&s.id, &SceneInput::named("mine"))
        .await
        .unwrap();
    f.service
        .update_scene(&s.id, &SceneInput::named("theirs"))
        .await
        .unwrap();
    let report = f.service.rebase_draft(&s.id).await.unwrap();
    assert_eq!(report.conflicts, vec!["settings"]);
    let draft = f
        .service
        .resolve_conflict(&s.id, Target::Settings, Keep::Live)
        .await
        .unwrap();
    assert_eq!(draft.row.name, "theirs");
    assert_eq!(draft.settings_change, Change::Unchanged);
}

#[tokio::test]
async fn draft_uploads_reach_live_on_submit() {
    let f = fixture().await;
    let s = f.scene().await;
    let args = json!({"schemas": {"in": {"fields": [{"key": "msg", "type": "STRING"}]}},
                      "script": "return payload", "model": "model.onnx"});
    let node = f
        .service
        .save_node(
            Store::Draft,
            &s.id,
            None,
            node_input("test-script", json!({"launch_args": args})),
        )
        .await
        .unwrap();
    let incoming = f.service.incoming_path().await.unwrap();
    std::fs::write(&incoming, b"weights").unwrap();
    let (upload, created) = f
        .service
        .put_upload_in(Store::Draft, &s.id, &node.id, "model.onnx", &incoming)
        .await
        .unwrap();
    assert!(created);
    let stored = f.service.files().files_dir.join(upload.stored_name());
    assert!(stored.exists());
    assert!(
        f.service
            .get_upload(&s.id, &node.id, "model.onnx")
            .await
            .is_err(),
        "not live yet"
    );
    let check = f.service.check_draft(&s.id).await.unwrap();
    assert!(
        !check.problems.iter().any(|p| p.contains("model.onnx")),
        "{check:?}"
    );

    f.service.submit_draft(&s.id, None, false).await.unwrap();
    let live = f
        .service
        .get_upload(&s.id, &node.id, "model.onnx")
        .await
        .unwrap();
    assert_eq!(live.sha256, upload.sha256);
    assert!(stored.exists(), "kept: live names it now");

    // A discarded draft's file goes, unless live still names it.
    f.service.start_draft(&s.id).await.unwrap();
    f.service.discard_draft(&s.id).await.unwrap();
    assert!(stored.exists());
}

#[tokio::test]
async fn a_batch_applies_everything_at_once_or_nothing() {
    let f = fixture().await;
    let s = f.scene().await;
    let a = f.node(&s.id, "test-source", Value::Null).await;
    enable(&f, &s.id).await;
    let revision = f.service.get_scene(&s.id).await.unwrap().revision;

    let ops = vec![
        BatchOp::CreateNode {
            key: Some("t".into()),
            input: node_input("test-transform", Value::Null),
        },
        BatchOp::CreateNode {
            key: Some("k".into()),
            input: node_input("test-sink", sink(3)),
        },
        BatchOp::CreateEdge {
            key: None,
            input: edge_input(&a.id, "$t", Value::Null),
        },
        BatchOp::CreateEdge {
            key: Some("e".into()),
            input: edge_input("$t", "$k", Value::Null),
        },
        BatchOp::UpdateNode {
            id: "$k".into(),
            input: node_input("test-sink", sink(4)),
        },
        BatchOp::UpdateScene(SceneInput::named("batched")),
    ];
    let result = f.service.batch(&s.id, Some(revision), ops).await.unwrap();
    assert_eq!(result.scene.revision, revision + 1);
    assert_eq!(result.scene.name, "batched");
    assert_eq!(result.ids.len(), 6);
    assert_eq!(result.ids[5], None);
    assert_eq!(f.commands().await, vec![format!("reload {}", s.id)]);
    let k = &result.keys["k"];
    assert_eq!(
        f.service
            .get_node(&s.id, k)
            .await
            .unwrap()
            .config
            .launch_args,
        json!({"timeout": 4})
    );
    assert_eq!(f.service.list_edges(&s.id).await.unwrap().len(), 2);

    // A stale revision.
    let stale = f.service.batch(&s.id, Some(revision), vec![]).await;
    assert!(matches!(stale, Err(ServiceError::Conflict(..))));

    // One bad operation: nothing is written.
    let ops = vec![
        BatchOp::DeleteEdge {
            id: result.keys["e"].clone(),
        },
        BatchOp::CreateNode {
            key: None,
            input: node_input("test-sink", sink(0)),
        },
    ];
    let errors = match f.service.batch(&s.id, None, ops).await.unwrap_err() {
        ServiceError::Invalid(errors) => errors,
        other => panic!("{other:?}"),
    };
    assert!(
        errors[0].starts_with("ops[1] (create_node): "),
        "{errors:?}"
    );
    assert_eq!(f.service.list_edges(&s.id).await.unwrap().len(), 2);
    assert_eq!(
        f.service.get_scene(&s.id).await.unwrap().revision,
        revision + 1
    );
    assert!(f.commands().await.is_empty());

    // Graph rules hold across the batch: a SINK can't send.
    let ops = vec![BatchOp::CreateEdge {
        key: None,
        input: edge_input(k, &a.id, Value::Null),
    }];
    assert!(matches!(
        f.service.batch(&s.id, None, ops).await,
        Err(ServiceError::Invalid(_))
    ));
    // Unknown keys.
    let ops = vec![BatchOp::DeleteNode { id: "$nope".into() }];
    assert!(matches!(
        f.service.batch(&s.id, None, ops).await,
        Err(ServiceError::Invalid(_))
    ));
}

#[tokio::test]
async fn layout_is_kept_apart_from_content() {
    let f = fixture().await;
    let s = f.scene().await;
    let at = |x: f64, y: f64| Some(Position { x, y });
    let mut input = node_input("test-sink", sink(5));
    input.position = at(10.0, 20.0);
    let a = f.service.create_node(&s.id, input).await.unwrap();
    assert_eq!(a.position, at(10.0, 20.0));
    let revision = f.service.get_scene(&s.id).await.unwrap().revision;

    // Live: no revision for a move.
    let one = |id: &str, p| BTreeMap::from([(id.to_string(), p)]);
    f.service
        .set_layout(&s.id, &one(&a.id, at(1.0, 2.0)))
        .await
        .unwrap();
    assert_eq!(
        f.service.get_node(&s.id, &a.id).await.unwrap().position,
        at(1.0, 2.0)
    );
    assert_eq!(f.service.get_scene(&s.id).await.unwrap().revision, revision);

    // A draft copies positions; a move there is no draft change, and it
    // reaches live and the draft alike.
    f.service.start_draft(&s.id).await.unwrap();
    f.service
        .set_layout(&s.id, &one(&a.id, at(3.0, 4.0)))
        .await
        .unwrap();
    let draft = f.service.get_draft(&s.id).await.unwrap();
    assert!(!draft.has_changes());
    assert_eq!(draft.nodes[0].node.position, at(3.0, 4.0));
    assert_eq!(
        f.service.get_node(&s.id, &a.id).await.unwrap().position,
        at(3.0, 4.0)
    );

    // A node only in the draft takes its position to live on submit; a
    // content change keeps the position.
    let mut added = node_input("test-sink", sink(6));
    added.position = at(50.0, 60.0);
    let b = f
        .service
        .save_node(Store::Draft, &s.id, None, added)
        .await
        .unwrap();
    f.service
        .set_layout(&s.id, &one(&b.id, at(70.0, 80.0)))
        .await
        .unwrap();
    f.service
        .save_node(
            Store::Draft,
            &s.id,
            Some(&a.id),
            node_input("test-sink", sink(7)),
        )
        .await
        .unwrap();
    f.service.submit_draft(&s.id, None, false).await.unwrap();
    assert_eq!(
        f.service.get_node(&s.id, &b.id).await.unwrap().position,
        at(70.0, 80.0)
    );
    assert_eq!(
        f.service.get_node(&s.id, &a.id).await.unwrap().position,
        at(3.0, 4.0)
    );

    // Clearing; unknown nodes and bad numbers refuse the whole request.
    f.service
        .set_layout(&s.id, &one(&b.id, None))
        .await
        .unwrap();
    assert_eq!(
        f.service.get_node(&s.id, &b.id).await.unwrap().position,
        None
    );
    let mixed = BTreeMap::from([
        (a.id.clone(), at(0.0, 0.0)),
        ("nope".to_string(), at(0.0, 0.0)),
    ]);
    let e = f.service.set_layout(&s.id, &mixed).await.unwrap_err();
    assert!(
        matches!(&e, ServiceError::Invalid(d) if d[0].contains("nope")),
        "{e:?}"
    );
    assert_eq!(
        f.service.get_node(&s.id, &a.id).await.unwrap().position,
        at(3.0, 4.0)
    );
    let e = f
        .service
        .set_layout(&s.id, &one(&a.id, at(f64::MAX, 0.0)))
        .await
        .unwrap_err();
    assert!(matches!(e, ServiceError::Invalid(_)));

    // Batches place new nodes; exports and imports keep positions.
    let mut placed = node_input("test-sink", sink(8));
    placed.position = at(-5.0, 5.5);
    let result = f
        .service
        .batch(
            &s.id,
            None,
            vec![BatchOp::CreateNode {
                key: None,
                input: placed,
            }],
        )
        .await
        .unwrap();
    let c = result.ids[0].clone().unwrap();
    assert_eq!(
        f.service.get_node(&s.id, &c).await.unwrap().position,
        at(-5.0, 5.5)
    );
    let export = f.service.export_scene(&s.id, false).await.unwrap();
    let imported = f.service.import_scene(&export.bytes).await.unwrap();
    let mut positions: Vec<_> = f
        .service
        .list_nodes(&imported.scene.id)
        .await
        .unwrap()
        .into_iter()
        .map(|n| n.position)
        .collect();
    positions.sort_by(|p, q| format!("{p:?}").cmp(&format!("{q:?}")));
    assert_eq!(positions, vec![None, at(-5.0, 5.5), at(3.0, 4.0)]);
}
