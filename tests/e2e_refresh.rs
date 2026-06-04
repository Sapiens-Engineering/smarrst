//! End-to-end tests for the refresh pipeline. These tests use the
//! `concurrent_refresh_with` helper with synthetic fetchers so they
//! don't need a real HTTP server (and don't trip the SSRF guard).
//! The production path through `concurrent_refresh_all` is exercised
//! indirectly: the wiring between `list_feeds` → `concurrent_refresh_with`
//! is trivial, and `list_feeds` itself has its own round-trip test in
//! `backend.rs`.

use smarrst::backend::db;
use smarrst::backend::models::Feed;
use smarrst::backend::refresh;
use std::path::PathBuf;

fn fresh_data_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "smarrst-refresh-test-{}-{}",
        std::process::id(),
        name
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn concurrent_refresh_with_returns_zero_when_no_feeds() {
    let summary = refresh::concurrent_refresh_with(Vec::new(), 4, |_id| async { Ok(0) }).await;
    assert_eq!(summary.feeds_total, 0);
    assert_eq!(summary.feeds_succeeded, 0);
    assert_eq!(summary.feeds_failed, 0);
    assert_eq!(summary.new_articles, 0);
}

#[tokio::test]
async fn concurrent_refresh_with_isolates_per_feed_failures() {
    // Build a feed list with one feed that succeeds (3 new articles) and
    // one that fails. The success must be counted; the failure must be
    // counted; neither must abort the other.
    let feeds = vec![
        Feed {
            id: 1,
            url: "https://good.example/".into(),
            title: "Good".into(),
            description: None,
            last_fetched: None,
            created_at: chrono::Utc::now(),
        },
        Feed {
            id: 2,
            url: "https://bad.example/".into(),
            title: "Bad".into(),
            description: None,
            last_fetched: None,
            created_at: chrono::Utc::now(),
        },
    ];
    let summary = refresh::concurrent_refresh_with(feeds, 4, |id| async move {
        match id {
            1 => Ok(3),
            2 => Err(anyhow::anyhow!("404 not found")),
            _ => unreachable!(),
        }
    })
    .await;
    assert_eq!(summary.feeds_total, 2);
    assert_eq!(summary.feeds_succeeded, 1);
    assert_eq!(summary.feeds_failed, 1);
    assert_eq!(summary.new_articles, 3);
}

#[tokio::test]
async fn concurrent_refresh_with_actually_runs_in_parallel() {
    // Two feeds, each with a 200 ms delay. concurrency=2 should finish
    // in ~200 ms; concurrency=1 should take ~400 ms. The threshold
    // (concurrency=2 must beat concurrency=1 by ≥100 ms) avoids CI
    // flakiness while still pinning down the parallelism.
    use std::time::Instant;
    let feeds = vec![
        Feed {
            id: 1,
            url: "https://a.example/".into(),
            title: "A".into(),
            description: None,
            last_fetched: None,
            created_at: chrono::Utc::now(),
        },
        Feed {
            id: 2,
            url: "https://b.example/".into(),
            title: "B".into(),
            description: None,
            last_fetched: None,
            created_at: chrono::Utc::now(),
        },
    ];
    let slow_fetcher = |_id: i64| async {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        Ok(0)
    };

    let t = Instant::now();
    refresh::concurrent_refresh_with(feeds.clone(), 2, slow_fetcher).await;
    let parallel = t.elapsed();

    let t = Instant::now();
    refresh::concurrent_refresh_with(feeds, 1, slow_fetcher).await;
    let sequential = t.elapsed();

    assert!(
        parallel < sequential,
        "concurrency=2 ({parallel:?}) should be faster than concurrency=1 ({sequential:?})"
    );
    assert!(
        parallel < std::time::Duration::from_millis(350),
        "concurrency=2 should finish in ~200 ms (was {parallel:?})"
    );
}

#[tokio::test]
async fn concurrent_refresh_with_sums_new_article_counts() {
    let feeds = vec![
        Feed {
            id: 1,
            url: "u1".into(),
            title: "A".into(),
            description: None,
            last_fetched: None,
            created_at: chrono::Utc::now(),
        },
        Feed {
            id: 2,
            url: "u2".into(),
            title: "B".into(),
            description: None,
            last_fetched: None,
            created_at: chrono::Utc::now(),
        },
        Feed {
            id: 3,
            url: "u3".into(),
            title: "C".into(),
            description: None,
            last_fetched: None,
            created_at: chrono::Utc::now(),
        },
    ];
    let summary = refresh::concurrent_refresh_with(feeds, 2, |id| async move {
        Ok(id as usize) // 1, 2, 3 → sum 6
    })
    .await;
    assert_eq!(summary.feeds_succeeded, 3);
    assert_eq!(summary.new_articles, 6);
}

#[tokio::test]
async fn concurrent_refresh_all_returns_zero_when_db_has_no_feeds() {
    // The full wiring: list_feeds → concurrent_refresh_with → fetcher.
    // With an empty database, list_feeds returns nothing and the
    // summary is all zeros, with no fetcher ever being called.
    let dir = fresh_data_dir("no_feeds");
    let state = smarrst::AppState::new(&dir).expect("init state");
    let summary = refresh::concurrent_refresh_all(&state, 4)
        .await
        .expect("refresh");
    assert_eq!(summary.feeds_total, 0);
    assert_eq!(summary.feeds_succeeded, 0);
    assert_eq!(summary.feeds_failed, 0);
    assert_eq!(summary.new_articles, 0);
}

#[tokio::test]
async fn concurrent_refresh_all_uses_listed_feeds() {
    // Verify the wiring: feeds inserted via `db::add_feed` show up in
    // `concurrent_refresh_all`'s call to `concurrent_refresh_with`. We
    // can't override the real fetcher, but we can stub the feed's URL
    // to one that's guaranteed to fail (an unroutable address) so the
    // call is observable via the summary's failed count.
    let dir = fresh_data_dir("uses_listed_feeds");
    let state = smarrst::AppState::new(&dir).expect("init state");
    {
        let conn = state.db.lock().await;
        // Port 1 is reserved and refused; the request fails fast.
        db::add_feed(&conn, "http://127.0.0.1:1/feed", "Unreachable", None).expect("add feed");
    }
    // The SSRF guard will reject 127.0.0.1, so this fails with a
    // "blocked scheme or host" error, which is counted as a failure.
    // That's still a proof of wiring — the fetcher was reached.
    let summary = refresh::concurrent_refresh_all(&state, 1)
        .await
        .expect("refresh");
    assert_eq!(summary.feeds_total, 1);
    assert_eq!(summary.feeds_succeeded, 0);
    assert_eq!(summary.feeds_failed, 1);
    assert_eq!(summary.new_articles, 0);
}

#[tokio::test]
async fn full_pipeline_returns_zero_summary_when_db_is_empty() {
    // full_pipeline calls concurrent_refresh_all + fetch_pending_content
    // + embed_pending + classify_pending. With no feeds and no
    // pre-existing articles, every stage short-circuits. None of these
    // need Ollama.
    let dir = fresh_data_dir("pipeline_empty");
    let state = smarrst::AppState::new(&dir).expect("init state");
    let summary = refresh::full_pipeline(&state).await.expect("pipeline");
    assert_eq!(summary.feeds_total, 0);
    assert_eq!(summary.new_articles, 0);
}
