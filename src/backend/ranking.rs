use crate::backend::models::{Article, Vote};
use crate::backend::{db, ollama, AppState};
use chrono::Utc;
use rusqlite::{params, OptionalExtension};

const PREF_KEY: &str = "preference_vector";

/// Load the user preference vector (centroid of up-voted - down-voted embeddings) from settings.
pub fn load_preference_vector(conn: &rusqlite::Connection) -> anyhow::Result<Option<Vec<f32>>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![PREF_KEY],
            |r| r.get(0),
        )
        .ok();
    let Some(raw) = raw else { return Ok(None) };
    let vec: Vec<f32> = serde_json::from_str(&raw)?;
    Ok(Some(vec))
}

pub fn save_preference_vector(conn: &rusqlite::Connection, vec: &[f32]) -> anyhow::Result<()> {
    let raw = serde_json::to_string(vec)?;
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![PREF_KEY, raw],
    )?;
    Ok(())
}

/// Update the preference vector in response to a vote.
///
/// Uses the running weighted average: new_pref = (old_pref * (n-1) +/- embed) / n
/// where n is the count of votes contributing. We store the count alongside the
/// vector so updates are stable.
pub async fn apply_vote(state: &AppState, article: &Article, vote: Vote) -> anyhow::Result<()> {
    let text = ollama::article_to_text(
        &article.title,
        article.content.as_deref(),
        article.summary.as_deref(),
    );
    let embed = ollama::embed(state, &text).await?;
    let sign = match vote {
        Vote::Up => 1.0_f32,
        Vote::Down => -1.0_f32,
        Vote::None => 0.0_f32,
    };
    if sign == 0.0 {
        return Ok(());
    }
    let (up_count, down_count) = {
        let conn = state.db.lock().await;
        db::count_votes(&conn)?
    };
    let n = (up_count + down_count) as f32;
    let conn = state.db.lock().await;
    let prev = load_preference_vector(&conn)?.unwrap_or_default();
    let new = if prev.is_empty() {
        // First vote: seed the vector.
        embed.iter().map(|v| *v * sign).collect::<Vec<f32>>()
    } else if prev.len() != embed.len() {
        // Dimension mismatch: rebuild using this single signal.
        embed.iter().map(|v| *v * sign).collect::<Vec<f32>>()
    } else {
        prev.iter()
            .zip(embed.iter())
            .map(|(p, e)| (p * n + e * sign) / (n + 1.0))
            .collect::<Vec<f32>>()
    };
    save_preference_vector(&conn, &new)?;
    Ok(())
}

/// Score every article that has an embedding cached. Articles without an
/// embedding are skipped (they will be embedded on the next `embed_pending`).
pub fn rank_articles(
    conn: &rusqlite::Connection,
    pref: Option<&[f32]>,
    half_life_hours: f32,
) -> anyhow::Result<Vec<(i64, f64)>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, COALESCE(a.published, a.fetched_at), a.embedding
         FROM articles a
         WHERE a.embedding IS NOT NULL",
    )?;
    let now = Utc::now();
    let mut scores: Vec<(i64, f64)> = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let ts: String = row.get(1)?;
        let emb_raw: String = row.get(2)?;
        let ts = chrono::DateTime::parse_from_rfc3339(&ts)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| now);
        let emb: Vec<f32> = serde_json::from_str(&emb_raw).unwrap_or_default();
        if emb.is_empty() {
            continue;
        }
        let hours = (now - ts).num_minutes() as f32 / 60.0;
        let time_decay = if half_life_hours > 0.0 {
            0.5_f32.powf(hours / half_life_hours)
        } else {
            0.0
        };
        let similarity = pref
            .map(|p| ollama::cosine_similarity(p, &emb))
            .unwrap_or(0.0);
        // Map cosine similarity from [-1, 1] to [0, 1], then add time decay.
        let sim_norm = (similarity + 1.0) * 0.5;
        let score = (sim_norm as f64) + (time_decay as f64);
        scores.push((id, score));
    }
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scores)
}

pub fn store_embedding(
    conn: &rusqlite::Connection,
    article_id: i64,
    emb: &[f32],
) -> anyhow::Result<()> {
    let raw = serde_json::to_string(emb)?;
    conn.execute(
        "UPDATE articles SET embedding = ?1 WHERE id = ?2",
        params![raw, article_id],
    )?;
    Ok(())
}

pub async fn embed_pending(state: &AppState, max: usize) -> anyhow::Result<usize> {
    let pending = {
        let conn = state.db.lock().await;
        db::articles_missing_scores(&conn, max as i64)?
    };
    if pending.is_empty() {
        return Ok(0);
    }
    // Embed each article. We re-read content for each to keep it simple; this
    // runs in a background task so latency is not user-visible.
    let mut embedded = 0;
    for a in pending {
        let text = ollama::article_to_text(&a.title, a.content.as_deref(), a.summary.as_deref());
        match ollama::embed(state, &text).await {
            Ok(emb) => {
                let conn = state.db.lock().await;
                if let Err(e) = store_embedding(&conn, a.id, &emb) {
                    log::warn!("failed to store embedding for article {}: {e}", a.id);
                } else {
                    embedded += 1;
                }
            }
            Err(e) => {
                log::warn!("embedding failed for article {}: {e}", a.id);
                break;
            }
        }
    }
    Ok(embedded)
}

pub fn ranked_articles_with_scores(
    conn: &rusqlite::Connection,
    pref: Option<&[f32]>,
    half_life_hours: f32,
    feed_filter: Option<i64>,
) -> anyhow::Result<Vec<db::ScoredArticle>> {
    let scores = rank_articles(conn, pref, half_life_hours)?;
    if scores.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(scores.len());
    for (id, score) in scores {
        out.push(db::ScoredArticle { id, score });
    }
    if let Some(fid) = feed_filter {
        out.retain(|s| {
            conn.query_row(
                "SELECT 1 FROM articles WHERE id = ?1 AND feed_id = ?2",
                params![s.id, fid],
                |_| Ok(()),
            )
            .optional()
            .map(|o| o.is_some())
            .unwrap_or(false)
        });
    }
    Ok(out)
}
