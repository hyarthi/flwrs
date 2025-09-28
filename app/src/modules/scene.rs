use std::sync::Arc;
use tokio::sync::OnceCell;
use crate::db::main_db;
use crate::modules::scene::service::Service;

pub(crate) mod service;
pub(crate) mod api;

static SERVICE: OnceCell<Arc<Service>> = OnceCell::const_new();

pub(crate) async fn service() -> &'static Service {
    SERVICE
        .get_or_init(|| async {
            let db = main_db().await;
            Arc::new(Service::new(db))
        })
        .await
}