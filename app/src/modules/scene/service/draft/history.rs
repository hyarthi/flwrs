//! Undo / redo for drafts: every state a draft has been in is recorded
//! after each change ([`record`], called by `query::touch`, which every
//! draft write ends with, and when a draft starts). A state is a JSON
//! snapshot of the draft's rows (settings, nodes, edges, uploads; layout
//! too), built and restored by SQLite's JSON functions. `history_at` points
//! at the current state; undo restores the one before it, redo the one
//! after; a change after an undo drops the states after it.
//!
//! Requests can share an undo group ([`with_group`], the API's
//! `X-Flwrs-Undo-Group` header): consecutive changes in one group make a
//! single step (e.g. a paste of several nodes and edges).
//!
//! Bounded per draft: [`MAX_STEPS`] states and [`MAX_BYTES`] of snapshots
//! (the oldest go). Uploads a snapshot names are kept from garbage
//! collection (`scene_draft_history_uploads`).

use serde_json::Value;
use sqlx::SqliteConnection;

/// States kept per draft.
pub(crate) const MAX_STEPS: i64 = 100;
/// Snapshot bytes kept per draft.
const MAX_BYTES: i64 = 16 * 1024 * 1024;

tokio::task_local! {
    /// The undo group (and the step's label) of the request being handled.
    static GROUP: (String, Option<String>);
}

/// Runs `f` with changes recorded in `group` (merged into one step), named
/// `label` if given.
pub(crate) async fn with_group<F: std::future::Future>(
    group: Option<(String, Option<String>)>,
    f: F,
) -> F::Output {
    match group {
        Some(g) => GROUP.scope(g, f).await,
        None => f.await,
    }
}

fn current_group() -> Option<(String, Option<String>)> {
    GROUP.try_with(Clone::clone).ok()
}

const SNAPSHOT: &str = "SELECT json_object(
    'draft', (SELECT json_object('base_revision', base_revision, 'name', \"name\", 'max_hops', max_hops,
                                 'defaults', defaults, 'groups', \"groups\", 'base_hash', base_hash, 'conflict', conflict)
              FROM scene_drafts WHERE scene_id = $1),
    'nodes', (SELECT json_group_array(json_object('id', id, 'name', \"name\", 'type', \"type\", 'plugin_id', plugin_id,
                 'plugin_version', plugin_version, 'config', config, 'create_time', create_time,
                 'update_time', update_time, 'base_hash', base_hash, 'removed', removed,
                 'conflict', conflict, 'ui', ui))
              FROM (SELECT * FROM scene_draft_nodes WHERE scene_id = $1 ORDER BY create_time, id)),
    'edges', (SELECT json_group_array(json_object('id', id, 'name', \"name\", 'source_node_id', source_node_id,
                 'target_node_id', target_node_id, 'mapping', mapping, 'create_time', create_time,
                 'update_time', update_time, 'base_hash', base_hash, 'removed', removed,
                 'conflict', conflict, 'ui', ui))
              FROM (SELECT * FROM scene_draft_edges WHERE scene_id = $1 ORDER BY create_time, id)),
    'uploads', (SELECT json_group_array(json_object('node_id', u.node_id, 'name', u.\"name\", 'sha256', u.sha256,
                   'size', u.\"size\", 'create_time', u.create_time))
                FROM scene_draft_node_uploads u JOIN scene_draft_nodes n ON n.id = u.node_id
                WHERE n.scene_id = $1))";

async fn snapshot(conn: &mut SqliteConnection, scene_id: &str) -> Result<String, sqlx::Error> {
    sqlx::query_scalar::<_, String>(SNAPSHOT)
        .bind(scene_id)
        .fetch_one(conn)
        .await
}

async fn history_at(conn: &mut SqliteConnection, scene_id: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT history_at FROM scene_drafts WHERE scene_id = $1")
        .bind(scene_id)
        .fetch_one(conn)
        .await
}

/// Records the draft's state after a change (in the change's transaction).
pub(crate) async fn record(conn: &mut SqliteConnection, scene_id: &str) -> Result<(), sqlx::Error> {
    record_in(conn, scene_id, current_group()).await
}

/// Records a new draft's first state: never part of the request's undo
/// group, so the group's changes stay undoable.
pub(crate) async fn record_start(
    conn: &mut SqliteConnection,
    scene_id: &str,
) -> Result<(), sqlx::Error> {
    record_in(conn, scene_id, None).await
}

async fn record_in(
    conn: &mut SqliteConnection,
    scene_id: &str,
    group: Option<(String, Option<String>)>,
) -> Result<(), sqlx::Error> {
    let now = snapshot(conn, scene_id).await?;
    let at = history_at(conn, scene_id).await?;
    let current: Option<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT snapshot, undo_group, label FROM scene_draft_history WHERE scene_id = $1 AND seq = $2",
    )
    .bind(scene_id)
    .bind(at)
    .fetch_optional(&mut *conn)
    .await?;
    if current.as_ref().is_some_and(|(s, _, _)| *s == now) {
        return Ok(()); // nothing changed (e.g. a refused or repeated write)
    }
    // States after the current one (undone) go.
    sqlx::query("DELETE FROM scene_draft_history WHERE scene_id = $1 AND seq > $2")
        .bind(scene_id)
        .bind(at)
        .execute(&mut *conn)
        .await?;

    let previous = current.as_ref().map(|(s, _, _)| s.as_str());
    let label = match (&group, previous) {
        (_, None) => "Start the draft".into(),
        (Some((_, Some(label))), _) => label.clone(),
        (_, Some(before)) => describe(before, &now),
    };
    let merge =
        matches!((&group, &current), (Some((g, _)), Some((_, Some(cg), _))) if g == cg) && at > 0;
    let seq = if merge { at } else { at + 1 };
    if merge {
        // The same group: this step's state moves on; its label stays its
        // first change's unless the group names it.
        let label = match &group {
            Some((_, Some(l))) => l.clone(),
            _ => current.as_ref().map(|(_, _, l)| l.clone()).unwrap_or(label),
        };
        sqlx::query("DELETE FROM scene_draft_history WHERE scene_id = $1 AND seq = $2")
            .bind(scene_id)
            .bind(seq)
            .execute(&mut *conn)
            .await?;
        insert(
            conn,
            scene_id,
            seq,
            &label,
            group.as_ref().map(|g| g.0.as_str()),
            &now,
        )
        .await?;
    } else {
        insert(
            conn,
            scene_id,
            seq,
            &label,
            group.as_ref().map(|g| g.0.as_str()),
            &now,
        )
        .await?;
    }
    sqlx::query("UPDATE scene_drafts SET history_at = $1 WHERE scene_id = $2")
        .bind(seq)
        .bind(scene_id)
        .execute(&mut *conn)
        .await?;
    trim(conn, scene_id, seq).await
}

async fn insert(
    conn: &mut SqliteConnection,
    scene_id: &str,
    seq: i64,
    label: &str,
    group: Option<&str>,
    snapshot: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO scene_draft_history (scene_id, seq, label, undo_group, snapshot, \"size\") \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(scene_id)
    .bind(seq)
    .bind(label)
    .bind(group)
    .bind(snapshot)
    .bind(i64::try_from(snapshot.len()).unwrap_or(i64::MAX))
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO scene_draft_history_uploads (scene_id, seq, \"name\", sha256) \
         SELECT $1, $2, json_extract(value, '$.name'), json_extract(value, '$.sha256') \
         FROM json_each(json_extract($3, '$.uploads'))",
    )
    .bind(scene_id)
    .bind(seq)
    .bind(snapshot)
    .execute(conn)
    .await?;
    Ok(())
}

/// Drops the oldest states beyond the bounds (never the current one).
async fn trim(conn: &mut SqliteConnection, scene_id: &str, at: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM scene_draft_history WHERE scene_id = $1 AND seq <= $2")
        .bind(scene_id)
        .bind(at - MAX_STEPS)
        .execute(&mut *conn)
        .await?;
    loop {
        let (total, oldest): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(\"size\"), 0), COALESCE(MIN(seq), 0) FROM scene_draft_history WHERE scene_id = $1",
        )
        .bind(scene_id)
        .fetch_one(&mut *conn)
        .await?;
        if total <= MAX_BYTES || oldest >= at {
            return Ok(());
        }
        sqlx::query("DELETE FROM scene_draft_history WHERE scene_id = $1 AND seq = $2")
            .bind(scene_id)
            .bind(oldest)
            .execute(&mut *conn)
            .await?;
    }
}

/// The labels of the step undo would take back and the one redo would
/// make again.
pub(crate) async fn steps(
    conn: &mut SqliteConnection,
    scene_id: &str,
) -> Result<(Option<String>, Option<String>), sqlx::Error> {
    let at = history_at(conn, scene_id).await?;
    let has_before: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM scene_draft_history WHERE scene_id = $1 AND seq < $2)",
    )
    .bind(scene_id)
    .bind(at)
    .fetch_one(&mut *conn)
    .await?;
    let undo = if has_before {
        sqlx::query_scalar("SELECT label FROM scene_draft_history WHERE scene_id = $1 AND seq = $2")
            .bind(scene_id)
            .bind(at)
            .fetch_optional(&mut *conn)
            .await?
    } else {
        None
    };
    let redo = sqlx::query_scalar(
        "SELECT label FROM scene_draft_history WHERE scene_id = $1 AND seq > $2 ORDER BY seq LIMIT 1",
    )
    .bind(scene_id)
    .bind(at)
    .fetch_optional(&mut *conn)
    .await?;
    Ok((undo, redo))
}

/// Moves to the state before (`back`) or after the current one and puts it
/// in place. `None` if there's none (nothing to undo / redo). Returns the
/// step's label.
pub(crate) async fn step(
    conn: &mut SqliteConnection,
    scene_id: &str,
    back: bool,
) -> Result<Option<String>, sqlx::Error> {
    let at = history_at(conn, scene_id).await?;
    let target: Option<(i64, String)> = if back {
        sqlx::query_as("SELECT seq, snapshot FROM scene_draft_history WHERE scene_id = $1 AND seq < $2 ORDER BY seq DESC LIMIT 1")
    } else {
        sqlx::query_as("SELECT seq, snapshot FROM scene_draft_history WHERE scene_id = $1 AND seq > $2 ORDER BY seq LIMIT 1")
    }
    .bind(scene_id)
    .bind(at)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((seq, snapshot)) = target else {
        return Ok(None);
    };
    // The step undone is the current one; the one redone is the next.
    let label: String = sqlx::query_scalar(
        "SELECT label FROM scene_draft_history WHERE scene_id = $1 AND seq = $2",
    )
    .bind(scene_id)
    .bind(if back { at } else { seq })
    .fetch_optional(&mut *conn)
    .await?
    .unwrap_or_default();
    restore(conn, scene_id, &snapshot).await?;
    // Another state: a submit's mark no longer applies.
    sqlx::query(
        "UPDATE scene_drafts SET history_at = $1, update_time = $2, submit_state = NULL, \
             submit_problems = NULL, submit_message = NULL, submit_time = NULL, submit_id = NULL \
         WHERE scene_id = $3",
    )
    .bind(seq)
    .bind(chrono::Local::now())
    .bind(scene_id)
    .execute(conn)
    .await?;
    Ok(Some(label))
}

/// Puts a snapshot's rows in place of the draft's.
async fn restore(
    conn: &mut SqliteConnection,
    scene_id: &str,
    snapshot: &str,
) -> Result<(), sqlx::Error> {
    // A node's edges and uploads go with it (cascade).
    sqlx::query("DELETE FROM scene_draft_edges WHERE scene_id = $1")
        .bind(scene_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM scene_draft_nodes WHERE scene_id = $1")
        .bind(scene_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE scene_drafts SET base_revision = json_extract($2, '$.draft.base_revision'), \
             \"name\" = json_extract($2, '$.draft.name'), max_hops = json_extract($2, '$.draft.max_hops'), \
             defaults = json_extract($2, '$.draft.defaults'), \"groups\" = COALESCE(json_extract($2, '$.draft.groups'), '[]'), base_hash = json_extract($2, '$.draft.base_hash'), \
             conflict = json_extract($2, '$.draft.conflict') \
         WHERE scene_id = $1",
    )
    .bind(scene_id)
    .bind(snapshot)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO scene_draft_nodes (id, \"name\", scene_id, \"type\", plugin_id, plugin_version, config, \
             create_time, update_time, base_hash, removed, conflict, ui) \
         SELECT json_extract(value, '$.id'), json_extract(value, '$.name'), $1, json_extract(value, '$.type'), \
             json_extract(value, '$.plugin_id'), json_extract(value, '$.plugin_version'), json_extract(value, '$.config'), \
             json_extract(value, '$.create_time'), json_extract(value, '$.update_time'), json_extract(value, '$.base_hash'), \
             json_extract(value, '$.removed'), json_extract(value, '$.conflict'), json_extract(value, '$.ui') \
         FROM json_each(json_extract($2, '$.nodes'))",
    )
    .bind(scene_id)
    .bind(snapshot)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO scene_draft_edges (id, \"name\", scene_id, source_node_id, target_node_id, mapping, \
             create_time, update_time, base_hash, removed, conflict, ui) \
         SELECT json_extract(value, '$.id'), json_extract(value, '$.name'), $1, json_extract(value, '$.source_node_id'), \
             json_extract(value, '$.target_node_id'), json_extract(value, '$.mapping'), json_extract(value, '$.create_time'), \
             json_extract(value, '$.update_time'), json_extract(value, '$.base_hash'), json_extract(value, '$.removed'), \
             json_extract(value, '$.conflict'), json_extract(value, '$.ui') \
         FROM json_each(json_extract($2, '$.edges'))",
    )
    .bind(scene_id)
    .bind(snapshot)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO scene_draft_node_uploads (node_id, \"name\", sha256, \"size\", create_time) \
         SELECT json_extract(value, '$.node_id'), json_extract(value, '$.name'), json_extract(value, '$.sha256'), \
             json_extract(value, '$.size'), json_extract(value, '$.create_time') \
         FROM json_each(json_extract($1, '$.uploads'))",
    )
    .bind(snapshot)
    .execute(&mut *conn)
    .await?;
    // Layout is shared with live: nodes and edges there move back too.
    sqlx::query(
        "UPDATE scene_nodes SET ui = (SELECT d.ui FROM scene_draft_nodes d WHERE d.id = scene_nodes.id) \
         WHERE scene_id = $1 AND id IN (SELECT id FROM scene_draft_nodes WHERE scene_id = $1)",
    )
    .bind(scene_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE scene_edges SET ui = (SELECT d.ui FROM scene_draft_edges d WHERE d.id = scene_edges.id) \
         WHERE scene_id = $1 AND id IN (SELECT id FROM scene_draft_edges WHERE scene_id = $1)",
    )
    .bind(scene_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// A short description of the change between two snapshots, for the undo
/// and redo buttons.
fn describe(before: &str, after: &str) -> String {
    let parse = |s: &str| serde_json::from_str::<Value>(s).unwrap_or(Value::Null);
    let (b, a) = (parse(before), parse(after));
    let rows =
        |v: &Value, key: &str| -> Vec<Value> { v[key].as_array().cloned().unwrap_or_default() };
    let by_id = |list: &[Value]| -> std::collections::BTreeMap<String, Value> {
        list.iter()
            .filter_map(|r| Some((r["id"].as_str()?.to_string(), r.clone())))
            .collect()
    };
    let node_names: std::collections::BTreeMap<String, String> = rows(&a, "nodes")
        .iter()
        .chain(rows(&b, "nodes").iter())
        .filter_map(|n| {
            let id = n["id"].as_str()?.to_string();
            let name = n["name"]
                .as_str()
                .unwrap_or_else(|| n["plugin_id"].as_str().unwrap_or("node"));
            Some((id, name.to_string()))
        })
        .collect();
    let name = |id: &str| node_names.get(id).cloned().unwrap_or_else(|| "node".into());

    // What differs, besides `update_time` and layout.
    let content = |r: &Value| {
        let mut r = r.clone();
        if let Some(o) = r.as_object_mut() {
            o.remove("update_time");
            o.remove("ui");
        }
        r
    };
    let mut parts: Vec<String> = vec![];
    let mut moved = 0;
    for (kind, key) in [("node", "nodes"), ("edge", "edges")] {
        let (old, new) = (by_id(&rows(&b, key)), by_id(&rows(&a, key)));
        let label = |id: &str, row: &Value| {
            if kind == "node" {
                name(id)
            } else {
                format!(
                    "{} → {}",
                    name(row["source_node_id"].as_str().unwrap_or_default()),
                    name(row["target_node_id"].as_str().unwrap_or_default())
                )
            }
        };
        let added: Vec<String> = new
            .iter()
            .filter(|(id, _)| !old.contains_key(*id))
            .map(|(id, r)| label(id, r))
            .collect();
        let gone: Vec<String> = old
            .iter()
            .filter(|(id, _)| !new.contains_key(*id))
            .map(|(id, r)| label(id, r))
            .collect();
        let mut removed = vec![];
        let mut restored = vec![];
        let mut changed = vec![];
        for (id, n) in &new {
            let Some(o) = old.get(id) else { continue };
            let (was, is) = (
                o["removed"].as_i64() == Some(1),
                n["removed"].as_i64() == Some(1),
            );
            if !was && is {
                removed.push(label(id, n));
            } else if was && !is {
                restored.push(label(id, n));
            } else if content(o) != content(n) {
                changed.push(label(id, n));
            } else if o["ui"] != n["ui"] {
                moved += 1;
            }
        }
        let say = |verb: &str, list: &[String], parts: &mut Vec<String>| match list {
            [] => {}
            [one] => parts.push(format!("{verb} {one}")),
            many => parts.push(format!("{verb} {} {kind}s", many.len())),
        };
        let add_verb = if kind == "edge" { "Connect" } else { "Add" };
        say(add_verb, &added, &mut parts);
        say("Remove", &[gone, removed].concat(), &mut parts);
        say("Restore", &restored, &mut parts);
        say("Edit", &changed, &mut parts);
    }
    if b["draft"] != a["draft"] {
        parts.push("Scene settings".into());
    }
    let uploads = |v: &Value| v["uploads"].to_string();
    if parts.is_empty() && uploads(&b) != uploads(&a) {
        parts.push("Uploads".into());
    }
    if parts.is_empty() && moved > 0 {
        parts.push(if moved == 1 {
            "Move".into()
        } else {
            format!("Move {moved} items")
        });
    }
    match parts.len() {
        0 => "Change".into(),
        1 | 2 => parts.join(", "),
        _ => format!("{} and more", parts[..2].join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::describe;
    use serde_json::json;

    #[test]
    fn describes_changes() {
        let node = |id: &str, removed: i64, cfg: &str, ui: Option<&str>| json!({"id": id, "name": null, "plugin_id": "ticker", "removed": removed, "config": cfg, "ui": ui, "update_time": "t"});
        let state = |nodes: Vec<serde_json::Value>| {
            json!({"draft": {}, "nodes": nodes, "edges": [], "uploads": []}).to_string()
        };
        let a = state(vec![node("a", 0, "{}", None)]);
        assert_eq!(describe(&state(vec![]), &a), "Add ticker");
        assert_eq!(
            describe(&a, &state(vec![node("a", 1, "{}", None)])),
            "Remove ticker"
        );
        assert_eq!(
            describe(&a, &state(vec![node("a", 0, "{\"x\":1}", None)])),
            "Edit ticker"
        );
        assert_eq!(
            describe(&a, &state(vec![node("a", 0, "{}", Some("{\"x\":1}"))])),
            "Move"
        );
    }
}
