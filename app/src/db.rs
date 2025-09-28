use flwrs_core::db::migrations::MigrationDef;
use flwrs_core::db::{build_db, Database, DbError};
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
    let mut ups = vec![];
    for file_name in MigrationDefs::iter().filter(|file_name| file_name.ends_with(".up.sql")) {
        ups.push(MigrationDef::new(
            file_name.to_string(),
            MigrationDefs::get(file_name.as_ref()).unwrap(),
        )?);
    }
    let mut downs = vec![];
    for file_name in MigrationDefs::iter().filter(|file_name| file_name.ends_with(".down.sql")) {
        downs.push(MigrationDef::new(
            file_name.to_string(),
            MigrationDefs::get(file_name.as_ref()).unwrap(),
        )?);
    }

    build_db("main", ups, downs, None).await
}
