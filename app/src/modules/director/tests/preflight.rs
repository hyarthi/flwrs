//! Pre-flight checks in the director and at enable: a real sink plugin whose
//! package has a shell-script configurator. What the check answers comes from
//! a file the test rewrites: `ok`, `busy` (retryable) or `broken` (permanent).

use super::*;
use crate::modules::configurator;
use crate::modules::node_config::{PreflightRetryOverride, SceneDefaults};
use crate::modules::plugin::package::tests::{Item, PLATFORM, build_zip};
use crate::modules::scene::service::draft::submit::{DraftSubmit, DraftSubmitState, Submitted};
use crate::modules::scene::service::{Scene, SceneInput, ServiceError, Store};

const PLUGIN: &str = "t-preflight-sink";

const LAUNCH_SCHEMA: &str = r#"{"type": "object", "additionalProperties": false, "properties": {
    "spec": {"type": "string"}, "device": {"type": "string"},
    "token": {"type": "string", "writeOnly": true, "x-flwrs-launch": {"via": "env", "alias": "TOKEN"}},
    "derived": {"type": "string"}}}"#;

/// Answers from `state`; counts its pre-flight runs in `runs`.
fn configurator_script(dir: &Path) -> String {
    let d = dir.display();
    format!(
        r#"#!/bin/sh
if [ "$1" != --mode ]; then echo '{{"values": {{"derived": "v"}}}}' > response.json; exit 0; fi
echo run >> '{d}/runs'
cp request.json '{d}/last-request'
case "$(cat '{d}/state')" in
  exclusive)
    device=$(sed '/"releasing"/,$d' request.json | sed -n 's/.*"device": "\([^"]*\)".*/\1/p' | head -n 1)
    if grep -qx "$device" '{d}/held' && ! sed -n '/"releasing"/,$p' request.json | grep -q "\"device\": \"$device\""; then
      printf '{{"problems": [{{"message": "%s busy", "retryable": true}}]}}' "$device" > response.json
    else echo '{{}}' > response.json; fi;;
  busy) echo '{{"problems": [{{"message": "camera busy", "retryable": true}}]}}' > response.json;;
  broken) echo '{{"problems": [{{"message": "no such device"}}]}}' > response.json;;
  hang) exec sleep 30;;
  values) echo '{{"values": {{"derived": "x"}}}}' > response.json;;
  crash) echo 'GLIBC_2.38 not found' >&2; exit 127;;
  *) echo '{{}}' > response.json;;
esac
"#
    )
}

async fn install(s: &Setup) {
    let path = s.f.plugins.service.new_upload_path();
    let manifest = format!(
        r#"{{"manifest_version": 2, "id": "{PLUGIN}", "name": "P", "version": "1.0.0", "type": "SINK",
            "executable": "bin/run", "platforms": ["{PLATFORM}"], "launch_args_schema": "schema/launch.json",
            "payload_schemas": {{"in": "schema/in.json"}},
            "configurator": {{"executable": "bin/configure", "fields": ["derived"], "preflight": true}}}}"#
    );
    // Notes an overlap with the node's previous process; `run_state` "crash"
    // is a version that can't start (a device still locked).
    let d = s.dir.path().display();
    let run = format!(
        "#!/bin/sh\n\
         if [ -f .pid ] && kill -0 \"$(cat .pid)\" 2>/dev/null; then echo overlap >> '{d}/overlaps'; fi\n\
         if [ \"$(cat '{d}/run_state' 2>/dev/null)\" = crash ]; then echo 'camera locked' >&2; exit 1; fi\n\
         echo $$ > .pid\n\
         exec '{}' \"$@\"\n",
        plugin_scripts().join("sink.sh").display()
    );
    let script = configurator_script(s.dir.path());
    build_zip(
        &path,
        &[
            Item::File("manifest.json", manifest.as_bytes()),
            Item::Dir("bin/"),
            Item::File("bin/run", run.as_bytes()),
            Item::File("bin/configure", script.as_bytes()),
            Item::File("schema/launch.json", LAUNCH_SCHEMA.as_bytes()),
            Item::File("schema/in.json", TEXT_SCHEMA_JSON.as_bytes()),
        ],
    );
    s.f.plugins.service.install(&path).await.unwrap();
}

fn set_state(s: &Setup, state: &str) {
    // Atomic: a check may be reading it.
    let partial = s.dir.path().join("state.new");
    std::fs::write(&partial, state).unwrap();
    std::fs::rename(&partial, s.dir.path().join("state")).unwrap();
}

fn runs(s: &Setup) -> usize {
    std::fs::read_to_string(s.dir.path().join("runs"))
        .unwrap_or_default()
        .lines()
        .count()
}

async fn prepared(state: &str) -> Setup {
    let s = setup().await;
    s.f.plugins.service.set_configure_env(configurator::Env {
        files_dir: s.f.service.files().files_dir.clone(),
        limits: s.f.service.configure_limits(),
    });
    set_state(&s, state);
    install(&s).await;
    s
}

async fn with_retry(s: &Setup, scene: &Scene, retry: PreflightRetryOverride) {
    s.f.service
        .update_scene(
            &scene.id,
            &SceneInput {
                name: scene.name.clone(),
                max_hops: scene.max_hops,
                defaults: SceneDefaults {
                    preflight_retry: Some(retry),
                    ..Default::default()
                },
                groups: None,
            },
        )
        .await
        .unwrap();
}

fn node_config(spec: &str) -> serde_json::Value {
    json!({"launch_args": {"spec": spec}})
}

async fn wait_state(s: &Setup, scene: &str, state: SceneState) -> SceneStatus {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some(status) = s.director.status(scene).await.unwrap()
            && status.state == state
        {
            return status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {state:?}: {:?}",
            s.director.status(scene).await
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits until the wait shows a problem (the first check has answered).
async fn wait_problem(s: &Setup, scene: &str) -> SceneStatus {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let status = wait_state(s, scene, SceneState::Waiting).await;
        if !status.preflight.is_empty() {
            return status;
        }
        assert!(tokio::time::Instant::now() < deadline, "no problem shown");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_node_running(s: &Setup, scene: &str, node: &str) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let status = s.status(scene).await;
        if status.nodes.iter().any(|n| {
            n.node_id == node && n.status.state == crate::modules::supervisor::NodeState::Running
        }) {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "{status:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Enables the scene and tells this test's director (the fixture's service
/// reports to a channel of its own).
async fn enable(s: &Setup, scene: &str) {
    s.f.service.set_enabled(scene, true).await.unwrap();
    s.director.request_reload(scene);
}

async fn disable(s: &Setup, scene: &str) {
    s.f.service.set_enabled(scene, false).await.unwrap();
    s.director.request_stop(scene);
}

#[tokio::test]
async fn enabling_is_refused_on_a_permanent_problem() {
    let mut s = prepared("broken").await;
    s.start_director(32).await;
    let scene = s.f.scene().await;
    let node = s.node_with(&scene.id, PLUGIN, node_config("a")).await;
    match s.f.service.set_enabled(&scene.id, true).await.unwrap_err() {
        ServiceError::Conflict(message, details) => {
            assert_eq!(message, "scene can't be enabled");
            assert_eq!(details, [format!("node {node}: no such device")]);
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert!(!s.f.service.get_scene(&scene.id).await.unwrap().enabled);
    // Fixed: it goes through.
    set_state(&s, "ok");
    enable(&s, &scene.id).await;
    wait_state(&s, &scene.id, SceneState::Running).await;
    s.stop_director().await;
}

#[tokio::test]
async fn a_binary_that_crashes_refuses_the_enable_with_its_stderr() {
    let s = prepared("crash").await;
    let scene = s.f.scene().await;
    s.node_with(&scene.id, PLUGIN, node_config("a")).await;
    match s.f.service.set_enabled(&scene.id, true).await.unwrap_err() {
        ServiceError::Conflict(_, details) => {
            assert!(details[0].contains("GLIBC_2.38"), "{details:?}")
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn a_response_with_values_is_refused() {
    let s = prepared("values").await;
    let scene = s.f.scene().await;
    let node = s.node_with(&scene.id, PLUGIN, node_config("a")).await;
    let run =
        s.f.service
            .preflight(&scene.id, Some(&node), false)
            .await
            .unwrap();
    assert!(run.permanent, "{run:?}");
    assert!(
        run.nodes[0].problems[0].message.contains("read-only"),
        "{run:?}"
    );
    // The stored settings are untouched.
    let stored = s.f.service.get_node(&scene.id, &node).await.unwrap();
    assert_eq!(stored.config.launch_args["derived"], "v");
}

#[tokio::test]
async fn waiting_turns_into_running_when_the_problem_goes_away() {
    let mut s = prepared("busy").await;
    s.start_director(32).await;
    let scene = s.f.scene().await;
    let node = s.node_with(&scene.id, PLUGIN, node_config("a")).await;
    // Retryable: the enable goes through.
    enable(&s, &scene.id).await;
    let status = wait_problem(&s, &scene.id).await;
    assert_eq!(status.preflight[0].node_id, node);
    assert!(status.preflight[0].retryable);
    assert_eq!(status.errors, [format!("node {node}: camera busy")]);
    assert_eq!(status.preflight_retry.as_ref().unwrap().max_attempts, 10);
    assert!(status.nodes.is_empty(), "nothing started yet");
    set_state(&s, "ok");
    wait_state(&s, &scene.id, SceneState::Running).await;
    wait_node_running(&s, &scene.id, &node).await;
    let status = s.status(&scene.id).await;
    assert!(status.preflight.is_empty() && status.preflight_retry.is_none());
    s.stop_director().await;
}

#[tokio::test]
async fn waiting_fails_at_the_limit() {
    let mut s = prepared("busy").await;
    s.start_director(32).await;
    let scene = s.f.scene().await;
    with_retry(
        &s,
        &scene,
        PreflightRetryOverride {
            max_attempts: Some(2),
            initial_delay: Some("1s".into()),
            ..Default::default()
        },
    )
    .await;
    let node = s.node_with(&scene.id, PLUGIN, node_config("a")).await;
    enable(&s, &scene.id).await;
    let status = wait_state(&s, &scene.id, SceneState::Failed).await;
    assert!(
        status.errors[0].contains(&format!("node {node}: camera busy")),
        "{status:?}"
    );
    assert!(
        status.errors.iter().any(|e| e.contains("gave up after 2")),
        "{status:?}"
    );
    assert_eq!(status.preflight.len(), 1);
    s.stop_director().await;
}

#[tokio::test]
async fn stopping_during_waiting_takes_effect_at_once() {
    let mut s = prepared("busy").await;
    s.start_director(32).await;
    let scene = s.f.scene().await;
    with_retry(
        &s,
        &scene,
        PreflightRetryOverride {
            initial_delay: Some("30s".into()),
            ..Default::default()
        },
    )
    .await;
    s.node_with(&scene.id, PLUGIN, node_config("a")).await;
    enable(&s, &scene.id).await;
    wait_problem(&s, &scene.id).await;
    let started = std::time::Instant::now();
    disable(&s, &scene.id).await;
    wait_state(&s, &scene.id, SceneState::Stopped).await;
    assert!(started.elapsed() < Duration::from_secs(5));
    s.stop_director().await;
}

#[tokio::test]
async fn stopping_kills_a_check_that_hangs_and_other_scenes_carry_on() {
    let mut s = prepared("ok").await;
    s.start_director(32).await;
    let hung = s.f.scene().await;
    s.node_with(&hung.id, PLUGIN, node_config("a")).await;
    s.f.service.set_enabled(&hung.id, true).await.unwrap();
    // The enable's own check passed; the director's hangs.
    set_state(&s, "hang");
    let before = runs(&s);
    s.director.request_reload(&hung.id);
    let deadline = tokio::time::Instant::now() + WAIT;
    while runs(&s) <= before {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    wait_state(&s, &hung.id, SceneState::Waiting).await;
    // Another scene starts meanwhile.
    let other = s.f.scene().await;
    let sink = s.node(&other.id, "t-sink").await;
    enable(&s, &other.id).await;
    wait_state(&s, &other.id, SceneState::Running).await;
    wait_node_running(&s, &other.id, &sink).await;
    let started = std::time::Instant::now();
    disable(&s, &hung.id).await;
    wait_state(&s, &hung.id, SceneState::Stopped).await;
    assert!(started.elapsed() < Duration::from_secs(5));
    s.stop_director().await;
}

#[tokio::test]
async fn a_reload_checks_only_the_changed_node_and_the_rest_keep_running() {
    let mut s = prepared("ok").await;
    s.start_director(32).await;
    let scene = s.f.scene().await;
    let kept = s.node_with(&scene.id, PLUGIN, node_config("kept")).await;
    let changed = s.node_with(&scene.id, PLUGIN, node_config("old")).await;
    enable(&s, &scene.id).await;
    wait_state(&s, &scene.id, SceneState::Running).await;
    wait_node_running(&s, &scene.id, &kept).await;
    wait_node_running(&s, &scene.id, &changed).await;
    let before = s.processes(&scene.id).await;

    set_state(&s, "busy");
    s.f.service
        .update_node(&scene.id, &changed, node_input(PLUGIN, node_config("new")))
        .await
        .unwrap();
    s.director.request_reload(&scene.id);
    let status = wait_problem(&s, &scene.id).await;
    // Only the changed node was checked; everything still runs meanwhile.
    assert_eq!(status.preflight.len(), 1, "{status:?}");
    assert_eq!(status.preflight[0].node_id, changed);
    assert_eq!(status.nodes.len(), 2);
    assert_eq!(s.processes(&scene.id).await, before);

    set_state(&s, "ok");
    wait_state(&s, &scene.id, SceneState::Running).await;
    wait_node_running(&s, &scene.id, &changed).await;
    let after = s.processes(&scene.id).await;
    assert_eq!(
        after[&kept], before[&kept],
        "the unchanged node kept running"
    );
    assert_ne!(
        after[&changed].0, before[&changed].0,
        "the changed node restarted"
    );
    s.stop_director().await;
}

#[tokio::test]
async fn a_direct_edit_failing_preflight_keeps_the_old_graph_until_a_good_reload() {
    let mut s = prepared("ok").await;
    s.start_director(32).await;
    let scene = s.f.scene().await;
    let node = s.node_with(&scene.id, PLUGIN, node_config("a")).await;
    enable(&s, &scene.id).await;
    wait_state(&s, &scene.id, SceneState::Running).await;
    wait_node_running(&s, &scene.id, &node).await;
    let before = s.processes(&scene.id).await;

    set_state(&s, "broken");
    s.f.service
        .update_node(&scene.id, &node, node_input(PLUGIN, node_config("b")))
        .await
        .unwrap();
    s.director.request_reload(&scene.id);
    let status = wait_failed_update(&s, &scene.id).await;
    assert_eq!(status.state, SceneState::Running);
    let failed = status.failed_update.as_ref().unwrap();
    assert_eq!(failed.errors, [format!("node {node}: no such device")]);
    assert_eq!(failed.problems[0].node_id, node);
    assert_eq!(
        s.processes(&scene.id).await,
        before,
        "the old process runs on"
    );

    // A later good reload takes the update, and clears the report.
    set_state(&s, "ok");
    s.director.reload(&scene.id).await.unwrap();
    wait_state(&s, &scene.id, SceneState::Running).await;
    wait_node_running(&s, &scene.id, &node).await;
    let status = s.status(&scene.id).await;
    assert!(status.failed_update.is_none(), "{status:?}");
    assert_ne!(s.processes(&scene.id).await[&node].0, before[&node].0);
    s.stop_director().await;
}

#[tokio::test]
async fn a_reload_hitting_the_retry_limit_keeps_the_old_graph() {
    let mut s = prepared("ok").await;
    s.start_director(32).await;
    let scene = s.f.scene().await;
    with_retry(
        &s,
        &scene,
        PreflightRetryOverride {
            max_attempts: Some(1),
            ..Default::default()
        },
    )
    .await;
    let node = s.node_with(&scene.id, PLUGIN, node_config("a")).await;
    enable(&s, &scene.id).await;
    wait_state(&s, &scene.id, SceneState::Running).await;
    wait_node_running(&s, &scene.id, &node).await;
    let before = s.processes(&scene.id).await;
    set_state(&s, "busy");
    s.f.service
        .update_node(&scene.id, &node, node_input(PLUGIN, node_config("b")))
        .await
        .unwrap();
    s.director.request_reload(&scene.id);
    let status = wait_failed_update(&s, &scene.id).await;
    assert_eq!(status.state, SceneState::Running);
    assert!(
        status
            .failed_update
            .unwrap()
            .errors
            .iter()
            .any(|e| e.contains("gave up after 1")),
    );
    assert_eq!(s.processes(&scene.id).await, before);
    s.stop_director().await;
}

// ---- drafts: checked before anything is committed ----

/// A running scene with one node, and a draft changing it.
async fn running_with_draft(
    s: &mut Setup,
    retry: Option<PreflightRetryOverride>,
) -> (String, String) {
    s.start_director(32).await;
    let scene = s.f.scene().await;
    if let Some(retry) = retry {
        with_retry(s, &scene, retry).await;
    }
    let node = s.node_with(&scene.id, PLUGIN, node_config("old")).await;
    enable(s, &scene.id).await;
    wait_state(s, &scene.id, SceneState::Running).await;
    wait_node_running(s, &scene.id, &node).await;
    s.f.service
        .save_node(
            Store::Draft,
            &scene.id,
            Some(&node),
            node_input(PLUGIN, node_config("new")),
        )
        .await
        .unwrap();
    (scene.id, node)
}

async fn live_spec(s: &Setup, scene: &str, node: &str) -> String {
    let n = s.f.service.get_node(scene, node).await.unwrap();
    n.config.launch_args["spec"].as_str().unwrap().to_string()
}

async fn wait_draft(s: &Setup, scene: &str, test: impl Fn(&DraftSubmit) -> bool) -> DraftSubmit {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Ok(draft) = s.f.service.get_draft(scene).await
            && let Some(submit) = draft.submit
            && test(&submit)
        {
            return submit;
        }
        assert!(tokio::time::Instant::now() < deadline, "the draft's mark");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_committed(s: &Setup, scene: &str) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while s.f.service.get_draft(scene).await.is_ok() {
        assert!(tokio::time::Instant::now() < deadline, "not committed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn is_failed(d: &DraftSubmit) -> bool {
    d.state == DraftSubmitState::Failed
}

/// Waiting, with the first check's problems in.
fn is_waiting(d: &DraftSubmit) -> bool {
    d.state == DraftSubmitState::Waiting && !d.problems.is_empty()
}

#[tokio::test]
async fn a_draft_with_a_permanent_problem_commits_nothing() {
    let mut s = prepared("ok").await;
    let (scene, node) = running_with_draft(&mut s, None).await;
    let before = s.processes(&scene).await;
    let revision = s.f.service.get_scene(&scene).await.unwrap().revision;
    set_state(&s, "broken");
    match s.f.service.submit(&scene, None, false).await.unwrap_err() {
        ServiceError::Conflict(_, details) => {
            assert_eq!(details, [format!("node {node}: no such device")])
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(live_spec(&s, &scene, &node).await, "old");
    assert_eq!(
        s.f.service.get_scene(&scene).await.unwrap().revision,
        revision
    );
    assert_eq!(s.processes(&scene).await, before, "the old process runs on");
    assert_eq!(s.status(&scene).await.state, SceneState::Running);
    let mark = s.f.service.get_draft(&scene).await.unwrap().submit.unwrap();
    assert_eq!(mark.state, DraftSubmitState::Failed);
    assert_eq!(mark.problems[0].message, "no such device");
    // An edit clears the mark.
    s.f.service
        .save_node(
            Store::Draft,
            &scene,
            Some(&node),
            node_input(PLUGIN, node_config("newer")),
        )
        .await
        .unwrap();
    assert!(
        s.f.service
            .get_draft(&scene)
            .await
            .unwrap()
            .submit
            .is_none()
    );
    // And a good submit commits at once.
    set_state(&s, "ok");
    assert!(matches!(
        s.f.service.submit(&scene, None, false).await.unwrap(),
        Submitted::Committed(_)
    ));
    assert_eq!(live_spec(&s, &scene, &node).await, "newer");
    s.stop_director().await;
}

#[tokio::test]
async fn a_draft_waits_out_retryable_problems_then_commits() {
    let mut s = prepared("ok").await;
    let (scene, node) = running_with_draft(&mut s, None).await;
    set_state(&s, "busy");
    assert!(matches!(
        s.f.service.submit(&scene, None, false).await.unwrap(),
        Submitted::Waiting(_)
    ));
    let mark = wait_draft(&s, &scene, is_waiting).await;
    assert_eq!(mark.problems[0].node_id, node);
    assert_eq!(live_spec(&s, &scene, &node).await, "old");
    set_state(&s, "ok");
    wait_committed(&s, &scene).await;
    assert_eq!(live_spec(&s, &scene, &node).await, "new");
    s.stop_director().await;
}

#[tokio::test]
async fn a_draft_at_the_retry_limit_leaves_live_unchanged() {
    let mut s = prepared("ok").await;
    let retry = PreflightRetryOverride {
        max_attempts: Some(2),
        ..Default::default()
    };
    let (scene, node) = running_with_draft(&mut s, Some(retry)).await;
    let before = s.processes(&scene).await;
    set_state(&s, "busy");
    assert!(matches!(
        s.f.service.submit(&scene, None, false).await.unwrap(),
        Submitted::Waiting(_)
    ));
    let mark = wait_draft(&s, &scene, is_failed).await;
    assert!(mark.message.unwrap().contains("gave up after 2"));
    assert_eq!(live_spec(&s, &scene, &node).await, "old");
    assert_eq!(s.processes(&scene).await, before);
    s.stop_director().await;
}

#[tokio::test]
async fn an_edit_during_the_wait_cancels_it() {
    let mut s = prepared("ok").await;
    let (scene, node) = running_with_draft(&mut s, None).await;
    set_state(&s, "busy");
    s.f.service.submit(&scene, None, false).await.unwrap();
    wait_draft(&s, &scene, is_waiting).await;
    s.f.service
        .save_node(
            Store::Draft,
            &scene,
            Some(&node),
            node_input(PLUGIN, node_config("edited")),
        )
        .await
        .unwrap();
    assert!(
        s.f.service
            .get_draft(&scene)
            .await
            .unwrap()
            .submit
            .is_none()
    );
    wait_no_submit_wait(&s, &scene).await;
    // The check would pass now: nothing is left to commit it.
    set_state(&s, "ok");
    let runs_then = runs(&s);
    tokio::task::yield_now().await;
    assert_eq!(runs(&s), runs_then);
    assert_eq!(live_spec(&s, &scene, &node).await, "old");
    assert!(s.f.service.get_draft(&scene).await.is_ok());
    s.stop_director().await;
}

#[tokio::test]
async fn live_moving_on_during_the_wait_stops_the_commit() {
    let mut s = prepared("ok").await;
    let (scene, node) = running_with_draft(&mut s, None).await;
    set_state(&s, "busy");
    s.f.service.submit(&scene, None, false).await.unwrap();
    wait_draft(&s, &scene, is_waiting).await;
    set_state(&s, "ok");
    s.f.service
        .update_node(&scene, &node, node_input(PLUGIN, node_config("direct")))
        .await
        .unwrap();
    let mark = wait_draft(&s, &scene, is_failed).await;
    assert!(mark.message.unwrap().contains("live changed"));
    assert_eq!(live_spec(&s, &scene, &node).await, "direct");
    assert!(s.f.service.get_draft(&scene).await.is_ok());
    s.stop_director().await;
}

#[tokio::test]
async fn a_restart_turns_a_waiting_draft_into_a_failed_one() {
    let mut s = prepared("ok").await;
    let (scene, node) = running_with_draft(&mut s, None).await;
    set_state(&s, "busy");
    s.f.service.submit(&scene, None, false).await.unwrap();
    wait_draft(&s, &scene, is_waiting).await;
    // What start-up does (the wait's task is gone with the old process).
    s.f.service.fail_stale_submits().await.unwrap();
    let mark = s.f.service.get_draft(&scene).await.unwrap().submit.unwrap();
    assert_eq!(mark.state, DraftSubmitState::Failed);
    assert!(mark.message.unwrap().contains("restarted"));
    assert_eq!(mark.problems.len(), 1, "the last problems stay");
    assert_eq!(live_spec(&s, &scene, &node).await, "old");
    s.stop_director().await;
}

#[tokio::test]
async fn a_submit_that_disables_commits_without_a_check() {
    let mut s = prepared("ok").await;
    let (scene, node) = running_with_draft(&mut s, None).await;
    set_state(&s, "broken");
    let runs_before = runs(&s);
    assert!(matches!(
        s.f.service.submit(&scene, None, true).await.unwrap(),
        Submitted::Committed(_)
    ));
    assert_eq!(runs(&s), runs_before);
    assert_eq!(live_spec(&s, &scene, &node).await, "new");
    s.stop_director().await;
}

#[tokio::test]
async fn disabling_during_the_wait_cancels_it() {
    let mut s = prepared("ok").await;
    let (scene, node) = running_with_draft(&mut s, None).await;
    set_state(&s, "busy");
    s.f.service.submit(&scene, None, false).await.unwrap();
    wait_draft(&s, &scene, is_waiting).await;
    disable(&s, &scene).await;
    assert!(
        s.f.service
            .get_draft(&scene)
            .await
            .unwrap()
            .submit
            .is_none()
    );
    wait_no_submit_wait(&s, &scene).await;
    assert_eq!(live_spec(&s, &scene, &node).await, "old");
    s.stop_director().await;
}

// ---- what the running version holds ----

fn camera(device: &str, spec: &str) -> serde_json::Value {
    json!({"launch_args": {"spec": spec, "device": device}})
}

fn hold(s: &Setup, devices: &[&str]) {
    std::fs::write(s.dir.path().join("held"), devices.join("\n") + "\n").unwrap();
}

/// A running scene with these nodes, each on its device, whose check reports
/// a held device as busy; one check per submit.
async fn cameras(s: &mut Setup, devices: &[&str]) -> (String, Vec<String>) {
    s.start_director(32).await;
    let scene = s.f.scene().await;
    with_retry(
        s,
        &scene,
        PreflightRetryOverride {
            max_attempts: Some(1),
            ..Default::default()
        },
    )
    .await;
    let mut nodes = vec![];
    for device in devices {
        nodes.push(s.node_with(&scene.id, PLUGIN, camera(device, "a")).await);
    }
    enable(s, &scene.id).await;
    wait_state(s, &scene.id, SceneState::Running).await;
    for node in &nodes {
        wait_node_running(s, &scene.id, node).await;
    }
    hold(s, devices);
    set_state(s, "exclusive");
    (scene.id, nodes)
}

async fn draft_node(s: &Setup, scene: &str, node: &str, config: serde_json::Value) {
    s.f.service
        .save_node(Store::Draft, scene, Some(node), node_input(PLUGIN, config))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_device_held_by_the_nodes_own_running_version_isnt_busy() {
    let mut s = prepared("ok").await;
    let (scene, nodes) = cameras(&mut s, &["cam0"]).await;
    draft_node(&s, &scene, &nodes[0], camera("cam0", "b")).await;
    assert!(matches!(
        s.f.service.submit(&scene, None, false).await.unwrap(),
        Submitted::Committed(_)
    ));
    assert_eq!(live_spec(&s, &scene, &nodes[0]).await, "b");
    s.stop_director().await;
}

#[tokio::test]
async fn a_device_held_by_a_node_that_keeps_running_is_still_busy() {
    let mut s = prepared("ok").await;
    let (scene, nodes) = cameras(&mut s, &["cam0", "cam1"]).await;
    draft_node(&s, &scene, &nodes[1], camera("cam0", "a")).await;
    match s.f.service.submit(&scene, None, false).await.unwrap_err() {
        ServiceError::Conflict(_, details) => {
            assert!(details[0].contains("cam0 busy"), "{details:?}")
        }
        other => panic!("{other:?}"),
    }
    let request = std::fs::read_to_string(s.dir.path().join("last-request")).unwrap();
    assert!(
        request.contains(&nodes[1]) && !request.contains(&format!("\"node_id\": \"{}\"", nodes[0]))
    );
    s.stop_director().await;
}

#[tokio::test]
async fn devices_swapped_between_two_nodes_in_one_draft_pass() {
    let mut s = prepared("ok").await;
    let (scene, nodes) = cameras(&mut s, &["cam0", "cam1"]).await;
    draft_node(&s, &scene, &nodes[0], camera("cam1", "a")).await;
    draft_node(&s, &scene, &nodes[1], camera("cam0", "a")).await;
    assert!(matches!(
        s.f.service.submit(&scene, None, false).await.unwrap(),
        Submitted::Committed(_)
    ));
    s.stop_director().await;
}

#[tokio::test]
async fn checking_a_running_scene_doesnt_report_a_node_as_blocking_itself() {
    let mut s = prepared("ok").await;
    s.start_director(32).await;
    let scene = s.f.scene().await;
    let config = |device: &str| json!({"launch_args": {"device": device}});
    let node = s.node_with(&scene.id, PLUGIN, config("cam0")).await;
    let other = s.node_with(&scene.id, PLUGIN, config("cam1")).await;
    enable(&s, &scene.id).await;
    wait_state(&s, &scene.id, SceneState::Running).await;
    wait_node_running(&s, &scene.id, &node).await;
    wait_node_running(&s, &scene.id, &other).await;

    // Both devices are held, each by its own node.
    std::fs::write(s.dir.path().join("held"), "cam0\ncam1\n").unwrap();
    set_state(&s, "exclusive");
    let run = s.f.service.preflight(&scene.id, None, false).await.unwrap();
    assert!(run.ok, "{run:?}");
    let one =
        s.f.service
            .preflight(&scene.id, Some(&node), false)
            .await
            .unwrap();
    assert!(one.ok, "{one:?}");
    assert_eq!(one.nodes.len(), 1);
    s.stop_director().await;
}

#[tokio::test]
async fn a_draft_change_that_keeps_the_process_isnt_checked() {
    let mut s = prepared("ok").await;
    s.start_director(32).await;
    let scene = s.f.scene().await;
    let config = |critical: bool| json!({"launch_args": {"device": "cam0"}, "runtime": {"is_critical_path": critical}});
    let node = s.node_with(&scene.id, PLUGIN, config(false)).await;
    enable(&s, &scene.id).await;
    wait_state(&s, &scene.id, SceneState::Running).await;
    wait_node_running(&s, &scene.id, &node).await;

    // The node holds its camera; a check would call it busy.
    std::fs::write(s.dir.path().join("held"), "cam0\n").unwrap();
    set_state(&s, "exclusive");
    let before = runs(&s);
    draft_node(&s, &scene.id, &node, config(true)).await;
    assert!(matches!(
        s.f.service.submit(&scene.id, None, false).await.unwrap(),
        Submitted::Committed(_)
    ));
    assert_eq!(runs(&s), before, "no check for a node that keeps running");
    s.stop_director().await;
}

#[tokio::test]
async fn releasing_carries_no_secrets() {
    let mut s = prepared("ok").await;
    s.start_director(32).await;
    let scene = s.f.scene().await;
    let with_token = |spec: &str| json!({"launch_args": {"spec": spec, "device": "cam0", "token": "s3cret-token"}});
    let node = s.node_with(&scene.id, PLUGIN, with_token("a")).await;
    enable(&s, &scene.id).await;
    wait_state(&s, &scene.id, SceneState::Running).await;
    wait_node_running(&s, &scene.id, &node).await;
    draft_node(&s, &scene.id, &node, with_token("b")).await;
    s.f.service.submit(&scene.id, None, false).await.unwrap();
    let request = std::fs::read_to_string(s.dir.path().join("last-request")).unwrap();
    let request: serde_json::Value = serde_json::from_str(&request).unwrap();
    assert_eq!(request["releasing"][0]["node_id"], node.as_str());
    assert_eq!(request["releasing"][0]["launch_args"]["device"], "cam0");
    assert!(!request.to_string().contains("s3cret-token"), "{request}");
    s.stop_director().await;
}

#[tokio::test]
async fn a_new_version_that_cant_start_after_the_swap_fails_the_scene() {
    let mut s = prepared("ok").await;
    s.start_director(32).await;
    let scene = s.f.scene().await;
    let config = |spec: &str| json!({"launch_args": {"spec": spec}, "runtime": {"max_restarts": 1, "restart_delay_ms": 10, "is_critical_path": true}});
    let node = s.node_with(&scene.id, PLUGIN, config("a")).await;
    enable(&s, &scene.id).await;
    wait_state(&s, &scene.id, SceneState::Running).await;
    wait_node_running(&s, &scene.id, &node).await;

    // Pre-flight passes; the new version can't take the device.
    let run_state = s.dir.path().join("run_state");
    std::fs::write(&run_state, "crash").unwrap();
    draft_node(&s, &scene.id, &node, config("b")).await;
    assert!(matches!(
        s.f.service.submit(&scene.id, None, false).await.unwrap(),
        Submitted::Committed(_)
    ));
    s.director.request_reload(&scene.id);
    let status = wait_state(&s, &scene.id, SceneState::Failed).await;
    assert_eq!(status.errors.len(), 1, "{status:?}");
    let error = &status.errors[0];
    assert!(error.contains(&format!("node {node}")), "{error}");
    assert!(
        error.contains("switch-over") && error.contains("may not have released"),
        "{error}"
    );
    assert!(error.contains("camera locked"), "{error}");
    assert!(status.nodes.is_empty());
    assert_eq!(
        live_spec(&s, &scene.id, &node).await,
        "b",
        "stays committed"
    );
    assert!(
        !s.dir.path().join("overlaps").exists(),
        "the old process had stopped before the new one launched"
    );

    // A person fixes it: disable and enable starts clean.
    std::fs::remove_file(&run_state).unwrap();
    disable(&s, &scene.id).await;
    wait_state(&s, &scene.id, SceneState::Stopped).await;
    enable(&s, &scene.id).await;
    wait_state(&s, &scene.id, SceneState::Running).await;
    wait_node_running(&s, &scene.id, &node).await;
    assert!(s.status(&scene.id).await.errors.is_empty());
    s.stop_director().await;
}

#[tokio::test]
async fn a_node_that_isnt_critical_goes_offline_and_the_scene_runs_on() {
    let mut s = prepared("ok").await;
    s.start_director(32).await;
    let scene = s.f.scene().await;
    let config = |spec: &str| json!({"launch_args": {"spec": spec}, "runtime": {"max_restarts": 1, "restart_delay_ms": 10}});
    let node = s.node_with(&scene.id, PLUGIN, config("a")).await;
    enable(&s, &scene.id).await;
    wait_state(&s, &scene.id, SceneState::Running).await;
    wait_node_running(&s, &scene.id, &node).await;

    let run_state = s.dir.path().join("run_state");
    std::fs::write(&run_state, "crash").unwrap();
    draft_node(&s, &scene.id, &node, config("b")).await;
    s.f.service.submit(&scene.id, None, false).await.unwrap();
    s.director.request_reload(&scene.id);
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let status = s.status(&scene.id).await;
        if status.nodes.iter().any(|n| {
            n.node_id == node && n.status.state == crate::modules::supervisor::NodeState::Failed
        }) {
            assert_eq!(status.state, SceneState::Running, "{status:?}");
            assert!(status.errors.is_empty(), "{status:?}");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{status:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // A reload brings it back.
    std::fs::remove_file(&run_state).unwrap();
    s.director.request_reload(&scene.id);
    wait_node_running(&s, &scene.id, &node).await;
    s.stop_director().await;
}

#[tokio::test]
async fn a_critical_node_that_cant_start_fails_the_scene_at_its_start() {
    let mut s = prepared("ok").await;
    s.start_director(32).await;
    let scene = s.f.scene().await;
    std::fs::write(s.dir.path().join("run_state"), "crash").unwrap();
    let config = json!({"launch_args": {"spec": "a"}, "runtime": {"is_critical_path": true, "restart_delay_ms": 1}});
    let node = s.node_with(&scene.id, PLUGIN, config).await;
    enable(&s, &scene.id).await;
    let status = wait_state(&s, &scene.id, SceneState::Failed).await;
    assert!(
        status.errors[0].contains(&format!("critical node {node}")),
        "{status:?}"
    );
    s.stop_director().await;
}

/// Until the scene's wait task has finished.
async fn wait_no_submit_wait(s: &Setup, scene: &str) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while s.f.service.has_submit_wait(scene) {
        assert!(tokio::time::Instant::now() < deadline, "the wait goes on");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_failed_update(s: &Setup, scene: &str) -> SceneStatus {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let status = s.status(scene).await;
        if status.failed_update.is_some() {
            return status;
        }
        assert!(tokio::time::Instant::now() < deadline, "{status:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn invalid_scene_limits_are_refused() {
    let s = prepared("ok").await;
    let scene = s.f.scene().await;
    let e =
        s.f.service
            .update_scene(
                &scene.id,
                &SceneInput {
                    name: scene.name.clone(),
                    max_hops: None,
                    defaults: SceneDefaults {
                        preflight_retry: Some(PreflightRetryOverride {
                            max_attempts: Some(0),
                            max_wait: Some("soon".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    groups: None,
                },
            )
            .await
            .unwrap_err();
    match e {
        ServiceError::Invalid(problems) => assert_eq!(problems.len(), 2, "{problems:?}"),
        other => panic!("{other:?}"),
    }
}
