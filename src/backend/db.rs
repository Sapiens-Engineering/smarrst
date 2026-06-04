use crate::backend::models::{Article, ContentStatus, Feed, Vote};
use chrono::Utc;
use rusqlite::{params, OptionalExtension, Row};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS feeds (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    url TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    description TEXT,
    last_fetched TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS articles (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    feed_id INTEGER NOT NULL,
    guid TEXT NOT NULL,
    title TEXT NOT NULL,
    url TEXT NOT NULL,
    author TEXT,
    summary TEXT,
    content TEXT,
    content_markdown TEXT,
    published TEXT,
    fetched_at TEXT NOT NULL,
    embedding TEXT,
    content_status TEXT NOT NULL DEFAULT 'none',
    content_fetched_at TEXT,
    read_at TEXT,
    category TEXT,
    UNIQUE(feed_id, guid),
    FOREIGN KEY(feed_id) REFERENCES feeds(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_articles_feed ON articles(feed_id);
CREATE INDEX IF NOT EXISTS idx_articles_published ON articles(published);

CREATE TABLE IF NOT EXISTS votes (
    article_id INTEGER PRIMARY KEY,
    vote INTEGER NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(article_id) REFERENCES articles(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

pub fn init_schema(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    conn.execute_batch(SCHEMA)?;
    migrate_articles(conn)?;
    Ok(())
}

/// Add columns introduced in later versions. Idempotent.
fn migrate_articles(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    let cols: Vec<String> = conn
        .prepare("PRAGMA table_info(articles)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !cols.iter().any(|c| c == "content_status") {
        conn.execute(
            "ALTER TABLE articles ADD COLUMN content_status TEXT NOT NULL DEFAULT 'none'",
            [],
        )?;
    }
    if !cols.iter().any(|c| c == "content_fetched_at") {
        conn.execute(
            "ALTER TABLE articles ADD COLUMN content_fetched_at TEXT",
            [],
        )?;
    }
    if !cols.iter().any(|c| c == "read_at") {
        conn.execute("ALTER TABLE articles ADD COLUMN read_at TEXT", [])?;
    }
    if !cols.iter().any(|c| c == "content_markdown") {
        conn.execute("ALTER TABLE articles ADD COLUMN content_markdown TEXT", [])?;
    }
    if !cols.iter().any(|c| c == "category") {
        conn.execute("ALTER TABLE articles ADD COLUMN category TEXT", [])?;
    }
    // The index on the (post-migration) column is safe to (re)create now.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_articles_content_status ON articles(content_status)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_articles_category ON articles(category)",
        [],
    )?;
    Ok(())
}

fn row_to_feed(row: &Row<'_>) -> rusqlite::Result<Feed> {
    let last_fetched: Option<String> = row.get(4)?;
    let last_fetched = last_fetched
        .map(|s| chrono::DateTime::parse_from_rfc3339(&s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::InvalidColumnType(4, e.to_string(), rusqlite::types::Type::Text)
        })?;
    let created_at: String = row.get(5)?;
    let created_at = chrono::DateTime::parse_from_rfc3339(&created_at)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::InvalidColumnType(5, e.to_string(), rusqlite::types::Type::Text)
        })?;
    Ok(Feed {
        id: row.get(0)?,
        url: row.get(1)?,
        title: row.get(2)?,
        description: row.get(3)?,
        last_fetched,
        created_at,
    })
}

pub fn add_feed(
    conn: &rusqlite::Connection,
    url: &str,
    title: &str,
    description: Option<&str>,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO feeds (url, title, description, created_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(url) DO UPDATE SET title = excluded.title, description = excluded.description",
        params![url, title, description, now],
    )?;
    let id: i64 = conn.query_row("SELECT id FROM feeds WHERE url = ?1", params![url], |row| {
        row.get(0)
    })?;
    Ok(id)
}

pub fn list_feeds(conn: &rusqlite::Connection) -> anyhow::Result<Vec<Feed>> {
    let mut stmt = conn.prepare(
        "SELECT id, url, title, description, last_fetched, created_at FROM feeds ORDER BY title",
    )?;
    let iter = stmt.query_map([], row_to_feed)?;
    let mut feeds = Vec::new();
    for f in iter {
        feeds.push(f?);
    }
    Ok(feeds)
}

pub fn get_feed(conn: &rusqlite::Connection, id: i64) -> anyhow::Result<Option<Feed>> {
    Ok(conn
        .query_row(
            "SELECT id, url, title, description, last_fetched, created_at FROM feeds WHERE id = ?1",
            params![id],
            row_to_feed,
        )
        .optional()?)
}

pub fn delete_feed(conn: &rusqlite::Connection, id: i64) -> anyhow::Result<()> {
    conn.execute("DELETE FROM feeds WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn mark_feed_fetched(conn: &rusqlite::Connection, id: i64) -> anyhow::Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE feeds SET last_fetched = ?1 WHERE id = ?2",
        params![now, id],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn insert_article(
    conn: &rusqlite::Connection,
    feed_id: i64,
    guid: &str,
    title: &str,
    url: &str,
    author: Option<&str>,
    summary: Option<&str>,
    content: Option<&str>,
    content_markdown: Option<&str>,
    published: Option<chrono::DateTime<Utc>>,
) -> anyhow::Result<Option<i64>> {
    let now = Utc::now().to_rfc3339();
    let published = published.map(|d| d.to_rfc3339());
    let changed = conn.execute(
        "INSERT OR IGNORE INTO articles
            (feed_id, guid, title, url, author, summary, content, content_markdown, published, fetched_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![feed_id, guid, title, url, author, summary, content, content_markdown, published, now],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    let id: i64 = conn.query_row(
        "SELECT id FROM articles WHERE feed_id = ?1 AND guid = ?2",
        params![feed_id, guid],
        |row| row.get(0),
    )?;
    Ok(Some(id))
}

fn row_to_article(row: &Row<'_>) -> rusqlite::Result<Article> {
    let published: Option<String> = row.get(8)?;
    let published = published
        .map(|s| chrono::DateTime::parse_from_rfc3339(&s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::InvalidColumnType(8, e.to_string(), rusqlite::types::Type::Text)
        })?;
    let fetched_at: String = row.get(9)?;
    let fetched_at = chrono::DateTime::parse_from_rfc3339(&fetched_at)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::InvalidColumnType(9, e.to_string(), rusqlite::types::Type::Text)
        })?;
    let content_status: String = row.get(12)?;
    let content_fetched_at: Option<String> = row.get(13)?;
    let content_fetched_at = content_fetched_at
        .map(|s| chrono::DateTime::parse_from_rfc3339(&s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::InvalidColumnType(13, e.to_string(), rusqlite::types::Type::Text)
        })?;
    let read_at: Option<String> = row.get(14)?;
    let read_at = read_at
        .map(|s| chrono::DateTime::parse_from_rfc3339(&s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::InvalidColumnType(14, e.to_string(), rusqlite::types::Type::Text)
        })?;
    let category: Option<String> = row.get(16)?;
    Ok(Article {
        id: row.get(0)?,
        feed_id: row.get(1)?,
        feed_title: row.get(2)?,
        title: row.get(3)?,
        url: row.get(4)?,
        author: row.get(5)?,
        summary: row.get(6)?,
        content: row.get(7)?,
        content_markdown: row.get(15)?,
        published,
        fetched_at,
        vote: row.get(10)?,
        score: row.get(11)?,
        display_score: None,
        content_status: ContentStatus::from_db(&content_status),
        content_fetched_at,
        read_at,
        category,
    })
}

pub fn get_article(conn: &rusqlite::Connection, id: i64) -> anyhow::Result<Option<Article>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.feed_id, f.title, a.title, a.url, a.author, a.summary, a.content,
                a.published, a.fetched_at,
                COALESCE(v.vote, 0) AS vote,
                0.0 AS score,
                a.content_status,
                a.content_fetched_at,
                a.read_at,
                a.content_markdown,
                a.category
         FROM articles a
         JOIN feeds f ON f.id = a.feed_id
         LEFT JOIN votes v ON v.article_id = a.id
         WHERE a.id = ?1",
    )?;
    Ok(stmt.query_row(params![id], row_to_article).optional()?)
}

pub fn set_vote(conn: &rusqlite::Connection, article_id: i64, vote: Vote) -> anyhow::Result<()> {
    let now = Utc::now().to_rfc3339();
    let v = vote as i32;
    conn.execute(
        "INSERT INTO votes (article_id, vote, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(article_id) DO UPDATE SET vote = excluded.vote, updated_at = excluded.updated_at",
        params![article_id, v, now],
    )?;
    Ok(())
}

pub fn get_vote(conn: &rusqlite::Connection, article_id: i64) -> anyhow::Result<Vote> {
    let raw: Option<i32> = conn
        .query_row(
            "SELECT vote FROM votes WHERE article_id = ?1",
            params![article_id],
            |r| r.get(0),
        )
        .ok();
    Ok(match raw {
        Some(1) => Vote::Up,
        Some(-1) => Vote::Down,
        _ => Vote::None,
    })
}

pub fn articles_missing_scores(
    conn: &rusqlite::Connection,
    limit: i64,
) -> anyhow::Result<Vec<Article>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.feed_id, f.title, a.title, a.url, a.author, a.summary, a.content,
                a.published, a.fetched_at,
                COALESCE(v.vote, 0) AS vote,
                0.0 AS score,
                a.content_status,
                a.content_fetched_at,
                a.read_at,
                a.content_markdown,
                a.category
         FROM articles a
         JOIN feeds f ON f.id = a.feed_id
         LEFT JOIN votes v ON v.article_id = a.id
         WHERE a.embedding IS NULL
           AND (a.content IS NOT NULL OR a.summary IS NOT NULL)
         ORDER BY a.id DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], row_to_article)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn articles_pending_content(
    conn: &rusqlite::Connection,
    limit: i64,
) -> anyhow::Result<Vec<Article>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.feed_id, f.title, a.title, a.url, a.author, a.summary, a.content,
                a.published, a.fetched_at,
                COALESCE(v.vote, 0) AS vote,
                0.0 AS score,
                a.content_status,
                a.content_fetched_at,
                a.read_at,
                a.content_markdown,
                a.category
         FROM articles a
         JOIN feeds f ON f.id = a.feed_id
         LEFT JOIN votes v ON v.article_id = a.id
         WHERE a.content_status IN ('none', 'failed')
         ORDER BY a.id DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], row_to_article)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn mark_content_status(
    conn: &rusqlite::Connection,
    article_id: i64,
    status: ContentStatus,
) -> anyhow::Result<()> {
    let now = Utc::now().to_rfc3339();
    let s = status.as_str();
    if matches!(status, ContentStatus::Loaded) {
        conn.execute(
            "UPDATE articles SET content_status = ?1, content_fetched_at = ?2 WHERE id = ?3",
            params![s, now, article_id],
        )?;
    } else {
        conn.execute(
            "UPDATE articles SET content_status = ?1 WHERE id = ?2",
            params![s, article_id],
        )?;
    }
    Ok(())
}

pub fn store_extracted_content(
    conn: &rusqlite::Connection,
    article_id: i64,
    extracted_text: &str,
    extracted_markdown: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE articles SET content = ?1, content_markdown = ?2,
                content_status = 'loaded', content_fetched_at = ?3
         WHERE id = ?4",
        params![
            extracted_text,
            extracted_markdown,
            Utc::now().to_rfc3339(),
            article_id
        ],
    )?;
    Ok(())
}

pub fn count_votes(conn: &rusqlite::Connection) -> anyhow::Result<(i64, i64)> {
    let up: i64 = conn.query_row("SELECT COUNT(*) FROM votes WHERE vote = 1", [], |r| {
        r.get(0)
    })?;
    let down: i64 = conn.query_row("SELECT COUNT(*) FROM votes WHERE vote = -1", [], |r| {
        r.get(0)
    })?;
    Ok((up, down))
}

pub fn set_article_read(conn: &rusqlite::Connection, id: i64) -> anyhow::Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE articles SET read_at = ?1 WHERE id = ?2",
        params![now, id],
    )?;
    Ok(())
}

pub fn set_article_unread(conn: &rusqlite::Connection, id: i64) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE articles SET read_at = NULL WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

pub fn set_article_category(
    conn: &rusqlite::Connection,
    id: i64,
    category: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE articles SET category = ?1 WHERE id = ?2",
        params![category, id],
    )?;
    Ok(())
}

pub fn articles_pending_classification(
    conn: &rusqlite::Connection,
    limit: i64,
) -> anyhow::Result<Vec<Article>> {
    // Includes articles whose content fetch failed: classification only
    // needs a title (and optionally a summary) to pick a label, so
    // gating on `content_status != 'failed'` would leave those articles
    // stuck with `category = NULL` forever. The Rust-side filter in
    // `classify_pending_with_concurrency` already drops inputs whose
    // combined title+summary+content is empty.
    let mut stmt = conn.prepare(
        "SELECT a.id, a.feed_id, f.title, a.title, a.url, a.author, a.summary, a.content,
                a.published, a.fetched_at,
                COALESCE(v.vote, 0) AS vote,
                0.0 AS score,
                a.content_status,
                a.content_fetched_at,
                a.read_at,
                a.content_markdown,
                a.category
         FROM articles a
         JOIN feeds f ON f.id = a.feed_id
         LEFT JOIN votes v ON v.article_id = a.id
         WHERE a.category IS NULL
           AND (a.content IS NOT NULL OR a.summary IS NOT NULL OR a.title != '')
         ORDER BY a.id DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], row_to_article)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn count_votes_for_category(
    conn: &rusqlite::Connection,
    category: &str,
) -> anyhow::Result<(i64, i64)> {
    let up: i64 = conn.query_row(
        "SELECT COUNT(*) FROM votes v
         JOIN articles a ON v.article_id = a.id
         WHERE v.vote = 1 AND a.category = ?1",
        params![category],
        |r| r.get(0),
    )?;
    let down: i64 = conn.query_row(
        "SELECT COUNT(*) FROM votes v
         JOIN articles a ON v.article_id = a.id
         WHERE v.vote = -1 AND a.category = ?1",
        params![category],
        |r| r.get(0),
    )?;
    Ok((up, down))
}

/// Per-category counts: every category that has at least one article,
/// with the unread count and the total count. Categories with zero
/// unread (i.e. all articles marked read) still appear so the user can
/// see the full landscape and click through to review them.
pub fn category_counts(conn: &rusqlite::Connection) -> anyhow::Result<Vec<(String, i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT a.category,
                SUM(CASE WHEN a.read_at IS NULL THEN 1 ELSE 0 END) AS unread,
                COUNT(*) AS total
         FROM articles a
         WHERE a.category IS NOT NULL
         GROUP BY a.category
         ORDER BY a.category",
    )?;
    let rows = stmt.query_map([], |r| {
        let c: Option<String> = r.get(0)?;
        let unread: i64 = r.get(1)?;
        let total: i64 = r.get(2)?;
        Ok((c.unwrap_or_default(), unread, total))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
