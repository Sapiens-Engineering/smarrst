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
    /// 0..=10 percentile within the current ranked list (10 = top of
    /// the list, 0 = bottom). `None` when the article was loaded
    /// outside of `ranked_articles` (e.g. direct `get_article` call).
    pub display_score: Option<f32>,
    pub content_status: ContentStatus,
    pub content_fetched_at: Option<DateTime<Utc>>,
    pub read_at: Option<DateTime<Utc>>,
    pub category: Option<String>,
    /// Normalized form of `url` (lowercase scheme/host, no `www.`,
    /// tracking params dropped, query sorted, fragment stripped,
    /// trailing slash stripped). Two articles from different feeds
    /// that point to the same page share a `canonical_url`, which is
    /// what the cross-feed unique index in `db.rs` enforces. `None`
    /// when the original URL couldn't be parsed.
    pub canonical_url: Option<String>,
    /// Normalized title (trim, lowercase, whitespace collapsed). Used
    /// together with `pub_day` to dedup aggregator posts that report
    /// on the same story (Lobsters + HN pointing at the same blog
    /// post): same title + same publication day = same story. `None`
    /// for articles with an empty/whitespace-only title.
    pub title_hash: Option<String>,
    /// Publication date in `YYYY-MM-DD` form, from `published` (or
    /// `fetched_at` if `published` is missing). Paired with
    /// `title_hash` to scope title-based dedup to a single day.
    pub pub_day: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Vote {
    Down = -1,
    None = 0,
    Up = 1,
}

pub const DEFAULT_CATEGORIES: &[&str] = &[
    "AI",
    "Cryptography",
    "Philosophy",
    "Psychology",
    "Tech",
    "Politics",
    "Science",
    "Business",
    "Culture",
    "Sports",
    "Gaming",
    "Other",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub ollama_url: String,
    pub ollama_embed_model: String,
    pub ollama_chat_model: String,
    pub vote_weight: f32,
    pub time_half_life_hours: f32,
    pub category_labels: Vec<String>,
    pub category_weight: f32,
    /// Interval for the background refresh loop, in minutes. `0` disables
    /// the loop. The setting is re-read on every tick, so changes from
    /// the Settings dialog take effect on the next tick (no restart).
    pub background_refresh_minutes: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ollama_url: "http://localhost:11434".to_string(),
            ollama_embed_model: "nomic-embed-text".to_string(),
            ollama_chat_model: "llama3.2".to_string(),
            vote_weight: 1.0,
            time_half_life_hours: 168.0,
            category_labels: DEFAULT_CATEGORIES.iter().map(|s| s.to_string()).collect(),
            category_weight: 1.0,
            background_refresh_minutes: 15,
        }
    }
}
