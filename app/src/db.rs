use flwrs_core::db::migrations::MigrationDef;
use flwrs_core::db::{Database, DbError, build_db};
use rust_embed::Embed;
use std::sync::Arc;
use tokio::sync::OnceCell;

#[derive(Embed)]
#[folder = "resources/migrations/sqlite/main/"]
struct MigrationDefs;

static DB: OnceCell<Arc<Database>> = OnceCell::const_new();

pub(crate) async fn main_db() -> &'static Database {
    DB.get_or_init(|| async {
        let db = match build_main_db().await {
            Ok(d) => d,
            Err(e) => {
                log::error!("Error: failed to initialise main DB: {e}");
                std::process::exit(1);
            }
        };
        Arc::new(db)
    })
    .await
}

async fn build_main_db() -> Result<Database, DbError> {
    let (ups, downs) = migrations()?;
    build_db("main", ups, downs, None).await
}

/// The embedded up and down migrations of the main DB.
fn migrations() -> Result<(Vec<MigrationDef>, Vec<MigrationDef>), DbError> {
    let load = |suffix: &str| -> Result<Vec<MigrationDef>, DbError> {
        MigrationDefs::iter()
            .filter(|file_name| file_name.ends_with(suffix))
            .map(|file_name| {
                let file = MigrationDefs::get(file_name.as_ref()).expect("embedded file listed");
                MigrationDef::new(file_name.to_string(), file)
            })
            .collect()
    };
    Ok((load(".up.sql")?, load(".down.sql")?))
}

/// A fully migrated main DB in a fresh file, for tests. Leaked so it can be
/// handed to services that expect `&'static Database`.
#[cfg(test)]
pub(crate) async fn test_db(path: &std::path::Path) -> &'static Database {
    let (ups, downs) = migrations().unwrap();
    let db = flwrs_core::db::build_db_at("main", &path.to_string_lossy(), ups, downs, None)
        .await
        .unwrap();
    Box::leak(Box::new(db))
}
