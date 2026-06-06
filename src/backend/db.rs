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
    canonical_url TEXT,
    title_hash TEXT,
    pub_day TEXT,
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
    if !cols.iter().any(|c| c == "canonical_url") {
        conn.execute("ALTER TABLE articles ADD COLUMN canonical_url TEXT", [])?;
        // First-time population: backfill the column, collapse any
        // existing cross-feed duplicates, then add the unique index.
        // Idempotent: re-running this branch on a partially-migrated
        // DB is fine — backfill is an UPDATE that's a no-op when the
        // column is already populated, and the index creation uses
        // IF NOT EXISTS.
        backfill_canonical_urls(conn)?;
        collapse_duplicate_canonical_urls(conn)?;
    }
    if !cols.iter().any(|c| c == "title_hash") {
        conn.execute("ALTER TABLE articles ADD COLUMN title_hash TEXT", [])?;
        conn.execute("ALTER TABLE articles ADD COLUMN pub_day TEXT", [])?;
        // Same shape as the canonical_url migration: backfill first,
        // then collapse any remaining duplicates that the URL pass
        // didn't catch (typically: aggregator posts like Lobsters +
        // HN that point at the same story via different URLs).
        backfill_title_hash_and_pub_day(conn)?;
        collapse_duplicate_title_and_day(conn)?;
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
    // Partial unique index: only enforced when canonical_url is set,
    // so articles with unparseable URLs (or future schema changes that
    // leave the column NULL) don't block the index creation.
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_articles_canonical_url
             ON articles(canonical_url) WHERE canonical_url IS NOT NULL",
        [],
    )?;
    // Second dedup axis: aggregator posts (Lobsters, HN, etc.) that
    // report on the same story have different URLs but typically the
    // same headline and publication day. Same `title_hash` + same
    // `pub_day` = same story. Partial index: empty/whitespace-only
    // titles or missing dates fall through to `NULL` and aren't
    // constrained, so we never block a legitimate article from
    // being inserted.
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_articles_title_day
             ON articles(title_hash, pub_day)
             WHERE title_hash IS NOT NULL AND pub_day IS NOT NULL",
        [],
    )?;
    Ok(())
}

/// Compute `canonical_url` for every row that doesn't have one yet.
/// Used by the first-run migration when the `canonical_url` column is
/// added to an existing database. The canonicalization is idempotent
/// and pure, so calling it on already-populated rows is a safe no-op.
fn backfill_canonical_urls(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    let mut stmt = conn.prepare("SELECT id, url FROM articles WHERE canonical_url IS NULL")?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, url) in rows {
        let canonical = crate::backend::url_norm::canonicalize(&url);
        conn.execute(
            "UPDATE articles SET canonical_url = ?1 WHERE id = ?2",
            rusqlite::params![canonical, id],
        )?;
    }
    Ok(())
}

/// Once every row has a `canonical_url`, fold any cross-feed duplicates
/// down to one. Keeps the row with the lowest `id` (oldest, so the one
/// the user has seen the longest). Votes on deleted rows are lost via
/// the FK CASCADE — acceptable for a first-pass migration; if a user
/// votes on duplicates differently there's no way to merge a single
/// +/-1 vote from each.
fn collapse_duplicate_canonical_urls(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    // Find canonical_url values with more than one row.
    let mut dup_stmt = conn.prepare(
        "SELECT canonical_url, COUNT(*) AS c, MIN(id) AS keep_id
         FROM articles
         WHERE canonical_url IS NOT NULL
         GROUP BY canonical_url
         HAVING c > 1",
    )?;
    let groups: Vec<(String, i64)> = dup_stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (canonical, keep_id) in groups {
        let deleted = conn.execute(
            "DELETE FROM articles
             WHERE canonical_url = ?1 AND id != ?2",
            rusqlite::params![canonical, keep_id],
        )?;
        if deleted > 0 {
            log::info!(
                "dedup: collapsed {deleted} duplicate(s) for canonical_url {canonical} (kept id {keep_id})"
            );
        }
    }
    Ok(())
}

/// Normalize a title into the form used by `title_hash`: trim,
/// collapse internal whitespace runs to a single space, lowercase.
/// Returns `None` for an empty/whitespace-only input so the title-based
/// dedup index can ignore it (otherwise every blank-titled article
/// would collapse to a single row).
pub fn normalize_title(title: &str) -> Option<String> {
    let normalized: String = title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// `YYYY-MM-DD` from `published`, falling back to `fetched_at` when
/// the feed didn't supply a publication timestamp. Returns `None` only
/// if both are missing (which shouldn't happen — `fetched_at` is
/// always set on insert).
fn pub_day_for(
    published: Option<chrono::DateTime<Utc>>,
    fetched_at: chrono::DateTime<Utc>,
) -> Option<String> {
    let d = published.unwrap_or(fetched_at);
    Some(d.format("%Y-%m-%d").to_string())
}

/// Backfill `title_hash` and `pub_day` for every row that doesn't
/// have them yet. Runs after the schema change adds the columns; on a
/// re-run it's a no-op (the WHERE clause skips already-populated
/// rows). Mirrors `backfill_canonical_urls` in shape.
fn backfill_title_hash_and_pub_day(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, title, published, fetched_at FROM articles
         WHERE title_hash IS NULL OR pub_day IS NULL",
    )?;
    let rows: Vec<(i64, String, Option<String>, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (id, title, published, fetched_at) in rows {
        let title_hash = normalize_title(&title);
        let pub_day = {
            let published_parsed = published
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc));
            let fetched_parsed = chrono::DateTime::parse_from_rfc3339(&fetched_at)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| chrono::Utc::now());
            pub_day_for(published_parsed, fetched_parsed)
        };
        conn.execute(
            "UPDATE articles SET title_hash = ?1, pub_day = ?2 WHERE id = ?3",
            rusqlite::params![title_hash, pub_day, id],
        )?;
    }
    Ok(())
}

/// Second-pass dedup: catch aggregator-post duplicates that share a
/// title (and publication day) but not a canonical URL. Same shape as
/// `collapse_duplicate_canonical_urls` — keeps the lowest `id` per
/// `(title_hash, pub_day)` group, deletes the rest, votes cascade.
fn collapse_duplicate_title_and_day(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    let mut dup_stmt = conn.prepare(
        "SELECT title_hash, pub_day, MIN(id) AS keep_id
         FROM articles
         WHERE title_hash IS NOT NULL AND pub_day IS NOT NULL
         GROUP BY title_hash, pub_day
         HAVING COUNT(*) > 1",
    )?;
    let groups: Vec<(String, String, i64)> = dup_stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut total_collapsed = 0usize;
    for (title_hash, pub_day, keep_id) in groups {
        let deleted = conn.execute(
            "DELETE FROM articles
             WHERE title_hash = ?1 AND pub_day = ?2 AND id != ?3",
            rusqlite::params![title_hash, pub_day, keep_id],
        )?;
        if deleted > 0 {
            total_collapsed += deleted;
        }
    }
    if total_collapsed > 0 {
        log::info!(
            "dedup: collapsed {total_collapsed} aggregator-post duplicate(s) via (title_hash, pub_day)"
        );
    }
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
    let now_dt = Utc::now();
    let now = now_dt.to_rfc3339();
    let published_str = published.map(|d| d.to_rfc3339());
    let canonical_url = crate::backend::url_norm::canonicalize(url);
    let title_hash = normalize_title(title);
    let pub_day = pub_day_for(published, now_dt);
    let changed = conn.execute(
        "INSERT OR IGNORE INTO articles
            (feed_id, guid, title, url, author, summary, content, content_markdown,
             published, fetched_at, canonical_url, title_hash, pub_day)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            feed_id,
            guid,
            title,
            url,
            author,
            summary,
            content,
            content_markdown,
            published_str,
            now,
            canonical_url,
            title_hash,
            pub_day
        ],
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
    let canonical_url: Option<String> = row.get(17)?;
    let title_hash: Option<String> = row.get(18)?;
    let pub_day: Option<String> = row.get(19)?;
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
        canonical_url,
        title_hash,
        pub_day,
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
                a.category,
                a.canonical_url,
                a.title_hash,
                a.pub_day
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
                a.category,
                a.canonical_url,
                a.title_hash,
                a.pub_day
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
                a.category,
                a.canonical_url,
                a.title_hash,
                a.pub_day
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
                a.category,
                a.canonical_url,
                a.title_hash,
                a.pub_day
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
