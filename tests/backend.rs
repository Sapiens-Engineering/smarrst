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
    let failed = insert_article_with_status(
        &conn,
        feed_id,
        "g3",
        "T3",
        "body",
        None,
        models::ContentStatus::Failed,
    );
    let no_body = insert_article_with_status(
        &conn,
        feed_id,
        "g4",
        "T4",
        "",
        None,
        models::ContentStatus::None,
    );
    let _ = no_body;

    let pending: Vec<i64> = db::articles_pending_classification(&conn, 100)
        .unwrap()
        .iter()
        .map(|a| a.id)
        .collect();
    assert!(pending.contains(&uncategorized));
    assert!(!pending.contains(&already_categorized));
    assert!(!pending.contains(&failed));
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
