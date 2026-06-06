pub mod actions;
pub mod content;
pub mod db;
pub mod models;
pub mod ollama;
pub mod ranking;
pub mod refresh;
pub mod rss;
pub mod settings;
pub mod url_norm;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub http: reqwest::Client,
    pub settings: Arc<Mutex<models::Settings>>,
    /// Set while a refresh pipeline (manual or background) is in flight.
    /// The background loop and the manual button both swap-test-set on
    /// this; whichever loses the race skips its run instead of stacking
    /// a second pipeline on top of the first.
    pub refresh_running: Arc<AtomicBool>,
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
                // Reject any redirect target whose host is a loopback /
                // private / link-local IP. This is the second line of
                // defense behind `content::validate_public_url`, which
                // guards the initial request URL.
                .redirect(reqwest::redirect::Policy::custom(|attempt| {
                    if content::url_is_safe(attempt.url()) {
                        attempt.follow()
                    } else {
                        let blocked = attempt.url().to_string();
                        attempt.error(format!("blocked redirect to {blocked}"))
                    }
                }))
                // Prefer HTTP/2 via ALPN where the server supports it; fall
                // back to HTTP/1.1 otherwise. (Using `http2_prior_knowledge`
                // would break the many sites that still speak only h1.1.)
                .build()?,
            settings: Arc::new(Mutex::new(settings)),
            refresh_running: Arc::new(AtomicBool::new(false)),
        })
    }
}
