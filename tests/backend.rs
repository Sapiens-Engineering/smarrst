//! Integration tests for the smarrst backend. Exercise database, settings and
//! ranking logic against an in-memory SQLite store; do not require Ollama.

use smarrst::backend::{content, db, models, ollama, ranking, settings};

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

    let scores = ranking::rank_articles(&conn, Some(&pref), 168.0).unwrap();
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
    };
    settings::save(&conn, &s).unwrap();
    let loaded = settings::load(&conn).unwrap();
    assert_eq!(loaded.ollama_url, "http://example:1234");
    assert_eq!(loaded.ollama_embed_model, "embed-x");
    assert!((loaded.time_half_life_hours - 24.0).abs() < 1e-6);
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
