pub mod actions;
pub mod content;
pub mod db;
pub mod models;
pub mod ollama;
pub mod ranking;
pub mod rss;
pub mod settings;

use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub http: reqwest::Client,
    pub settings: Arc<Mutex<models::Settings>>,
}

pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

impl AppState {
    pub fn new(data_dir: &std::path::Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let db_path = data_dir.join("smarrst.db");
        let conn = rusqlite::Connection::open(&db_path)?;
        db::init_schema(&conn)?;
        let settings = settings::load(&conn)?;
        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
            http: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(std::time::Duration::from_secs(30))
                // Prefer HTTP/2 via ALPN where the server supports it; fall
                // back to HTTP/1.1 otherwise. (Using `http2_prior_knowledge`
                // would break the many sites that still speak only h1.1.)
                .build()?,
            settings: Arc::new(Mutex::new(settings)),
        })
    }
}
