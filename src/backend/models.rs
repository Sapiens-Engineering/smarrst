use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Feed {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub description: Option<String>,
    pub last_fetched: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ContentStatus {
    #[default]
    None,
    Fetching,
    Loaded,
    Failed,
}

impl ContentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Fetching => "fetching",
            Self::Loaded => "loaded",
            Self::Failed => "failed",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "fetching" => Self::Fetching,
            "loaded" => Self::Loaded,
            "failed" => Self::Failed,
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Article {
    pub id: i64,
    pub feed_id: i64,
    pub feed_title: String,
    pub title: String,
    pub url: String,
    pub author: Option<String>,
    pub summary: Option<String>,
    pub content: Option<String>,
    pub content_markdown: Option<String>,
    pub published: Option<DateTime<Utc>>,
    pub fetched_at: DateTime<Utc>,
    pub vote: i32,
    pub score: f64,
    pub content_status: ContentStatus,
    pub content_fetched_at: Option<DateTime<Utc>>,
    pub read_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Vote {
    Down = -1,
    None = 0,
    Up = 1,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    pub ollama_url: String,
    pub ollama_embed_model: String,
    pub ollama_chat_model: String,
    pub vote_weight: f32,
    pub time_half_life_hours: f32,
}
