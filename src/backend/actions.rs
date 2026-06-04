use crate::backend::db;
use crate::backend::models::{Article, ContentStatus, Feed, Settings, Vote};
use crate::backend::{content, ollama, ranking, rss, AppState};
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
    // Kick off background work: fetch missing content, then embed.
    let state2 = state.clone();
    tokio::spawn(async move {
        if let Err(e) = fetch_pending_content(&state2, 256).await {
            log::warn!("background content fetch failed: {e}");
        }
        if let Err(e) = ranking::embed_pending(&state2, 256).await {
            log::warn!("background embedding failed: {e}");
        }
    });
    Ok(())
}

pub async fn refresh_all(state: &AppState) -> anyhow::Result<usize> {
    let feeds = list_feeds(state).await?;
    let mut total = 0;
    for f in feeds {
        match rss::refresh_feed(state, f.id).await {
            Ok(n) => total += n,
            Err(e) => log::warn!("refresh feed {} failed: {e}", f.id),
        }
    }
    // First bring in any missing content, then (re-)embed.
    let _ = fetch_pending_content(state, 256).await;
    let _ = ranking::embed_pending(state, 256).await;
    Ok(total)
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
    let vote = match direction {
        1 => Vote::Up,
        -1 => Vote::Down,
        _ => Vote::None,
    };
    {
        let db = state.db.clone();
        task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            db::set_vote(&conn, article_id, vote)
        })
        .await??;
    }
    // Apply AI preference update.
    let article = get_article(state, article_id).await?;
    if let Some(article) = article {
        if let Err(e) = ranking::apply_vote(state, &article, vote).await {
            log::warn!("apply_vote failed: {e}");
        }
    }
    Ok(())
}

pub async fn ranked_articles(
    state: &AppState,
    feed_filter: Option<i64>,
    half_life_hours: f32,
) -> anyhow::Result<Vec<Article>> {
    let pref = {
        let db = state.db.clone();
        let pref = task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            ranking::load_preference_vector(&conn)
        })
        .await??;
        pref
    };
    let scores = {
        let db = state.db.clone();
        let pref_clone = pref.clone();
        task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            ranking::ranked_articles_with_scores(
                &conn,
                pref_clone.as_deref(),
                half_life_hours,
                feed_filter,
            )
        })
        .await??
    };
    if scores.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(scores.len());
    for s in scores {
        if let Some(mut a) = get_article(state, s.id).await? {
            a.score = s.score;
            out.push(a);
        }
    }
    Ok(out)
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
