//! End-to-end test against a real local Ollama instance. Skipped automatically
//! when the server or the embedding model is not available, so CI on machines
//! without Ollama still passes.

use smarrst::backend::models::ContentStatus;
use smarrst::backend::ranking;
use std::path::PathBuf;

async fn ollama_reachable() -> bool {
    let client = reqwest::Client::new();
    let res = client
        .get("http://localhost:11434/api/tags")
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await;
    matches!(res, Ok(r) if r.status().is_success())
}

fn fresh_data_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("smarrst-test-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn end_to_end_add_feed_embed_rank() {
    if !ollama_reachable().await {
        eprintln!("Ollama not reachable, skipping");
        return;
    }

    let dir = fresh_data_dir("add_embed_rank");
    let state = smarrst::AppState::new(&dir).expect("init state");

    // Override settings so tests use whatever the host has.
    {
        let mut s = state.settings.lock().await;
        s.ollama_embed_model = "nomic-embed-text".to_string();
    }

    // Fetch a small, stable RSS feed. techCrunch isn't a guarantee; we use a
    // generic Atom feed from a reliable source.
    let url = "https://hnrss.org/frontpage";
    if let Err(e) = smarrst::backend::actions::add_feed(&state, url).await {
        eprintln!("could not fetch test feed (network?): {e}");
        return;
    }

    let feeds = smarrst::backend::actions::list_feeds(&state).await.unwrap();
    assert!(!feeds.is_empty(), "feed should have been added");

    // Embed pending articles.
    let n = ranking::embed_pending(&state, 32).await.expect("embed");
    assert!(n > 0, "should have embedded at least one article");

    // Rank articles.
    let ranked = smarrst::backend::actions::ranked_articles(&state, None, 168.0)
        .await
        .expect("rank");
    assert!(!ranked.is_empty());

    // Every ranked article should have a non-zero score: time-decay alone
    // contributes at least 0.5^(age_h / 168) for a fresh article, so this
    // guards against regressions where the score is forgotten in the UI flow.
    for a in &ranked {
        assert!(a.score > 0.0, "article {} has score 0", a.id);
    }

    // Vote on the top article; this should update the preference vector and
    // re-ranking should put it (or its close neighbors) first.
    if let Some(top) = ranked.first().cloned() {
        let top_id = top.id;
        let _ = smarrst::backend::actions::vote(&state, top_id, 1).await;
        let after = smarrst::backend::actions::ranked_articles(&state, None, 168.0)
            .await
            .expect("re-rank");
        assert!(!after.is_empty());
        assert_eq!(
            after.first().unwrap().id,
            top_id,
            "the upvoted article should still rank at the top"
        );
    }
}

/// Exercise the content-fetching pipeline against a real RSS feed whose
/// `content:encoded` is intentionally thin (Lobsters stories are mostly
/// "Comments" link stubs). The fetch action should pull the original URL,
/// extract the main text, store it, and re-embed.
#[tokio::test]
async fn end_to_end_content_fetch_for_thin_rss() {
    if !ollama_reachable().await {
        eprintln!("Ollama not reachable, skipping");
        return;
    }

    let dir = fresh_data_dir("add_embed_rank");
    let state = smarrst::AppState::new(&dir).expect("init state");
    {
        let mut s = state.settings.lock().await;
        s.ollama_embed_model = "nomic-embed-text".to_string();
    }

    let url = "https://hnrss.org/frontpage";
    if let Err(e) = smarrst::backend::actions::add_feed(&state, url).await {
        eprintln!("could not fetch test feed (network?): {e}");
        return;
    }

    eprintln!("add_feed succeeded, ranking...");
    // Embed so the article list is non-empty.
    let n = smarrst::backend::ranking::embed_pending(&state, 32)
        .await
        .expect("embed");
    eprintln!("embedded {n} articles");
    let ranked = smarrst::backend::actions::ranked_articles(&state, None, 168.0)
        .await
        .expect("rank");
    eprintln!("ranked returned {} articles", ranked.len());
    if ranked.is_empty() {
        eprintln!("no articles to test against");
        return;
    }
    // Pick any article. The test only needs to verify that the fetch path
    // moves content_status to either Loaded or Failed.
    let id = ranked[0].id;
    let result = smarrst::backend::actions::fetch_article_content(&state, id)
        .await
        .expect("fetch");
    let after = smarrst::backend::actions::get_article(&state, id)
        .await
        .unwrap()
        .unwrap();
    eprintln!(
        "fetch result for {}: {:?} (status now {:?}, body len {})",
        id,
        result,
        after.content_status,
        after.content.as_deref().map(str::len).unwrap_or(0)
    );
    assert!(matches!(
        after.content_status,
        ContentStatus::Loaded | ContentStatus::Failed
    ));
}
