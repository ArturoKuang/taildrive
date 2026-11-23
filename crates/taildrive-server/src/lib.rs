mod api;
mod auth;
pub mod config;
mod content;
mod db;
mod drive;
pub mod error;
pub mod storage;

use std::{future::Future, net::SocketAddr, sync::Arc};

use config::Config;
use db::StartupResult;
use drive::Drive;
use storage::StorageBackend;
use taildrive_core::hash::sha256_hex;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;

#[derive(Clone)]
pub struct App {
    state: Arc<AppState>,
}

struct AppState {
    drive: Arc<Drive>,
    token_hashes: Vec<String>,
    node: String,
    web_dir: Option<std::path::PathBuf>,
}

impl App {
    pub async fn new(config: Config) -> StartupResult<Self> {
        let storage = config.build_storage().await?;
        Self::with_storage(config, storage).await
    }

    pub async fn with_storage(
        config: Config,
        storage: Arc<dyn StorageBackend>,
    ) -> StartupResult<Self> {
        let drive = Drive::new(&config, storage).await?;
        Ok(Self {
            state: Arc::new(AppState {
                drive,
                token_hashes: config
                    .api_tokens
                    .iter()
                    .filter(|token| !token.is_empty())
                    .map(sha256_hex)
                    .collect(),
                node: config.node_name,
                web_dir: config.web_dir,
            }),
        })
    }

    pub fn router(&self) -> axum::Router {
        let router = api::router(self.clone());
        match &self.state.web_dir {
            Some(dir) => router.fallback_service(ServeDir::new(dir)),
            None => router,
        }
    }

    pub async fn serve(
        &self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let result = axum::serve(
            listener,
            self.router()
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await;
        self.state.drive.drain().await;
        result
    }
}
