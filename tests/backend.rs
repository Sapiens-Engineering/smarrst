//! Integration tests for the smarrst backend. Exercise database, settings and
//! ranking logic against an in-memory SQLite store; do not require Ollama.

use smarrst::backend::{content, db, models, ollama, ranking, refresh, settings};

fn open_memory() -> rusqlite::Connection {
    rusqlite::Connection::open_in_memory().expect("open in-memory db")
}

#[test]
fn schema_initializes_cleanly() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init schema");
    // Re-initializing should be a no-op.
    db::init_schema(&conn).expect("re-init schema");
}

/// Simulate a database that was created with the pre-category schema and
/// then upgraded. The initial SCHEMA must not reference the `category`
/// column, or this fails with "no such column: category".
#[test]
fn schema_upgrades_legacy_articles_table() {
    let conn = open_memory();
    // The pre-category schema: no `category` column, no
    // `idx_articles_category` index.
    conn.execute_batch(
        "CREATE TABLE articles (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            feed_id INTEGER NOT NULL,
            guid TEXT NOT NULL,
            title TEXT NOT NULL,
            url TEXT NOT NULL,
            author TEXT,
            summary TEXT,
            content TEXT,
            published TEXT,
            fetched_at TEXT NOT NULL,
            embedding TEXT,
            UNIQUE(feed_id, guid)
        );
        CREATE TABLE feeds (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            url TEXT NOT NULL UNIQUE,
            title TEXT NOT NULL,
            description TEXT,
            last_fetched TEXT,
            created_at TEXT NOT NULL
        );
        CREATE TABLE votes (
            article_id INTEGER PRIMARY KEY,
            vote INTEGER NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE INDEX idx_articles_feed ON articles(feed_id);
        CREATE INDEX idx_articles_published ON articles(published);",
    )
    .expect("create legacy schema");

    // Now run the real init: SCHEMA batch + migration. Should add the
    // category column, content_status, read_at, content_markdown, and
    // create the post-migration indexes.
    db::init_schema(&conn).expect("upgrade legacy schema");

    // Category column exists and is nullable.
    let cols: Vec<String> = conn
        .prepare("PRAGMA table_info(articles)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(cols.iter().any(|c| c == "category"));
    assert!(cols.iter().any(|c| c == "content_status"));
    assert!(cols.iter().any(|c| c == "read_at"));
    assert!(cols.iter().any(|c| c == "content_markdown"));
    assert!(cols.iter().any(|c| c == "canonical_url"));

    // Index on category exists.
    let idx_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='index' AND name='idx_articles_category'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx_count, 1, "idx_articles_category should exist");

    // Cross-feed dedup index exists (created after the backfill +
    // collapse-duplicates migration step).
    let canonical_idx: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='index' AND name='idx_articles_canonical_url'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(canonical_idx, 1, "idx_articles_canonical_url should exist");
}

/// A database created before the `canonical_url` feature, with two
/// feeds pointing at the same article, must be deduped on first launch
/// after the upgrade. The migration keeps the oldest row (lowest id)
/// and deletes the rest. New inserts after the migration are blocked
/// by the unique partial index.
#[test]
fn migration_collapses_existing_cross_feed_duplicates() {
    let conn = open_memory();

    // Pre-canonical_url schema: identical to the current SCHEMA minus
    // the canonical_url column, the unique partial index, and the
    // post-migration category column (so the migration path is the
    // one being exercised).
    conn.execute_batch(
        "CREATE TABLE feeds (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            url TEXT NOT NULL UNIQUE,
            title TEXT NOT NULL,
            description TEXT,
            last_fetched TEXT,
            created_at TEXT NOT NULL
         );
         CREATE TABLE articles (
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
            UNIQUE(feed_id, guid),
            FOREIGN KEY(feed_id) REFERENCES feeds(id) ON DELETE CASCADE
         );
         CREATE TABLE votes (
            article_id INTEGER PRIMARY KEY,
            vote INTEGER NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY(article_id) REFERENCES articles(id) ON DELETE CASCADE
         );
         CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE INDEX idx_articles_feed ON articles(feed_id);
         CREATE INDEX idx_articles_published ON articles(published);",
    )
    .expect("create pre-canonical schema");

    let feed_a = db::add_feed(&conn, "https://hn-a.example/rss", "A", None).unwrap();
    let feed_b = db::add_feed(&conn, "https://hn-b.example/rss", "B", None).unwrap();

    // Two rows that should collapse to one. The lower id wins; the
    // higher id is deleted.
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO articles
            (feed_id, guid, title, url, fetched_at)
         VALUES (?1, 'g1', 'Show HN: foo', 'https://example.com/p?utm_source=a', ?2)",
        rusqlite::params![feed_a, now],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO articles
            (feed_id, guid, title, url, fetched_at)
         VALUES (?1, 'g2', 'Show HN: foo', 'HTTPS://www.example.com/p/', ?2)",
        rusqlite::params![feed_b, now],
    )
    .unwrap();
    // An unrelated article that should NOT be touched.
    conn.execute(
        "INSERT INTO articles
            (feed_id, guid, title, url, fetched_at)
         VALUES (?1, 'g3', 'Other', 'https://other.example/q', ?2)",
        rusqlite::params![feed_a, now],
    )
    .unwrap();

    // Run the upgrade migration.
    db::init_schema(&conn).expect("upgrade");

    // Two rows total: the unrelated one and the deduped one. The deduped
    // row is the one originally inserted into feed_a (id 1, the
    // lowest), and its original URL is preserved.
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM articles", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2, "duplicates should be collapsed during migration");

    let kept: (i64, String) = conn
        .query_row(
            "SELECT id, url FROM articles WHERE title = 'Show HN: foo'",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .unwrap();
    assert_eq!(kept.0, 1, "lowest id wins; the older feed_a row is kept");
    assert_eq!(
        kept.1, "https://example.com/p?utm_source=a",
        "original URL is preserved (not the canonical form)"
    );

    // canonical_url was backfilled on both surviving rows.
    let canonical: Option<String> = conn
        .query_row("SELECT canonical_url FROM articles WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        canonical.as_deref(),
        Some("https://example.com/p"),
        "tracking param dropped, www. stripped, path trailing slash removed, scheme lowercased"
    );

    // And the unique partial index blocks a re-introduction: trying to
    // re-insert via the public API with a URL that canonicalizes to the
    // same form must be rejected.
    let dup = db::insert_article(
        &conn,
        feed_b,
        "g4",
        "Re-introduced",
        "https://EXAMPLE.com/p",
        None,
        None,
        None,
        None,
        None,
    )
    .expect("insert call");
    assert!(
        dup.is_none(),
        "should be blocked by canonical_url unique index"
    );
}

/// A pre-feature database with two aggregator-post duplicates (Lobsters
/// and HN pointing at the same story, different URLs) must be deduped
/// on the first launch after the title+date migration. Runs after the
/// canonical-URL dedup has already removed any URL-level duplicates.
#[test]
fn migration_collapses_existing_aggregator_post_duplicates() {
    let conn = open_memory();

    // Pre-canonical-url AND pre-title-dedup schema. Both migrations
    // should fire in order: canonical_url first (no-op here because
    // the URLs are all different), then title+day.
    conn.execute_batch(
        "CREATE TABLE feeds (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            url TEXT NOT NULL UNIQUE,
            title TEXT NOT NULL,
            description TEXT,
            last_fetched TEXT,
            created_at TEXT NOT NULL
         );
         CREATE TABLE articles (
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
         CREATE TABLE votes (
            article_id INTEGER PRIMARY KEY,
            vote INTEGER NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY(article_id) REFERENCES articles(id) ON DELETE CASCADE
         );
         CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE INDEX idx_articles_feed ON articles(feed_id);
         CREATE INDEX idx_articles_published ON articles(published);",
    )
    .expect("create pre-migration schema");

    let lobsters = db::add_feed(&conn, "https://lobste.rs/rss", "Lobsters", None).unwrap();
    let hn = db::add_feed(&conn, "https://hn.example/rss", "HN", None).unwrap();

    // Same story, two feeds, two completely different URLs, two
    // different GUIDs, two different authors, four hours apart in
    // publication time but same calendar day.
    let day = chrono::DateTime::parse_from_rfc3339("2026-06-03T08:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let later = day + chrono::Duration::hours(4);
    let day_str = day.format("%Y-%m-%d").to_string();
    let later_str = later.to_rfc3339();

    // Lobsters post, inserted first (lowest id) — wins the keep.
    conn.execute(
        "INSERT INTO articles (feed_id, guid, title, url, author, published, fetched_at)
         VALUES (?1, 'g1', 'A Post-Quantum Future for Let''s Encrypt',
                 'https://lobste.rs/s/abc', 'alice', ?2, ?3)",
        rusqlite::params![lobsters, day.to_rfc3339(), day.to_rfc3339()],
    )
    .unwrap();
    // HN post about the same story, inserted second.
    conn.execute(
        "INSERT INTO articles (feed_id, guid, title, url, author, published, fetched_at)
         VALUES (?1, 'g2', 'A Post-Quantum Future for Let''s Encrypt',
                 'https://news.ycombinator.com/item?id=99999', 'SGran', ?2, ?3)",
        rusqlite::params![hn, later_str, later_str],
    )
    .unwrap();
    // Unrelated article, must be untouched.
    conn.execute(
        "INSERT INTO articles (feed_id, guid, title, url, fetched_at)
         VALUES (?1, 'g3', 'Something else entirely', 'https://other.example/q', ?2)",
        rusqlite::params![lobsters, day.to_rfc3339()],
    )
    .unwrap();
    // Same story *on a different day* — must NOT be collapsed (e.g. a
    // syndicated republication the next week).
    let next_week = day + chrono::Duration::days(7);
    conn.execute(
        "INSERT INTO articles (feed_id, guid, title, url, fetched_at)
         VALUES (?1, 'g4', 'A Post-Quantum Future for Let''s Encrypt',
                 'https://example.com/repub', ?2)",
        rusqlite::params![hn, next_week.to_rfc3339()],
    )
    .unwrap();

    // Run the full upgrade migration. This must add canonical_url
    // and title_hash/pub_day columns, backfill them, collapse the
    // aggregator-post duplicate, and create both unique indexes.
    db::init_schema(&conn).expect("upgrade");

    // Expect: unrelated article + the kept (Lobsters) post + the
    // next-week republication = 3 rows. The HN post was deduped.
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM articles", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 3,
        "aggregator-post duplicate should be collapsed; next-week republication kept"
    );

    // The kept row is the Lobsters post (lowest id, oldest insert).
    let kept: (i64, String) = conn
        .query_row(
            "SELECT id, url FROM articles
             WHERE title = 'A Post-Quantum Future for Let''s Encrypt'
               AND pub_day = ?1",
            rusqlite::params![day_str],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .unwrap();
    assert_eq!(kept.0, 1, "lowest-id row wins");
    assert_eq!(
        kept.1, "https://lobste.rs/s/abc",
        "original URL preserved, not the canonical form"
    );

    // The next-week republication survives (different pub_day).
    let next: Option<String> = conn
        .query_row(
            "SELECT url FROM articles WHERE pub_day = '2026-06-10'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(next.as_deref(), Some("https://example.com/repub"));

    // The unique partial index on (title_hash, pub_day) is in place.
    let idx: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='index' AND name='idx_articles_title_day'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1, "idx_articles_title_day should exist");
}

#[test]
fn add_and_list_feeds() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");

    db::add_feed(&conn, "https://a.example/feed", "Alpha", Some("first")).expect("add alpha");
    db::add_feed(&conn, "https://b.example/feed", "Beta", None).expect("add beta");

    let feeds = db::list_feeds(&conn).expect("list");
    assert_eq!(feeds.len(), 2);
    let titles: Vec<&str> = feeds.iter().map(|f| f.title.as_str()).collect();
    assert!(titles.contains(&"Alpha"));
    assert!(titles.contains(&"Beta"));
}

#[test]
fn insert_article_dedupes_by_guid() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");

    let feed_id = db::add_feed(&conn, "https://a.example/feed", "A", None).unwrap();
    let first = db::insert_article(
        &conn,
        feed_id,
        "guid-1",
        "T1",
        "https://a.example/1",
        None,
        None,
        Some("body"),
        None,
        None,
    )
    .expect("insert");
    assert!(first.is_some());

    let second = db::insert_article(
        &conn,
        feed_id,
        "guid-1",
        "T1 (dup)",
        "https://a.example/1",
        None,
        None,
        Some("body"),
        None,
        None,
    )
    .expect("insert");
    assert!(second.is_none(), "duplicate guid should be ignored");
}

/// Two feeds pointing at the same article (e.g. two HN mirrors using
/// different GUIDs) should collapse to a single row, keyed on the
/// canonical form of the URL. Tracking params, scheme case, and
/// `www.` prefix must not produce duplicates.
#[test]
fn insert_article_dedupes_across_feeds_via_canonical_url() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");

    let feed_a = db::add_feed(&conn, "https://hn-a.example/rss", "HN A", None).unwrap();
    let feed_b = db::add_feed(&conn, "https://hn-b.example/rss", "HN B", None).unwrap();

    let id_a = db::insert_article(
        &conn,
        feed_a,
        "guid-aaa",
        "Show HN: foo",
        "https://news.ycombinator.com/item?id=123",
        None,
        Some("from feed A"),
        None,
        None,
        None,
    )
    .expect("insert A")
    .expect("A inserted");

    // Same article from feed B, but the URL has a different scheme case,
    // a `www.` prefix, and a tracking param. The canonical form should
    // match feed A's row, so the insert is ignored.
    let id_b = db::insert_article(
        &conn,
        feed_b,
        "guid-bbb",
        "Show HN: foo",
        "HTTPS://www.news.ycombinator.com/item?id=123&utm_source=hn-b",
        None,
        Some("from feed B"),
        None,
        None,
        None,
    )
    .expect("insert B call");

    // B's row should have collapsed into A's via the canonical_url
    // unique index, so the call returns Ok(None). The first insert
    // wins; the row is attributed to feed A and the original URL is
    // preserved (not the canonical form), so the "Open in browser"
    // link still works with whatever tracking params the publisher
    // embedded.
    assert!(id_b.is_none(), "B's row should be deduped by canonical_url");
    let kept = db::get_article(&conn, id_a).unwrap().expect("article");
    assert_eq!(kept.feed_id, feed_a);
    assert_eq!(kept.url, "https://news.ycombinator.com/item?id=123");
    assert_eq!(
        kept.canonical_url.as_deref(),
        Some("https://news.ycombinator.com/item?id=123"),
        "canonical_url is the lowercased/normalized form"
    );
}

/// Two aggregator feeds (e.g. Lobsters + HN) reporting on the same
/// underlying story have different URLs and different GUIDs, so the
/// `canonical_url` index cannot dedup them. The `(title_hash, pub_day)`
/// index catches the case: same headline + same publication day =
/// same story.
#[test]
fn insert_article_dedupes_aggregator_posts_via_title_and_day() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");

    let lobsters = db::add_feed(&conn, "https://lobste.rs/rss", "Lobsters", None).unwrap();
    let hn = db::add_feed(&conn, "https://news.ycombinator.com/rss", "HN", None).unwrap();

    let published = chrono::DateTime::parse_from_rfc3339("2026-06-03T15:06:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let id_lobsters = db::insert_article(
        &conn,
        lobsters,
        "lobsters-abc",
        "A Post-Quantum Future for Let's Encrypt",
        "https://lobste.rs/s/abc/post_quantum",
        Some("alice"),
        Some("from lobsters"),
        None,
        None,
        Some(published),
    )
    .expect("insert A call")
    .expect("lobsters row inserted");

    // HN picked up the same story four hours later under its own
    // GUID, with a completely different URL and a different author.
    // URL-based dedup can't help here; only the title+date match.
    let id_hn = db::insert_article(
        &conn,
        hn,
        "hn-xyz",
        "A Post-Quantum Future for Let's Encrypt",
        "https://news.ycombinator.com/item?id=99999",
        Some("SGran"),
        Some("from hn"),
        None,
        None,
        Some(published + chrono::Duration::hours(4)),
    )
    .expect("insert B call");
    assert!(
        id_hn.is_none(),
        "HN row should be deduped by (title_hash, pub_day)"
    );

    let kept = db::get_article(&conn, id_lobsters)
        .unwrap()
        .expect("article");
    assert_eq!(
        kept.feed_id, lobsters,
        "older feed (Lobsters) keeps the row"
    );
    assert_eq!(kept.author.as_deref(), Some("alice"));
    assert_eq!(
        kept.title_hash.as_deref(),
        Some("a post-quantum future for let's encrypt")
    );
    assert_eq!(kept.pub_day.as_deref(), Some("2026-06-03"));
}

/// Same title on a *different* day is a different story and must NOT
/// be collapsed. This is the trade-off the user accepted: a weekly
/// newsletter titled the same on successive days would not dedup.
#[test]
fn insert_article_keeps_same_title_on_different_days() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");

    let feed = db::add_feed(&conn, "https://example.com/rss", "F", None).unwrap();

    let day1 = chrono::DateTime::parse_from_rfc3339("2026-06-01T08:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let day2 = day1 + chrono::Duration::days(1);

    let id1 = db::insert_article(
        &conn,
        feed,
        "g1",
        "Daily digest",
        "https://example.com/1",
        None,
        None,
        None,
        None,
        Some(day1),
    )
    .expect("insert 1")
    .expect("1 inserted");
    let id2 = db::insert_article(
        &conn,
        feed,
        "g2",
        "Daily digest",
        "https://example.com/2",
        None,
        None,
        None,
        None,
        Some(day2),
    )
    .expect("insert 2")
    .expect("2 inserted: different day");
    assert_ne!(id1, id2);

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM articles", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2, "same title on different days must not dedup");
}

/// Whitespace / case differences in titles must still match: a feed
/// that double-spaces or capitalizes a headline is the same story as
/// one that doesn't.
#[test]
fn insert_article_dedup_title_is_case_and_whitespace_insensitive() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_a = db::add_feed(&conn, "https://a.example/rss", "A", None).unwrap();
    let feed_b = db::add_feed(&conn, "https://b.example/rss", "B", None).unwrap();
    let day = chrono::DateTime::parse_from_rfc3339("2026-06-03T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    db::insert_article(
        &conn,
        feed_a,
        "g1",
        "Hello   World",
        "https://a/x",
        None,
        None,
        None,
        None,
        Some(day),
    )
    .expect("insert 1")
    .expect("1 ok");
    let dup = db::insert_article(
        &conn,
        feed_b,
        "g2",
        "  hello world\n",
        "https://b/y",
        None,
        None,
        None,
        None,
        Some(day),
    )
    .expect("insert 2 call");
    assert!(dup.is_none(), "case + whitespace variants must dedup");

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM articles", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn cosine_similarity_basic() {
    let a = vec![1.0, 0.0, 0.0];
    let b = vec![1.0, 0.0, 0.0];
    assert!((ollama::cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
    let c = vec![0.0, 1.0, 0.0];
    assert!(ollama::cosine_similarity(&a, &c).abs() < 1e-6);
    let d = vec![-1.0, 0.0, 0.0];
    assert!((ollama::cosine_similarity(&a, &d) + 1.0).abs() < 1e-6);
}

#[test]
fn cosine_similarity_zero_length() {
    let empty: Vec<f32> = vec![];
    let a = vec![1.0, 2.0];
    assert_eq!(ollama::cosine_similarity(&empty, &a), 0.0);
    assert_eq!(ollama::cosine_similarity(&a, &empty), 0.0);
}

#[test]
fn vote_persists_and_overwrites() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    let aid = db::insert_article(
        &conn,
        feed_id,
        "g",
        "T",
        "u/1",
        None,
        None,
        Some("x"),
        None,
        None,
    )
    .unwrap()
    .unwrap();

    db::set_vote(&conn, aid, models::Vote::Up).unwrap();
    let article = db::get_article(&conn, aid).unwrap().unwrap();
    assert_eq!(article.vote, 1);

    db::set_vote(&conn, aid, models::Vote::Down).unwrap();
    let article = db::get_article(&conn, aid).unwrap().unwrap();
    assert_eq!(article.vote, -1);

    db::set_vote(&conn, aid, models::Vote::None).unwrap();
    let article = db::get_article(&conn, aid).unwrap().unwrap();
    assert_eq!(article.vote, 0);
}

#[test]
fn get_vote_round_trip() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    let aid = db::insert_article(
        &conn, feed_id, "g", "T", "u/1", None, None, None, None, None,
    )
    .unwrap()
    .unwrap();

    // No row yet -> None.
    assert_eq!(db::get_vote(&conn, aid).unwrap(), models::Vote::None);

    db::set_vote(&conn, aid, models::Vote::Up).unwrap();
    assert_eq!(db::get_vote(&conn, aid).unwrap(), models::Vote::Up);

    db::set_vote(&conn, aid, models::Vote::Down).unwrap();
    assert_eq!(db::get_vote(&conn, aid).unwrap(), models::Vote::Down);

    db::set_vote(&conn, aid, models::Vote::None).unwrap();
    assert_eq!(db::get_vote(&conn, aid).unwrap(), models::Vote::None);
}

#[test]
fn ranked_articles_sorts_by_similarity() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    let now = chrono::Utc::now();
    let a1 = db::insert_article(
        &conn,
        feed_id,
        "g1",
        "T1",
        "u/1",
        None,
        None,
        Some("x"),
        None,
        Some(now),
    )
    .unwrap()
    .unwrap();
    let a2 = db::insert_article(
        &conn,
        feed_id,
        "g2",
        "T2",
        "u/2",
        None,
        None,
        Some("x"),
        None,
        Some(now),
    )
    .unwrap()
    .unwrap();

    // Embeddings: a1 matches the preference, a2 does not.
    let pref = vec![1.0_f32, 0.0, 0.0];
    let emb1 = vec![1.0_f32, 0.0, 0.0];
    let emb2 = vec![0.0_f32, 1.0, 0.0];
    ranking::store_embedding(&conn, a1, &emb1).unwrap();
    ranking::store_embedding(&conn, a2, &emb2).unwrap();

    let scores = ranking::rank_articles_with_category(&conn, Some(&pref), 168.0, None).unwrap();
    assert_eq!(scores.len(), 2);
    assert_eq!(scores[0].0, a1, "matching article should rank first");
    assert!(scores[0].1 > scores[1].1);
}

#[test]
fn preference_vector_round_trip() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    assert!(ranking::load_preference_vector(&conn).unwrap().is_none());
    let v = vec![0.1_f32, 0.2, 0.3];
    ranking::save_preference_vector(&conn, &v).unwrap();
    let loaded = ranking::load_preference_vector(&conn).unwrap().unwrap();
    assert_eq!(loaded, v);
}

#[test]
fn settings_persist() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let s = models::Settings {
        ollama_url: "http://example:1234".into(),
        ollama_embed_model: "embed-x".into(),
        ollama_chat_model: "chat-y".into(),
        vote_weight: 0.5,
        time_half_life_hours: 24.0,
        category_labels: vec!["Tech".into(), "Politics".into()],
        category_weight: 1.0,
        background_refresh_minutes: 7,
    };
    settings::save(&conn, &s).unwrap();
    let loaded = settings::load(&conn).unwrap();
    assert_eq!(loaded.ollama_url, "http://example:1234");
    assert_eq!(loaded.ollama_embed_model, "embed-x");
    assert!((loaded.time_half_life_hours - 24.0).abs() < 1e-6);
    assert_eq!(loaded.background_refresh_minutes, 7);
}

#[test]
fn settings_round_trip_preserves_background_refresh_minutes() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    // Saving 0 (disabled) round-trips as 0.
    settings::save(
        &conn,
        &models::Settings {
            background_refresh_minutes: 0,
            ..models::Settings::default()
        },
    )
    .unwrap();
    assert_eq!(settings::load(&conn).unwrap().background_refresh_minutes, 0);

    // A non-default value also round-trips.
    settings::save(
        &conn,
        &models::Settings {
            background_refresh_minutes: 30,
            ..models::Settings::default()
        },
    )
    .unwrap();
    assert_eq!(
        settings::load(&conn).unwrap().background_refresh_minutes,
        30
    );

    // A garbage value falls back to the default (15) instead of panicking.
    conn.execute(
        "UPDATE settings SET value = 'not-a-number' WHERE key = 'background_refresh_minutes'",
        [],
    )
    .unwrap();
    assert_eq!(
        settings::load(&conn).unwrap().background_refresh_minutes,
        15
    );
}

#[test]
fn is_disabled_uses_zero_as_sentinel() {
    // The convention: 0 minutes = disabled. This is the contract the
    // Settings dialog and the run_background_loop both rely on, so
    // pin it down here.
    assert!(refresh::is_disabled(0));
    assert!(!refresh::is_disabled(1));
    assert!(!refresh::is_disabled(15));
    assert!(!refresh::is_disabled(u32::MAX));
}

#[test]
fn refresh_lock_allows_only_one_holder() {
    use std::sync::atomic::AtomicBool;
    let flag = AtomicBool::new(false);
    // First acquire wins.
    assert!(refresh::try_acquire_refresh_lock(&flag));
    // Second acquire while the first is held must fail.
    assert!(!refresh::try_acquire_refresh_lock(&flag));
    // After release, a new acquire succeeds.
    refresh::release_refresh_lock(&flag);
    assert!(refresh::try_acquire_refresh_lock(&flag));
    refresh::release_refresh_lock(&flag);
}

#[test]
fn article_to_text_truncates_long_content() {
    use ollama::article_to_text;
    let big = "x".repeat(10_000);
    let s = article_to_text("Title", Some(&big), Some("sum"));
    assert!(s.contains("Title"));
    assert!(s.contains("sum"));
    // The whole "Content: xxxx..." section should be no more than MAX_CHARS chars.
    assert!(s.len() < 10_000);
}

#[test]
fn content_status_round_trip() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    let aid = db::insert_article(
        &conn,
        feed_id,
        "g",
        "T",
        "u/1",
        None,
        None,
        Some("x"),
        None,
        None,
    )
    .unwrap()
    .unwrap();

    // Default is None.
    let a = db::get_article(&conn, aid).unwrap().unwrap();
    assert_eq!(a.content_status, models::ContentStatus::None);
    assert!(a.content_fetched_at.is_none());

    db::mark_content_status(&conn, aid, models::ContentStatus::Fetching).unwrap();
    let a = db::get_article(&conn, aid).unwrap().unwrap();
    assert_eq!(a.content_status, models::ContentStatus::Fetching);
    assert!(a.content_fetched_at.is_none());

    db::mark_content_status(&conn, aid, models::ContentStatus::Loaded).unwrap();
    let a = db::get_article(&conn, aid).unwrap().unwrap();
    assert_eq!(a.content_status, models::ContentStatus::Loaded);
    assert!(a.content_fetched_at.is_some());

    db::store_extracted_content(&conn, aid, "extracted body", "extracted **body**").unwrap();
    let a = db::get_article(&conn, aid).unwrap().unwrap();
    assert_eq!(a.content.as_deref(), Some("extracted body"));
    assert_eq!(a.content_markdown.as_deref(), Some("extracted **body**"));
    assert_eq!(a.content_status, models::ContentStatus::Loaded);
}

#[test]
fn articles_pending_content_filters_by_status() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    let a1 = db::insert_article(
        &conn,
        feed_id,
        "g1",
        "T1",
        "u/1",
        None,
        None,
        Some("x"),
        None,
        None,
    )
    .unwrap()
    .unwrap();
    let a2 = db::insert_article(
        &conn,
        feed_id,
        "g2",
        "T2",
        "u/2",
        None,
        None,
        Some("x"),
        None,
        None,
    )
    .unwrap()
    .unwrap();
    let a3 = db::insert_article(
        &conn,
        feed_id,
        "g3",
        "T3",
        "u/3",
        None,
        None,
        Some("x"),
        None,
        None,
    )
    .unwrap()
    .unwrap();

    db::mark_content_status(&conn, a2, models::ContentStatus::Loaded).unwrap();
    let pending = db::articles_pending_content(&conn, 100).unwrap();
    let pending_ids: Vec<i64> = pending.iter().map(|a| a.id).collect();
    assert!(pending_ids.contains(&a1));
    assert!(!pending_ids.contains(&a2));
    assert!(pending_ids.contains(&a3));
}

#[test]
fn content_is_substantial_heuristic() {
    assert!(!content::content_is_substantial(None));
    assert!(!content::content_is_substantial(Some("")));
    let long = "word ".repeat(200);
    assert!(content::content_is_substantial(Some(&long)));
    let linky = "<a href='x'>link</a> ".repeat(50);
    assert!(!content::content_is_substantial(Some(&linky)));
    let lobsters_like = format!(
        "<p><a href=\"{}\">Comments</a></p>",
        "https://lobste.rs/s/abc/story"
    );
    assert!(
        !content::content_is_substantial(Some(&lobsters_like)),
        "lobsters' content field is just a Comments link"
    );
}

#[test]
fn article_read_state_round_trip() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    let aid = db::insert_article(
        &conn,
        feed_id,
        "g",
        "T",
        "u/1",
        None,
        None,
        Some("x"),
        None,
        None,
    )
    .unwrap()
    .unwrap();

    // Fresh article: unread.
    let a = db::get_article(&conn, aid).unwrap().unwrap();
    assert!(a.read_at.is_none());

    db::set_article_read(&conn, aid).unwrap();
    let a = db::get_article(&conn, aid).unwrap().unwrap();
    assert!(a.read_at.is_some());

    db::set_article_unread(&conn, aid).unwrap();
    let a = db::get_article(&conn, aid).unwrap().unwrap();
    assert!(a.read_at.is_none());
}

fn insert_article_with_status(
    conn: &rusqlite::Connection,
    feed_id: i64,
    guid: &str,
    title: &str,
    body: &str,
    category: Option<&str>,
    status: models::ContentStatus,
) -> i64 {
    let id = db::insert_article(
        conn,
        feed_id,
        guid,
        title,
        &format!("u/{guid}"),
        None,
        None,
        Some(body),
        None,
        None,
    )
    .unwrap()
    .unwrap();
    if let Some(c) = category {
        db::set_article_category(conn, id, c).unwrap();
    }
    if !matches!(status, models::ContentStatus::None) {
        db::mark_content_status(conn, id, status).unwrap();
    }
    id
}

#[test]
fn set_article_category_round_trip() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    let id = insert_article_with_status(
        &conn,
        feed_id,
        "g1",
        "T",
        "body",
        None,
        models::ContentStatus::None,
    );
    assert!(db::get_article(&conn, id)
        .unwrap()
        .unwrap()
        .category
        .is_none());
    db::set_article_category(&conn, id, "AI").unwrap();
    assert_eq!(
        db::get_article(&conn, id)
            .unwrap()
            .unwrap()
            .category
            .as_deref(),
        Some("AI")
    );
    db::set_article_category(&conn, id, "Cryptography").unwrap();
    assert_eq!(
        db::get_article(&conn, id)
            .unwrap()
            .unwrap()
            .category
            .as_deref(),
        Some("Cryptography")
    );
}

#[test]
fn articles_pending_classification_filters() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    let uncategorized = insert_article_with_status(
        &conn,
        feed_id,
        "g1",
        "T1",
        "body",
        None,
        models::ContentStatus::None,
    );
    let already_categorized = insert_article_with_status(
        &conn,
        feed_id,
        "g2",
        "T2",
        "body",
        Some("Tech"),
        models::ContentStatus::None,
    );
    let failed_fetch = insert_article_with_status(
        &conn,
        feed_id,
        "g3",
        "T3",
        "body",
        None,
        models::ContentStatus::Failed,
    );

    let pending: Vec<i64> = db::articles_pending_classification(&conn, 100)
        .unwrap()
        .iter()
        .map(|a| a.id)
        .collect();
    assert!(pending.contains(&uncategorized));
    assert!(!pending.contains(&already_categorized));
    // Failed content fetches no longer block classification: the
    // title + summary alone are enough for the model to pick a label,
    // and excluding these would leave the article stuck with
    // `category = NULL` forever. The Rust-side filter in
    // `classify_pending_with_concurrency` then drops inputs whose
    // combined title+summary+content is actually empty.
    assert!(pending.contains(&failed_fetch));
}

#[test]
fn category_counts_groups_by_category_with_unread_and_total() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    let a1 = insert_article_with_status(
        &conn,
        feed_id,
        "g1",
        "T1",
        "b",
        Some("AI"),
        models::ContentStatus::None,
    );
    let a2 = insert_article_with_status(
        &conn,
        feed_id,
        "g2",
        "T2",
        "b",
        Some("AI"),
        models::ContentStatus::None,
    );
    let _a3 = insert_article_with_status(
        &conn,
        feed_id,
        "g3",
        "T3",
        "b",
        Some("AI"),
        models::ContentStatus::None,
    );
    let _a4 = insert_article_with_status(
        &conn,
        feed_id,
        "g4",
        "T4",
        "b",
        Some("Tech"),
        models::ContentStatus::None,
    );
    let _a5 = insert_article_with_status(
        &conn,
        feed_id,
        "g5",
        "T5",
        "b",
        Some("Cryptography"),
        models::ContentStatus::None,
    );
    // Mark two AI articles as read.
    db::set_article_read(&conn, a1).unwrap();
    db::set_article_read(&conn, a2).unwrap();
    // _a3 stays unread (still in AI). All Tech/Cryptography articles
    // are unread (no read_at set).

    let counts = db::category_counts(&conn).unwrap();
    let by_cat: std::collections::HashMap<String, (i64, i64)> = counts
        .into_iter()
        .map(|(name, unread, total)| (name, (unread, total)))
        .collect();
    // AI: 3 total, 1 unread.
    assert_eq!(by_cat.get("AI"), Some(&(1, 3)));
    // Tech: 1 total, 1 unread.
    assert_eq!(by_cat.get("Tech"), Some(&(1, 1)));
    // Cryptography: 1 total, 1 unread. (The user-reported bug was that
    // this category disappeared when its only article was read; the
    // new query keeps it visible with (0, 1).)
    assert_eq!(by_cat.get("Cryptography"), Some(&(1, 1)));

    // Mark the only Cryptography article as read — category should still
    // appear with (0, 1).
    let only_crypto = by_cat.keys().find(|k| *k == "Cryptography").cloned();
    if only_crypto.is_some() {
        // Find the actual id we inserted (we know it's a5 — but the
        // test doesn't keep that handle, so do it via SQL).
        let crypto_id: i64 = conn
            .query_row("SELECT id FROM articles WHERE guid = 'g5'", [], |r| {
                r.get(0)
            })
            .unwrap();
        db::set_article_read(&conn, crypto_id).unwrap();
    }
    let counts2 = db::category_counts(&conn).unwrap();
    let by_cat2: std::collections::HashMap<String, (i64, i64)> = counts2
        .into_iter()
        .map(|(name, unread, total)| (name, (unread, total)))
        .collect();
    // Cryptography now has 0 unread but 1 total — must still appear.
    assert!(
        by_cat2.contains_key("Cryptography"),
        "all-read category should still appear in sidebar counts"
    );
    assert_eq!(by_cat2.get("Cryptography"), Some(&(0, 1)));
}

#[test]
fn rank_articles_uses_category_vector_when_present() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    let now = chrono::Utc::now();
    let ai_a = db::insert_article(
        &conn,
        feed_id,
        "a1",
        "AI1",
        "u/a1",
        None,
        None,
        Some("body"),
        None,
        Some(now),
    )
    .unwrap()
    .unwrap();
    let tech_a = db::insert_article(
        &conn,
        feed_id,
        "t1",
        "Tech1",
        "u/t1",
        None,
        None,
        Some("body"),
        None,
        Some(now),
    )
    .unwrap()
    .unwrap();
    db::set_article_category(&conn, ai_a, "AI").unwrap();
    db::set_article_category(&conn, tech_a, "Tech").unwrap();
    // AI article's embedding is aligned with the GLOBAL pref but not with its
    // own category's pref; Tech article's embedding is aligned with its
    // category's pref but not with the global. If ranking uses the per-category
    // vector, Tech should outrank AI. If it falls back to the global, AI
    // outranks Tech.
    let ai_emb = vec![1.0_f32, 0.0, 0.0];
    let tech_emb = vec![0.0_f32, 1.0, 0.0];
    ranking::store_embedding(&conn, ai_a, &ai_emb).unwrap();
    ranking::store_embedding(&conn, tech_a, &tech_emb).unwrap();
    let global_pref = vec![1.0_f32, 0.0, 0.0];
    let ai_cat_pref = vec![0.0_f32, 1.0, 0.0];
    let tech_cat_pref = vec![0.0_f32, 1.0, 0.0];
    ranking::save_preference_vector(&conn, &global_pref).unwrap();
    ranking::save_category_preference(&conn, "AI", &ai_cat_pref).unwrap();
    ranking::save_category_preference(&conn, "Tech", &tech_cat_pref).unwrap();

    let scores =
        ranking::rank_articles_with_category(&conn, Some(&global_pref), 168.0, None).unwrap();
    let ai_score = scores.iter().find(|(id, _, _)| *id == ai_a).unwrap().1;
    let tech_score = scores.iter().find(|(id, _, _)| *id == tech_a).unwrap().1;
    assert!(
        tech_score > ai_score,
        "Tech article ({tech_score}) should outrank AI article ({ai_score}) when category prefs override the global"
    );
}

#[test]
fn rank_articles_falls_back_to_global_when_category_empty() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    let now = chrono::Utc::now();
    let a = db::insert_article(
        &conn,
        feed_id,
        "g",
        "T",
        "u/1",
        None,
        None,
        Some("body"),
        None,
        Some(now),
    )
    .unwrap()
    .unwrap();
    // Category assigned but no per-category vector stored.
    db::set_article_category(&conn, a, "AI").unwrap();
    ranking::store_embedding(&conn, a, &[1.0_f32, 0.0, 0.0]).unwrap();
    // Global pref aligned with the embedding.
    let global = vec![1.0_f32, 0.0, 0.0];
    ranking::save_preference_vector(&conn, &global).unwrap();
    // Sanity: no per-category vector exists yet.
    assert!(ranking::load_category_preference(&conn, "AI")
        .unwrap()
        .is_none());

    let scores = ranking::rank_articles_with_category(&conn, Some(&global), 168.0, None).unwrap();
    assert_eq!(scores.len(), 1);
    assert!(
        scores[0].1 > 1.0,
        "global fallback should still apply: got {}",
        scores[0].1
    );
}

#[test]
fn count_votes_for_category_filters_by_article_category() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    let ai_a = insert_article_with_status(
        &conn,
        feed_id,
        "ai",
        "AI1",
        "b",
        Some("AI"),
        models::ContentStatus::None,
    );
    let tech_a = insert_article_with_status(
        &conn,
        feed_id,
        "t1",
        "Tech1",
        "b",
        Some("Tech"),
        models::ContentStatus::None,
    );
    db::set_vote(&conn, ai_a, models::Vote::Up).unwrap();
    db::set_vote(&conn, tech_a, models::Vote::Down).unwrap();
    let (up, down) = db::count_votes_for_category(&conn, "AI").unwrap();
    assert_eq!(up, 1);
    assert_eq!(down, 0);
    let (up, down) = db::count_votes_for_category(&conn, "Tech").unwrap();
    assert_eq!(up, 0);
    assert_eq!(down, 1);
}

#[test]
fn assign_display_scores_is_linear_percentile() {
    use smarrst::backend::actions::assign_display_scores;

    // Empty list: no scores assigned.
    let mut empty: Vec<models::Article> = Vec::new();
    assign_display_scores(&mut empty);
    assert!(empty.is_empty());

    // Single article: gets 10.
    let mut one = vec![make_article(1, 0.5)];
    assign_display_scores(&mut one);
    assert_eq!(one[0].display_score, Some(10.0));

    // Two articles: 10 and 0.
    let mut two = vec![make_article(2, 0.9), make_article(3, 0.4)];
    assign_display_scores(&mut two);
    assert_eq!(two[0].display_score, Some(10.0));
    assert_eq!(two[1].display_score, Some(0.0));

    // Five articles: 10, 7.5, 5, 2.5, 0.
    let mut five: Vec<models::Article> = (0..5)
        .map(|i| make_article(i, 1.0 - i as f64 * 0.1))
        .collect();
    assign_display_scores(&mut five);
    let scores: Vec<f32> = five.iter().map(|a| a.display_score.unwrap()).collect();
    assert_eq!(scores, vec![10.0, 7.5, 5.0, 2.5, 0.0]);
}

fn make_article(id: i64, score: f64) -> models::Article {
    use chrono::Utc;
    models::Article {
        id,
        feed_id: 1,
        feed_title: "F".to_string(),
        title: format!("T{id}"),
        url: format!("u/{id}"),
        author: None,
        summary: None,
        content: None,
        content_markdown: None,
        published: None,
        fetched_at: Utc::now(),
        vote: 0,
        score,
        display_score: None,
        content_status: models::ContentStatus::None,
        content_fetched_at: None,
        read_at: None,
        category: None,
        canonical_url: None,
        title_hash: None,
        pub_day: None,
    }
}

#[test]
fn ranked_articles_groups_unread_then_read_within_each_by_score() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    let now = chrono::Utc::now();

    // Six articles: alternating read state, descending score.
    // We then mark a subset as read and assert the final order is:
    //   1) unread, by score desc
    //   2) read, by score desc
    let a1 = db::insert_article(
        &conn,
        feed_id,
        "a1",
        "A1",
        "u/1",
        None,
        None,
        Some("b"),
        None,
        Some(now),
    )
    .unwrap()
    .unwrap();
    let a2 = db::insert_article(
        &conn,
        feed_id,
        "a2",
        "A2",
        "u/2",
        None,
        None,
        Some("b"),
        None,
        Some(now),
    )
    .unwrap()
    .unwrap();
    let a3 = db::insert_article(
        &conn,
        feed_id,
        "a3",
        "A3",
        "u/3",
        None,
        None,
        Some("b"),
        None,
        Some(now),
    )
    .unwrap()
    .unwrap();
    let a4 = db::insert_article(
        &conn,
        feed_id,
        "a4",
        "A4",
        "u/4",
        None,
        None,
        Some("b"),
        None,
        Some(now),
    )
    .unwrap()
    .unwrap();
    let a5 = db::insert_article(
        &conn,
        feed_id,
        "a5",
        "A5",
        "u/5",
        None,
        None,
        Some("b"),
        None,
        Some(now),
    )
    .unwrap()
    .unwrap();
    let a6 = db::insert_article(
        &conn,
        feed_id,
        "a6",
        "A6",
        "u/6",
        None,
        None,
        Some("b"),
        None,
        Some(now),
    )
    .unwrap()
    .unwrap();
    let _ = (a1, a2, a3, a4, a5, a6);

    // Mark a1, a3, a5 as read. a2, a4, a6 stay unread.
    db::set_article_read(&conn, a1).unwrap();
    db::set_article_read(&conn, a3).unwrap();
    db::set_article_read(&conn, a5).unwrap();

    // Now drive the ranking by running `ranked_articles` against a
    // synchronous state. We can't easily build a real AppState in an
    // integration test, so exercise the helper directly.
    let mut articles: Vec<models::Article> = (1..=6i64)
        .map(|id| db::get_article(&conn, id).unwrap().unwrap())
        .collect();

    // Sort by score desc (the raw ranking), then assign display scores
    // from that rank order, then re-sort by is_read (unread first) for
    // display. This is the exact sequence in `actions::ranked_articles`.
    articles.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    smarrst::backend::actions::assign_display_scores(&mut articles);
    articles.sort_by(|a, b| {
        let a_read = a.read_at.is_some();
        let b_read = b.read_at.is_some();
        a_read.cmp(&b_read)
    });

    // The article scores all start at 0 (no embeddings) so the
    // score-desc sort is stable, preserving the insert order. After
    // the read-state re-sort the order should be:
    //   a2 (unread), a4 (unread), a6 (unread),
    //   a1 (read), a3 (read), a5 (read)
    let order: Vec<i64> = articles.iter().map(|a| a.id).collect();
    assert_eq!(order, vec![a2, a4, a6, a1, a3, a5]);

    // Display scores are now the rank-percentile of the *rank* order, not
    // the list position. Rank order is a1..a6 (insert order, since all
    // scores are tied at 0), so the percentile is:
    //   a1=10, a2=8, a3=6, a4=4, a5=2, a6=0
    // Mapped to the regrouped list order (a2, a4, a6, a1, a3, a5),
    // the display_scores in list order are: 8, 4, 0, 10, 6, 2.
    // Crucially, an article's display_score does NOT change when its
    // read state changes — the rank ordering and percentile are stable.
    let displays: Vec<f32> = articles
        .iter()
        .map(|a| a.display_score.expect("display_score set"))
        .collect();
    let expected = [8.0_f32, 4.0, 0.0, 10.0, 6.0, 2.0];
    assert_eq!(displays.len(), expected.len());
    for (i, (got, want)) in displays.iter().zip(expected.iter()).enumerate() {
        assert!(
            (got - want).abs() < 1e-4,
            "display[{i}]: got {got}, want {want}"
        );
    }
}

/// Regression test: the user-reported bug was that reading an article
/// dropped its display_score (e.g. 10.0 → 7.9). The root cause was that
/// `assign_display_scores` ran *after* a read-state re-sort, so the
/// percentile was computed over a list whose order depended on read
/// state. After the fix, display_score is computed from the *rank*
/// order, so it stays stable when an article's read state changes.
#[test]
fn reading_an_article_does_not_change_its_display_score() {
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    let now = chrono::Utc::now();

    // Four articles with distinct scores so the rank order is
    // deterministic: a4 (highest) > a3 > a2 > a1.
    let mut aids = Vec::new();
    for i in 1..=4i64 {
        let aid = db::insert_article(
            &conn,
            feed_id,
            &format!("g{i}"),
            &format!("A{i}"),
            &format!("u/{i}"),
            None,
            None,
            Some("b"),
            None,
            Some(now),
        )
        .unwrap()
        .unwrap();
        aids.push(aid);
    }
    let (a1, a2, a3, a4) = (aids[0], aids[1], aids[2], aids[3]);
    // Give each article a distinct score. We set `score` directly via
    // the ranking helper (a1=0.1, a2=0.2, a3=0.3, a4=0.4). The DB
    // doesn't have a score column on the article row, so we drive the
    // rank order through the helper that `ranked_articles` uses.
    //
    // Since none of these have embeddings, the rank order falls back to
    // a stable order by published timestamp (all `now`, so insert order).
    // We rely on the stable-sort guarantee of `sort_by` and use distinct
    // `embedding` strings (set later) to break ties if needed; for this
    // test, distinct scores is enough because the article struct holds
    // `score` as a field that's already filled in by the actions layer.
    // To avoid needing Ollama, we replicate the sequence directly.
    let mut articles: Vec<models::Article> = aids
        .iter()
        .map(|id| db::get_article(&conn, *id).unwrap().unwrap())
        .collect();
    // Hand-set distinct scores to drive the rank order a4 > a3 > a2 > a1.
    articles[0].score = 0.1; // a1
    articles[1].score = 0.2; // a2
    articles[2].score = 0.3; // a3
    articles[3].score = 0.4; // a4

    // Run the ranking sequence *as it is in production* (after the
    // fix): sort by score desc, assign display scores, then re-sort by
    // read state for display.
    articles.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    smarrst::backend::actions::assign_display_scores(&mut articles);
    articles.sort_by(|a, b| {
        let a_read = a.read_at.is_some();
        let b_read = b.read_at.is_some();
        a_read.cmp(&b_read)
    });

    // Capture each article's display_score (lookup by id, since the
    // list is now reordered).
    fn score_of(articles: &[models::Article], id: i64) -> f32 {
        articles
            .iter()
            .find(|a| a.id == id)
            .and_then(|a| a.display_score)
            .expect("display_score set")
    }
    let before = [
        (a1, score_of(&articles, a1)),
        (a2, score_of(&articles, a2)),
        (a3, score_of(&articles, a3)),
        (a4, score_of(&articles, a4)),
    ];

    // All unread, so the read-state re-sort is a no-op. Verify the
    // baseline: a4 (highest score) is at the top with display_score 10.
    assert_eq!(articles[0].id, a4);
    assert!((score_of(&articles, a4) - 10.0).abs() < 1e-4);

    // Now mark a4 (the top-ranked article) as read and re-run the
    // sequence. The rank order is unchanged, so display_score should
    // also be unchanged for every article.
    db::set_article_read(&conn, a4).unwrap();
    let mut articles2: Vec<models::Article> = aids
        .iter()
        .map(|id| db::get_article(&conn, *id).unwrap().unwrap())
        .collect();
    articles2[0].score = 0.1;
    articles2[1].score = 0.2;
    articles2[2].score = 0.3;
    articles2[3].score = 0.4;
    articles2.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    smarrst::backend::actions::assign_display_scores(&mut articles2);
    articles2.sort_by(|a, b| {
        let a_read = a.read_at.is_some();
        let b_read = b.read_at.is_some();
        a_read.cmp(&b_read)
    });

    // The display_score for every article must be unchanged.
    for (id, prev) in before {
        let now = score_of(&articles2, id);
        assert!(
            (now - prev).abs() < 1e-4,
            "display_score changed for article {id} after reading a4: {prev} -> {now}"
        );
    }

    // Specifically, a4 (now read) must still have display_score 10 —
    // the user-reported "ranking dropped to 7.9" bug.
    assert!(
        (score_of(&articles2, a4) - 10.0).abs() < 1e-4,
        "a4's rank score should still be 10 after being read, got {}",
        score_of(&articles2, a4)
    );

    // And a4 should now be at the top of the *read* group (the bottom
    // of the visual list), not the top of the whole list.
    let a4_pos = articles2.iter().position(|a| a.id == a4).unwrap();
    assert!(
        a4_pos >= 3,
        "a4 should be in the read group (positions 3..4), got position {a4_pos}"
    );
}

/// Time-mode display sort: articles sorted by `published` desc, with
/// `fetched_at` fallback when `published` is None. Read and unread
/// articles are interleaved — the visible ordering is by date, not by
/// read state. The underlying rank ordering and `display_score`
/// percentile are computed first (stable), so reading an article in
/// Time mode still doesn't change its rank.
#[test]
fn ranked_articles_sorts_by_published_date_desc_when_sort_mode_is_time() {
    use smarrst::backend::models::SortMode;
    let conn = open_memory();
    db::init_schema(&conn).expect("init");
    let feed_id = db::add_feed(&conn, "u", "U", None).unwrap();
    // Three articles, distinct publication dates spanning a few days.
    // a_oldest is 3 days ago, a_middle is 2 days ago, a_newest is now.
    let now = chrono::Utc::now();
    let day = chrono::Duration::days(1);
    let a_oldest = db::insert_article(
        &conn,
        feed_id,
        "ao",
        "Oldest",
        "u/ao",
        None,
        None,
        Some("b"),
        None,
        Some(now - day * 3),
    )
    .unwrap()
    .unwrap();
    let a_middle = db::insert_article(
        &conn,
        feed_id,
        "am",
        "Middle",
        "u/am",
        None,
        None,
        Some("b"),
        None,
        Some(now - day * 2),
    )
    .unwrap()
    .unwrap();
    let a_newest = db::insert_article(
        &conn,
        feed_id,
        "an",
        "Newest",
        "u/an",
        None,
        None,
        Some("b"),
        None,
        Some(now),
    )
    .unwrap()
    .unwrap();
    // Mark the middle one as read to verify that Time mode does NOT
    // group reads below unreads.
    db::set_article_read(&conn, a_middle).unwrap();

    // Mirror the production sequence in Time mode: score desc →
    // assign_display_scores → sort by published desc.
    let mut articles: Vec<models::Article> = [a_oldest, a_middle, a_newest]
        .iter()
        .map(|id| db::get_article(&conn, *id).unwrap().unwrap())
        .collect();
    articles.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    smarrst::backend::actions::assign_display_scores(&mut articles);
    articles.sort_by(|a, b| {
        let a_dt = a.published.unwrap_or(a.fetched_at);
        let b_dt = b.published.unwrap_or(b.fetched_at);
        b_dt.cmp(&a_dt)
    });

    // Expected order: newest, middle (read), oldest. Read state is
    // ignored for ordering.
    let order: Vec<i64> = articles.iter().map(|a| a.id).collect();
    assert_eq!(
        order,
        vec![a_newest, a_middle, a_oldest],
        "Time mode should order by published date desc, ignoring read state"
    );

    // Display scores are still the rank-percentile of the raw score
    // order. Since scores are all 0, the rank order falls back to
    // insert order: a_oldest=10, a_middle=5, a_newest=0 (n=3, denom=2).
    // After the Time re-sort the visible order is a_newest, a_middle,
    // a_oldest with display_scores 0, 5, 10.
    let displays: Vec<f32> = articles
        .iter()
        .map(|a| a.display_score.expect("display_score set"))
        .collect();
    let expected = [0.0_f32, 5.0, 10.0];
    for (i, (got, want)) in displays.iter().zip(expected.iter()).enumerate() {
        assert!(
            (got - want).abs() < 1e-4,
            "display[{i}]: got {got}, want {want}"
        );
    }
    // Reading the middle article must not change any article's
    // display_score — the rank order is unchanged.
    let _ = SortMode::default(); // silence unused-import warning on rebuild
}

/// Filter predicate used by the article-list render path. Verifies
/// each (filter, read state, staleness) combination behaves as the
/// inline docstring on `should_show` describes.
#[test]
fn should_show_respects_filter_and_cutoff() {
    use smarrst::backend::models::ListFilter;
    let now = chrono::Utc::now();
    let day = chrono::Duration::days(1);
    // Cutoff = 1 day ago. Read articles older than that are stale.
    let cutoff = now - day;

    // Build a synthetic article: unread, fresh.
    let fresh_unread = sample_article(None);
    // Read 30 minutes ago (within cutoff, so still fresh).
    let fresh_read = sample_article(Some(now - chrono::Duration::minutes(30)));
    // Read 2 days ago (older than cutoff, so stale).
    let stale_read = sample_article(Some(now - day * 2));

    // UnreadOnly: only unread.
    assert!(should_show_pub(
        &fresh_unread,
        ListFilter::UnreadOnly,
        cutoff
    ));
    assert!(!should_show_pub(
        &fresh_read,
        ListFilter::UnreadOnly,
        cutoff
    ));
    assert!(!should_show_pub(
        &stale_read,
        ListFilter::UnreadOnly,
        cutoff
    ));

    // All: unread always; read only if not stale.
    assert!(should_show_pub(&fresh_unread, ListFilter::All, cutoff));
    assert!(should_show_pub(&fresh_read, ListFilter::All, cutoff));
    assert!(!should_show_pub(&stale_read, ListFilter::All, cutoff));

    // ReadOnly: only read; staleness ignored (user is browsing).
    assert!(!should_show_pub(
        &fresh_unread,
        ListFilter::ReadOnly,
        cutoff
    ));
    assert!(should_show_pub(&fresh_read, ListFilter::ReadOnly, cutoff));
    assert!(should_show_pub(&stale_read, ListFilter::ReadOnly, cutoff));
}

/// Edge case: an article read exactly at the cutoff boundary counts
/// as fresh (the comparison is `>=`, not `>`). This is deliberate —
/// the user just read it, so it's not "stale" yet.
#[test]
fn should_show_treats_cutoff_as_inclusive() {
    use smarrst::backend::models::ListFilter;
    let cutoff = chrono::Utc::now();
    let at_cutoff = sample_article(Some(cutoff));
    assert!(should_show_pub(&at_cutoff, ListFilter::All, cutoff));
}

fn sample_article(read_at: Option<chrono::DateTime<chrono::Utc>>) -> models::Article {
    models::Article {
        id: 1,
        feed_id: 1,
        feed_title: "F".to_string(),
        title: "T".to_string(),
        url: "https://example.com/x".to_string(),
        author: None,
        summary: None,
        content: None,
        content_markdown: None,
        published: None,
        fetched_at: chrono::Utc::now(),
        vote: 0,
        score: 0.0,
        display_score: None,
        content_status: models::ContentStatus::None,
        content_fetched_at: None,
        read_at,
        category: None,
        canonical_url: None,
        title_hash: None,
        pub_day: None,
    }
}

fn should_show_pub(
    a: &models::Article,
    filter: models::ListFilter,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> bool {
    smarrst::backend::actions::should_show(a, filter, cutoff)
}
