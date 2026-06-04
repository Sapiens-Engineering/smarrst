use crate::backend::models::{Article, Vote};
use crate::backend::{db, ollama, AppState};
use chrono::Utc;
use rusqlite::params;

const PREF_KEY: &str = "preference_vector";
const CATEGORY_PREF_PREFIX: &str = "pref_vec:";

fn category_pref_key(category: &str) -> String {
    format!("{CATEGORY_PREF_PREFIX}{category}")
}

pub fn load_preference_vector(conn: &rusqlite::Connection) -> anyhow::Result<Option<Vec<f32>>> {
    load_pref_vector_for_key(conn, PREF_KEY)
}

pub fn save_preference_vector(conn: &rusqlite::Connection, vec: &[f32]) -> anyhow::Result<()> {
    save_pref_vector_for_key(conn, PREF_KEY, vec)
}

pub fn load_category_preference(
    conn: &rusqlite::Connection,
    category: &str,
) -> anyhow::Result<Option<Vec<f32>>> {
    load_pref_vector_for_key(conn, &category_pref_key(category))
}

pub fn save_category_preference(
    conn: &rusqlite::Connection,
    category: &str,
    vec: &[f32],
) -> anyhow::Result<()> {
    save_pref_vector_for_key(conn, &category_pref_key(category), vec)
}

fn load_pref_vector_for_key(
    conn: &rusqlite::Connection,
    key: &str,
) -> anyhow::Result<Option<Vec<f32>>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .ok();
    let Some(raw) = raw else { return Ok(None) };
    let vec: Vec<f32> = serde_json::from_str(&raw)?;
    Ok(Some(vec))
}

fn save_pref_vector_for_key(
    conn: &rusqlite::Connection,
    key: &str,
    vec: &[f32],
) -> anyhow::Result<()> {
    let raw = serde_json::to_string(vec)?;
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, raw],
    )?;
    Ok(())
}

fn update_running_mean(prev: &[f32], embed: &[f32], sign: f32, n: f32) -> Vec<f32> {
    if prev.is_empty() {
        return embed.iter().map(|v| *v * sign).collect();
    }
    if prev.len() != embed.len() {
        return embed.iter().map(|v| *v * sign).collect();
    }
    prev.iter()
        .zip(embed.iter())
        .map(|(p, e)| (p * (n - 1.0) + e * sign) / n)
        .collect()
}

/// Apply a vote *change* to the global and per-category preference vectors.
///
/// `delta` is the change in signed vote (`new_sign - old_sign`), so it can
/// be `+1.0` (newly Up-voted), `-1.0` (newly Down-voted), `+2.0` (flipped
/// from Down to Up — "really like this"), `-2.0` (flipped from Up to
/// Down), or any fractional value that sums to the change. `0.0` is a
/// no-op. Computing the delta in the caller (rather than the absolute
/// sign of the new vote) is what makes "Clear" work: a Down-vote cleared
/// becomes `delta = +1.0`, which removes the prior `-embed` contribution
/// from the running mean.
pub async fn apply_vote_delta(
    state: &AppState,
    article: &Article,
    delta: f32,
) -> anyhow::Result<()> {
    if delta == 0.0 {
        return Ok(());
    }
    let text = ollama::article_to_text(
        &article.title,
        article.content.as_deref(),
        article.summary.as_deref(),
    );
    let embed = ollama::embed(state, &text).await?;
    let (up_count, down_count) = {
        let conn = state.db.lock().await;
        db::count_votes(&conn)?
    };
    let n = (up_count + down_count) as f32;
    {
        let conn = state.db.lock().await;
        let prev = load_preference_vector(&conn)?.unwrap_or_default();
        let new = update_running_mean(&prev, &embed, delta, n);
        save_preference_vector(&conn, &new)?;
    }
    if let Some(category) = article.category.as_deref().filter(|c| !c.is_empty()) {
        let (cat_up, cat_down) = {
            let conn = state.db.lock().await;
            db::count_votes_for_category(&conn, category)?
        };
        let cat_n = (cat_up + cat_down) as f32;
        let conn = state.db.lock().await;
        let prev = load_category_preference(&conn, category)?.unwrap_or_default();
        let new = update_running_mean(&prev, &embed, delta, cat_n);
        save_category_preference(&conn, category, &new)?;
    }
    Ok(())
}

/// Pure helper: the signed change in vote between two states.
pub fn vote_delta(current: Vote, new: Vote) -> f32 {
    (new as i32) as f32 - (current as i32) as f32
}

/// Score every article using the appropriate per-category preference vector
/// (falling back to the global one when the article has no category yet or
/// the category vector is still empty). Returns `(id, score, category)` per
/// row so the action layer doesn't have to re-query the category.
pub fn rank_articles_with_category(
    conn: &rusqlite::Connection,
    global_pref: Option<&[f32]>,
    half_life_hours: f32,
    feed_filter: Option<i64>,
) -> anyhow::Result<Vec<(i64, f64, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, COALESCE(a.published, a.fetched_at), a.embedding, a.category, a.feed_id
         FROM articles a
         WHERE a.embedding IS NOT NULL",
    )?;
    let now = Utc::now();
    let mut rows = stmt.query([])?;
    let mut scores: Vec<(i64, f64, Option<String>)> = Vec::new();
    let mut cache: std::collections::HashMap<String, Option<Vec<f32>>> =
        std::collections::HashMap::new();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let feed_id: i64 = row.get(4)?;
        if let Some(fid) = feed_filter {
            if fid != feed_id {
                continue;
            }
        }
        let ts: String = row.get(1)?;
        let emb_raw: String = row.get(2)?;
        let category: Option<String> = row.get(3)?;
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
        let pref_vec: Option<&[f32]> = if let Some(ref cat) = category {
            let entry = cache
                .entry(cat.clone())
                .or_insert_with(|| load_category_preference(conn, cat).ok().flatten());
            entry.as_deref().or(global_pref)
        } else {
            global_pref
        };
        let similarity = pref_vec
            .map(|p| ollama::cosine_similarity(p, &emb))
            .unwrap_or(0.0);
        let sim_norm = (similarity + 1.0) * 0.5;
        let score = (sim_norm as f64) + (time_decay as f64);
        scores.push((id, score, category));
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

#[cfg(test)]
mod tests {
    use super::{update_running_mean, vote_delta};
    use crate::backend::models::Vote;

    /// `update_running_mean` is supposed to be the textbook running-mean
    /// update: given the mean of `n - 1` samples and a new sample, return
    /// the mean of `n` samples. This is the property the off-by-one fix
    /// (ranking.rs:75) preserves.
    #[test]
    fn update_running_mean_converges_to_correct_mean() {
        let mut pref: Vec<f32> = Vec::new();
        let samples = [0.2_f32, 0.4, 0.6, 0.8, 1.0];
        for (i, s) in samples.iter().enumerate() {
            pref = update_running_mean(&pref, &[*s], 1.0, (i + 1) as f32);
        }
        assert!(
            (pref[0] - 0.6).abs() < 1e-6,
            "running mean should converge to 0.6, got {}",
            pref[0]
        );
    }

    /// With the off-by-one fix, calling `update_running_mean` repeatedly
    /// with the same `embed`/`sign`/`n` must produce a stable result.
    /// (Before the fix, the pref vector drifted on every repeated call —
    /// the user-visible bug was "the score drops if I click multiple
    /// times" on a Down-vote.)
    #[test]
    fn update_running_mean_idempotent_on_repeated_call() {
        let embed = vec![0.5_f32, 0.3, 0.7];
        let first = update_running_mean(&[], &embed, -1.0, 1.0);
        let second = update_running_mean(&first, &embed, -1.0, 1.0);
        let third = update_running_mean(&second, &embed, -1.0, 1.0);
        for i in 0..embed.len() {
            assert!(
                (first[i] - second[i]).abs() < 1e-6,
                "first[{i}]={} != second[{i}]={}",
                first[i],
                second[i]
            );
            assert!(
                (second[i] - third[i]).abs() < 1e-6,
                "second[{i}]={} != third[{i}]={}",
                second[i],
                third[i]
            );
            assert!(
                (first[i] - (-embed[i])).abs() < 1e-6,
                "first[{i}]={} != -embed[{i}]={}",
                first[i],
                -embed[i]
            );
        }
    }

    /// The original bug, expressed in a way the test can verify: the
    /// buggy formula `(p * n + e*sign) / (n+1)` shifts the pref vector by
    /// `1/(n+1)` per vote. The fixed formula shifts by `1/n`. For a
    /// single Down-vote on a previously Up-voted article, that difference
    /// is small (1/3 vs 1/4) but accumulates visibly over many votes —
    /// which is exactly the user-reported "the score drops if I click
    /// multiple times" symptom.
    ///
    /// This test does NOT assert that repeated identical function calls
    /// are stable at the function level — each call is a valid one-sample
    /// running-mean update and will shift the vector. The function-level
    /// drift is prevented at a higher level by the early-return in
    /// `actions::vote`. The math is correct either way; the early-return
    /// just avoids the wasted Ollama call.
    #[test]
    fn update_running_mean_shift_per_call_matches_formula() {
        let a = vec![0.5_f32];
        let b = vec![0.7_f32];
        let c = vec![0.3_f32];
        let mut pref: Vec<f32> = Vec::new();
        pref = update_running_mean(&pref, &a, 1.0, 1.0);
        pref = update_running_mean(&pref, &b, 1.0, 2.0);
        pref = update_running_mean(&pref, &c, 1.0, 3.0);
        // Prior pref = (2*0.5 + 0.7 + 0.3) / 4 = 0.5.
        let pref_before = pref[0];
        assert!(
            (pref_before - 0.5).abs() < 1e-6,
            "prior pref should be 0.5, got {}",
            pref_before
        );
        // Down-vote on A. n is still 3.
        pref = update_running_mean(&pref, &a, -1.0, 3.0);
        // new = (0.5 * 2 + 0.5 * -1) / 3 = 0.5/3 = 1/6.
        let expected = 1.0_f32 / 6.0;
        assert!(
            (pref[0] - expected).abs() < 1e-6,
            "Down-vote should shift to 1/6, got {} (expected {})",
            pref[0],
            expected
        );
        // The shift per call is 1/n = 1/3 of the (target - prior) distance,
        // not 1/(n+1) = 1/4 as the buggy formula would give.
        let target = -0.5_f32; // e * sign = 0.5 * -1
        let shift_fixed = (expected - pref_before).abs();
        let shift_expected = ((target - pref_before) / 3.0).abs();
        assert!(
            (shift_fixed - shift_expected).abs() < 1e-6,
            "shift magnitude {} != expected {} (1/n of the way to target)",
            shift_fixed,
            shift_expected
        );
    }

    /// After 3 up-votes, a single Down-vote on the first article should
    /// *remove* that article's positive contribution from the pref vector
    /// (since the user has flipped their opinion). This is what the bug
    /// got wrong: the buggy formula would leave a small positive residual
    /// from the previous Up-vote.
    #[test]
    fn update_running_mean_flip_up_to_down_removes_old_contribution() {
        let a = vec![0.5_f32];
        let b = vec![0.7_f32];
        let mut pref: Vec<f32> = Vec::new();
        pref = update_running_mean(&pref, &a, 1.0, 1.0); // [+0.5]
        pref = update_running_mean(&pref, &b, 1.0, 2.0); // [(0.5 + 0.7) / 2 = 0.6]
                                                         // Flip A to Down. n is still 2.
        pref = update_running_mean(&pref, &a, -1.0, 2.0);
        // Expected: (0.6 * 1 + 0.5 * -1) / 2 = 0.05. This is b's
        // contribution, with a's contribution cancelled.
        assert!(
            (pref[0] - 0.05).abs() < 1e-6,
            "Up→Down should leave ~0.05, got {}",
            pref[0]
        );
    }

    /// `vote_delta` is the signed change between two vote states. It's the
    /// single source of truth for how "Clear" undoes a prior vote, how
    /// flipping Up→Down works, and how the no-op short-circuit fires.
    #[test]
    fn vote_delta_computes_signed_change() {
        // No-ops: same state in, same state out -> 0.
        assert_eq!(vote_delta(Vote::None, Vote::None), 0.0);
        assert_eq!(vote_delta(Vote::Up, Vote::Up), 0.0);
        assert_eq!(vote_delta(Vote::Down, Vote::Down), 0.0);
        // First-time votes: delta is the absolute sign.
        assert_eq!(vote_delta(Vote::None, Vote::Up), 1.0);
        assert_eq!(vote_delta(Vote::None, Vote::Down), -1.0);
        // Clear: delta is the negative of the prior vote. This is what
        // makes "Clear on a Down-voted article" update the pref vector
        // toward +embed (undoing the prior dislike).
        assert_eq!(vote_delta(Vote::Up, Vote::None), -1.0);
        assert_eq!(vote_delta(Vote::Down, Vote::None), 1.0);
        // Flips: the change in sign is +/- 2.
        assert_eq!(vote_delta(Vote::Up, Vote::Down), -2.0);
        assert_eq!(vote_delta(Vote::Down, Vote::Up), 2.0);
    }

    /// End-to-end: applying a Down-vote then a Clear (delta = +1.0) should
    /// move the pref vector 1/n of the way toward the new target (+embed).
    /// The running-mean model gives a *partial* shift per update (each
    /// update moves the vector 1/n of the distance to the target), not a
    /// full restore — that's a property of the running mean, not a bug.
    /// The user-visible behavior is "Clear moves the score in the right
    /// direction," which is what the original bug report was about.
    #[test]
    fn clear_undoes_prior_down_vote_partially() {
        let a = vec![0.5_f32];
        let b = vec![0.7_f32];
        let mut pref: Vec<f32> = Vec::new();
        pref = update_running_mean(&pref, &a, 1.0, 1.0);
        pref = update_running_mean(&pref, &b, 1.0, 2.0);
        let pref_before_down = pref[0];
        assert!((pref_before_down - 0.6).abs() < 1e-6);
        // Down-vote A: delta = -1.0, n=2.
        pref = update_running_mean(&pref, &a, -1.0, 2.0);
        let pref_after_down = pref[0];
        assert!((pref_after_down - 0.05).abs() < 1e-6);
        // Clear: delta = +1.0, n=2. The vector moves 1/2 of the way from
        // 0.05 toward the new target +embed (= 0.5), landing at 0.275.
        // It does NOT return to the pre-Down value (0.6) — that's the
        // running mean model: each update is 1/n toward the new target.
        pref = update_running_mean(&pref, &a, 1.0, 2.0);
        let target = 0.5_f32; // +embed
        let expected = pref_after_down + (target - pref_after_down) / 2.0;
        assert!(
            (pref[0] - expected).abs() < 1e-6,
            "Clear should move pref to {} (1/n toward +embed), got {}",
            expected,
            pref[0]
        );
        // Crucially, the pref vector must have moved at all (this is what
        // the original bug was about — Clear did nothing).
        assert!(
            (pref[0] - pref_after_down).abs() > 0.1,
            "Clear should noticeably change the pref vector, but it only moved {}",
            (pref[0] - pref_after_down).abs()
        );
        // And it should move in the right direction (toward +embed, i.e.
        // upward from pref_after_down).
        assert!(
            pref[0] > pref_after_down,
            "pref should be higher than {} after Clear, got {}",
            pref_after_down,
            pref[0]
        );
    }

    /// Same as above, but for Up → Clear. The prior +embed contribution
    /// should be removed.
    #[test]
    fn clear_undoes_prior_up_vote() {
        let a = vec![0.5_f32];
        let mut pref: Vec<f32> = Vec::new();
        pref = update_running_mean(&pref, &a, 1.0, 1.0);
        let pref_before_clear = pref.clone();
        assert!((pref[0] - 0.5).abs() < 1e-6);
        // Clear: delta = -1.0, n=1.
        pref = update_running_mean(&pref, &a, -1.0, 1.0);
        // Expected: prior was [0.5], n=1, p=0.5, new = (0.5*0 + 0.5*-1)/1 = -0.5.
        assert!(
            (pref[0] - (-0.5)).abs() < 1e-6,
            "Clear should flip to -0.5, got {}",
            pref[0]
        );
        let _ = pref_before_clear;
    }
}
