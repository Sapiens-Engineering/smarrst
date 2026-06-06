use crate::backend::db;
use crate::backend::models::{Article, ContentStatus, Feed, ListFilter, Settings, SortMode, Vote};
use crate::backend::{content, ollama, ranking, rss, AppState};
use chrono::{DateTime, Utc};
use tokio::task;

// Each public action runs its synchronous SQLite work inside spawn_blocking
// so we never call blocking_lock() from the async runtime.

pub async fn list_feeds(state: &AppState) -> anyhow::Result<Vec<Feed>> {
    let db = state.db.clone();
    task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        db::list_feeds(&conn)
    })
    .await?
}

pub async fn add_feed(state: &AppState, url: &str) -> anyhow::Result<()> {
    let parsed = rss::fetch_feed(state, url).await?;
    let feed_id = {
        let db = state.db.clone();
        let url = url.to_string();
        let title = parsed.title.clone();
        let description = parsed.description.clone();
        task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            db::add_feed(&conn, &url, &title, description.as_deref())
        })
        .await??
    };
    let count = parsed.entries.len();
    {
        let db = state.db.clone();
        task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            for e in parsed.entries {
                let _ = db::insert_article(
                    &conn,
                    feed_id,
                    &e.guid,
                    &e.title,
                    &e.url,
                    e.author.as_deref(),
                    e.summary.as_deref(),
                    e.content.as_deref(),
                    e.content_markdown.as_deref(),
                    e.published,
                )?;
            }
            db::mark_feed_fetched(&conn, feed_id)
        })
        .await??;
    }
    log::info!("added feed {url} with {count} entries");
    // Kick off background work: fetch missing content, then embed, then classify.
    let state2 = state.clone();
    tokio::spawn(async move {
        if let Err(e) = fetch_pending_content(&state2, 256).await {
            log::warn!("background content fetch failed: {e}");
        }
        if let Err(e) = ranking::embed_pending(&state2, 256).await {
            log::warn!("background embedding failed: {e}");
        }
        if let Err(e) = classify_pending(&state2, 256).await {
            log::warn!("background classify failed: {e}");
        }
    });
    Ok(())
}

pub async fn refresh_all(state: &AppState) -> anyhow::Result<usize> {
    let summary = crate::backend::refresh::concurrent_refresh_all(state, 4).await?;
    let _ = fetch_pending_content(state, 256).await;
    let _ = ranking::embed_pending(state, 256).await;
    let _ = classify_pending(state, 256).await;
    Ok(summary.new_articles)
}

pub async fn delete_feed(state: &AppState, id: i64) -> anyhow::Result<()> {
    let db = state.db.clone();
    task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        db::delete_feed(&conn, id)
    })
    .await?
}

pub async fn get_article(state: &AppState, id: i64) -> anyhow::Result<Option<Article>> {
    let db = state.db.clone();
    task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        db::get_article(&conn, id)
    })
    .await?
}

pub async fn mark_article_read(state: &AppState, id: i64) -> anyhow::Result<()> {
    let db = state.db.clone();
    task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        db::set_article_read(&conn, id)
    })
    .await?
}

pub async fn mark_article_unread(state: &AppState, id: i64) -> anyhow::Result<()> {
    let db = state.db.clone();
    task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        db::set_article_unread(&conn, id)
    })
    .await?
}

pub async fn vote(state: &AppState, article_id: i64, direction: i32) -> anyhow::Result<()> {
    let new_vote = match direction {
        1 => Vote::Up,
        -1 => Vote::Down,
        _ => Vote::None,
    };
    let current_vote: Vote = {
        let db = state.db.clone();
        task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            db::get_vote(&conn, article_id)
        })
        .await??
    };
    if current_vote == new_vote {
        return Ok(());
    }
    let delta = ranking::vote_delta(current_vote, new_vote);
    {
        let db = state.db.clone();
        task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            db::set_vote(&conn, article_id, new_vote)
        })
        .await??;
    }
    // Apply AI preference update. `delta` is the signed change in vote
    // (e.g. -1.0 for None → Down, +1.0 for Down → None, +2.0 for Down → Up),
    // so clearing a vote is the inverse of casting it.
    let article = get_article(state, article_id).await?;
    if let Some(article) = article {
        if let Err(e) = ranking::apply_vote_delta(state, &article, delta).await {
            log::warn!("apply_vote_delta failed: {e}");
        }
    }
    Ok(())
}

pub async fn ranked_articles(
    state: &AppState,
    feed_filter: Option<i64>,
    half_life_hours: f32,
    sort_mode: SortMode,
) -> anyhow::Result<Vec<Article>> {
    let (pref, scored) = {
        let db = state.db.clone();
        let pref = task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            ranking::load_preference_vector(&conn)
        })
        .await??;
        let db2 = state.db.clone();
        let pref_clone = pref.clone();
        let scored = task::spawn_blocking(move || {
            let conn = db2.blocking_lock();
            ranking::rank_articles_with_category(
                &conn,
                pref_clone.as_deref(),
                half_life_hours,
                feed_filter,
            )
        })
        .await??;
        (pref, scored)
    };
    let _ = pref;
    if scored.is_empty() {
        return Ok(Vec::new());
    }
    let mut by_id: std::collections::HashMap<i64, (f64, Option<String>)> =
        std::collections::HashMap::with_capacity(scored.len());
    for (id, score, cat) in scored {
        by_id.insert(id, (score, cat));
    }
    let articles = {
        let db = state.db.clone();
        let ids: Vec<i64> = by_id.keys().copied().collect();
        task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(mut a) = db::get_article(&conn, id)? {
                    if let Some((score, cat)) = by_id.get(&id) {
                        a.score = *score;
                        if a.category.is_none() {
                            a.category = cat.clone();
                        }
                    }
                    out.push(a);
                }
            }
            Ok::<Vec<Article>, anyhow::Error>(out)
        })
        .await??
    };
    let mut articles = articles;
    // Sort by score descending first. This is the "raw" ranking; it's
    // what feeds the display_score percentile below. `sort_by` is
    // stable, so the subsequent re-sort preserves the score-based
    // order within each group.
    articles.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // Assign the 0..=10 percentile *before* the display re-sort, so
    // that reading an article doesn't change its rating. The score is
    // the AI's preference rank, which is independent of read state and
    // sort mode. The re-sort below only changes visual ordering, not
    // the rating.
    assign_display_scores(&mut articles);
    // Apply the display sort. Time mode is the new default: list by
    // publication date desc (fetched_at fallback) — read and unread
    // are interleaved. Rating mode is the legacy behaviour: group
    // unread above read, score-desc within each group.
    match sort_mode {
        SortMode::Time => {
            articles.sort_by(|a, b| {
                let a_dt = a.published.unwrap_or(a.fetched_at);
                let b_dt = b.published.unwrap_or(b.fetched_at);
                b_dt.cmp(&a_dt)
            });
        }
        SortMode::Rating => {
            articles.sort_by(|a, b| {
                let a_read = a.read_at.is_some();
                let b_read = b.read_at.is_some();
                a_read.cmp(&b_read)
            });
        }
    }
    Ok(articles)
}

/// Assign a 0..=10 rank-percentile to each article based on its current
/// position in the (already-sorted-descending) slice. Top of the list
/// gets 10, bottom gets 0, middle gets 5. Linear interpolation. A
/// single-article list gets 10. Pure function — extracted for testing.
pub fn assign_display_scores(articles: &mut [Article]) {
    let n = articles.len();
    if n == 0 {
        return;
    }
    let denom = (n - 1).max(1) as f32;
    for (i, a) in articles.iter_mut().enumerate() {
        a.display_score = Some(10.0 * (1.0 - (i as f32) / denom));
    }
}

/// Whether the article should be visible in the list given the current
/// filter and (in `All` mode) a read-staleness cutoff. Pure function —
/// extracted for testing. Caller is responsible for picking the cutoff
/// timestamp, which is `now - time_half_life_hours` for the auto-hide
/// feature.
pub fn should_show(article: &Article, filter: ListFilter, hide_cutoff: DateTime<Utc>) -> bool {
    let is_read = article.read_at.is_some();
    match filter {
        ListFilter::UnreadOnly => !is_read,
        ListFilter::All => {
            if !is_read {
                true
            } else {
                // Safe: `is_read` implies `read_at.is_some()`.
                article.read_at.expect("is_read implies read_at is Some") >= hide_cutoff
            }
        }
        ListFilter::ReadOnly => is_read,
    }
}

pub async fn ping_ollama(state: &AppState) -> anyhow::Result<bool> {
    ollama::ping(state).await
}

pub async fn save_settings(state: &AppState, s: &Settings) -> anyhow::Result<()> {
    let db = state.db.clone();
    let s_for_db = s.clone();
    task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        crate::backend::settings::save(&conn, &s_for_db)
    })
    .await??;
    *state.settings.lock().await = s.clone();
    Ok(())
}

/// Fetch the original URL for a single article, extract the main text, and
/// re-embed. Idempotent: if content is already Loaded, this is a no-op.
pub async fn fetch_article_content(
    state: &AppState,
    article_id: i64,
) -> anyhow::Result<ContentStatus> {
    let article = get_article(state, article_id).await?;
    let Some(article) = article else {
        return Ok(ContentStatus::None);
    };
    // If the RSS content is already substantial, mark as loaded and skip the fetch.
    if content::content_is_substantial(article.content.as_deref()) {
        let db = state.db.clone();
        let id = article_id;
        task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            db::mark_content_status(&conn, id, ContentStatus::Loaded)
        })
        .await??;
        return Ok(ContentStatus::Loaded);
    }
    if article.url.trim().is_empty() {
        return Ok(ContentStatus::Failed);
    }
    // Mark as Fetching.
    {
        let db = state.db.clone();
        let id = article_id;
        task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            db::mark_content_status(&conn, id, ContentStatus::Fetching)
        })
        .await??;
    }
    let url = article.url.clone();
    let result = content::extract_from_url(state, &url).await;
    match result {
        Ok(product) => {
            // Use readability's cleaned HTML (not the raw page) so JSON-LD /
            // inline-JS / `<head>` noise that some feeds embed inside the
            // article body doesn't leak into the rendered view.
            let markdown = if !product.content.trim().is_empty() {
                content::html_to_markdown(&product.content, &url)
            } else {
                product
                    .text
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| format!("{l}\n\n"))
                    .collect()
            };
            let text = product.text;
            {
                let db = state.db.clone();
                let id = article_id;
                let t = text.clone();
                let md = markdown.clone();
                task::spawn_blocking(move || {
                    let conn = db.blocking_lock();
                    db::store_extracted_content(&conn, id, &t, &md)
                })
                .await??;
            }
            // Re-embed using the new content.
            let article_now = get_article(state, article_id).await?;
            if let Some(a) = article_now {
                let title = a.title.clone();
                let summary = a.summary.clone();
                let embed_text = ollama::article_to_text(&title, Some(&text), summary.as_deref());
                if let Err(e) = reembed_article(state, a.id, &embed_text).await {
                    log::warn!("re-embed after content fetch failed: {e}");
                }
            }
            Ok(ContentStatus::Loaded)
        }
        Err(e) => {
            log::warn!("content fetch failed for {url}: {e}");
            let db = state.db.clone();
            let id = article_id;
            task::spawn_blocking(move || {
                let conn = db.blocking_lock();
                db::mark_content_status(&conn, id, ContentStatus::Failed)
            })
            .await??;
            Ok(ContentStatus::Failed)
        }
    }
}

async fn reembed_article(state: &AppState, article_id: i64, text: &str) -> anyhow::Result<()> {
    let emb = ollama::embed(state, text).await?;
    let db = state.db.clone();
    task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        ranking::store_embedding(&conn, article_id, &emb)
    })
    .await??;
    Ok(())
}

/// Fetch content for up to `max` articles that don't have it yet. Returns the
/// number of articles that successfully transitioned to Loaded.
pub async fn fetch_pending_content(state: &AppState, max: usize) -> anyhow::Result<usize> {
    let pending = {
        let db = state.db.clone();
        let limit = max as i64;
        task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            db::articles_pending_content(&conn, limit)
        })
        .await??
    };
    if pending.is_empty() {
        return Ok(0);
    }
    let mut loaded = 0;
    for a in pending {
        match fetch_article_content(state, a.id).await {
            Ok(crate::backend::models::ContentStatus::Loaded) => loaded += 1,
            Ok(_) => {}
            Err(e) => log::warn!("fetch_article_content({}) failed: {e}", a.id),
        }
    }
    Ok(loaded)
}

pub async fn category_counts(state: &AppState) -> anyhow::Result<Vec<(String, i64, i64)>> {
    let db = state.db.clone();
    task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        db::category_counts(&conn)
    })
    .await?
}

pub async fn classify_pending(state: &AppState, max: usize) -> anyhow::Result<usize> {
    classify_pending_with_concurrency(state, max, 4).await
}

/// Bounded-concurrency fan-out: process up to `concurrency` Ollama calls in
/// parallel. A per-article failure logs and continues; the batch is not
/// aborted by a single bad model response (404, timeout, parse error).
/// Returns the number of articles successfully classified.
pub async fn classify_pending_with_concurrency(
    state: &AppState,
    max: usize,
    concurrency: usize,
) -> anyhow::Result<usize> {
    let labels = {
        let s = state.settings.lock().await;
        s.category_labels.clone()
    };
    let pending = {
        let db = state.db.clone();
        let limit = max as i64;
        task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            db::articles_pending_classification(&conn, limit)
        })
        .await??
    };
    if pending.is_empty() {
        return Ok(0);
    }
    let total = pending.len();
    log::info!("classify_pending: starting on {total} articles (concurrency {concurrency})");

    // Pre-filter empty inputs so we don't burn Ollama calls on them.
    let work: Vec<Article> = pending
        .into_iter()
        .filter(|a| {
            !ollama::article_to_classify_text(&a.title, a.content.as_deref(), a.summary.as_deref())
                .trim()
                .is_empty()
        })
        .collect();
    if work.is_empty() {
        return Ok(0);
    }

    // Verify the configured chat model is actually available before fanning
    // out — saves a flood of 404s on a misconfigured install.
    match ollama::ping_model(state, "chat").await {
        Ok(true) => {}
        Ok(false) => {
            log::warn!(
                "chat model not reachable on Ollama; classify will likely fail for every article. \
                 Check Settings → Chat model and ensure the model is pulled (`ollama list`)."
            );
        }
        Err(e) => {
            log::warn!("could not check chat model: {e}");
        }
    }

    let mut classified = 0usize;
    let mut failures = 0usize;
    let mut index = 0usize;
    let mut stream = futures::stream::iter(work.into_iter().map(|a| {
        let state = state.clone();
        let labels = labels.clone();
        async move {
            let text = ollama::article_to_classify_text(
                &a.title,
                a.content.as_deref(),
                a.summary.as_deref(),
            );
            let result = ollama::classify(&state, &text, &labels).await;
            (a.id, result)
        }
    }))
    .buffer_unordered(concurrency.max(1));
    use futures::StreamExt;
    while let Some((id, result)) = stream.next().await {
        index += 1;
        match result {
            Ok(category) => {
                let db = state.db.clone();
                let cat = category.clone();
                let stored = task::spawn_blocking(move || {
                    let conn = db.blocking_lock();
                    db::set_article_category(&conn, id, &cat)
                })
                .await;
                if stored.is_ok() {
                    classified += 1;
                    log::info!(
                        "classified {index}/{total}: article {id} -> {category} ({classified} ok)"
                    );
                } else if let Err(e) = stored {
                    log::warn!("classify store failed for article {id}: {e}");
                    failures += 1;
                }
            }
            Err(e) => {
                log::warn!("classify failed for article {id}: {e}");
                failures += 1;
            }
        }
    }
    if failures > 0 {
        log::warn!("classify_pending: {classified} ok, {failures} failed");
    } else {
        log::info!("classify_pending: {classified} ok");
    }
    Ok(classified)
}
