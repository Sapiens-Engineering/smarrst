//! Background refresh loop and its supporting helpers.
//!
//! The UI spawns a single long-lived future that ticks on a configurable
//! interval and re-runs the same pipeline the manual "Refresh all" button
//! uses (RSS fetch + content extraction + embedding + classification).
//! One slow feed never blocks the others, and the manual button can't
//! stack a second pipeline on top of an in-flight one.

use crate::backend::models::{Article, Feed};
use crate::backend::{actions, db, AppState};
use chrono::{DateTime, Utc};
use dioxus::prelude::WritableExt;
use futures::stream::StreamExt;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task;

/// UI-facing snapshot of the background loop's last known state.
#[derive(Debug, Clone, Default)]
pub enum BackgroundStatus {
    /// Either the loop has not run yet, or the last run completed
    /// successfully without fetching any new articles.
    #[default]
    Idle,
    /// The loop's most recent run successfully fetched at least one
    /// new article (or at least one feed's content was updated).
    IdleWithUpdate { last_refresh: DateTime<Utc> },
    /// The loop is currently fetching / embedding / classifying.
    Refreshing,
    /// The loop's most recent run failed. The user-visible string is
    /// shown in the status pill.
    Error { message: String },
}

impl BackgroundStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Idle => "Background refresh: idle".to_string(),
            Self::IdleWithUpdate { last_refresh } => {
                let secs = (Utc::now() - *last_refresh).num_seconds().max(0);
                format!("Updated {} ago", humanize_ago(secs))
            }
            Self::Refreshing => "⟳ Updating…".to_string(),
            Self::Error { message } => format!("Background refresh error: {message}"),
        }
    }
}

fn humanize_ago(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// `true` when the background loop should not run on the next tick.
pub fn is_disabled(minutes: u32) -> bool {
    minutes == 0
}

/// Outcome of a single RSS-refresh pass over every feed. Counts are
/// feed-level and article-level totals; the caller uses these to decide
/// what to show in the UI ("Updated X new articles" vs. "Up to date").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshSummary {
    pub feeds_total: usize,
    pub feeds_succeeded: usize,
    pub feeds_failed: usize,
    pub new_articles: usize,
}

/// Refresh every feed in the database, with bounded concurrency. A
/// per-feed failure is logged and counted but does not abort the rest.
/// Returns a summary so the caller can show a meaningful status message
/// and decide whether to refresh the UI.
pub async fn concurrent_refresh_all(
    state: &AppState,
    concurrency: usize,
) -> anyhow::Result<RefreshSummary> {
    let feeds = list_feeds(state).await?;
    let state_for_fetcher = state.clone();
    Ok(concurrent_refresh_with(feeds, concurrency, move |id| {
        let state = state_for_fetcher.clone();
        async move { crate::backend::rss::refresh_feed(&state, id).await }
    })
    .await)
}

/// Dispatch a refresh for each feed in parallel, up to `concurrency`
/// in-flight. The `fetcher` closure is called once per feed; its
/// return value is the number of new articles inserted for that feed.
/// This is the testable core of `concurrent_refresh_all`, factored out
/// so tests can inject a synthetic fetcher (no real network, no SSRF
/// guard in the way) and exercise the dispatch / isolation / counting
/// logic in isolation.
pub async fn concurrent_refresh_with<F, Fut>(
    feeds: Vec<Feed>,
    concurrency: usize,
    fetcher: F,
) -> RefreshSummary
where
    F: Fn(i64) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<usize>>,
{
    let total = feeds.len();
    if total == 0 {
        return RefreshSummary::default();
    }
    let mut stream = futures::stream::iter(feeds.into_iter().map(|f| {
        let id = f.id;
        let title = f.title;
        let fut = fetcher(id);
        async move {
            let result = fut.await;
            (id, title, result)
        }
    }))
    .buffer_unordered(concurrency.max(1));
    let mut succeeded = 0usize;
    let mut failed = 0usize;
    let mut new_articles = 0usize;
    while let Some((id, title, result)) = stream.next().await {
        match result {
            Ok(n) => {
                succeeded += 1;
                new_articles += n;
            }
            Err(e) => {
                failed += 1;
                log::warn!("refresh feed {id} ({title}) failed: {e}");
            }
        }
    }
    RefreshSummary {
        feeds_total: total,
        feeds_succeeded: succeeded,
        feeds_failed: failed,
        new_articles,
    }
}

/// Fetch the original URL for a single article, extract the main text,
/// embed, then classify. This is the same sequence the manual "Refresh
/// all" button runs. Each step's errors are logged and swallowed so
/// that a single bad Ollama response doesn't poison the whole pass.
pub async fn full_pipeline(state: &AppState) -> anyhow::Result<RefreshSummary> {
    let summary = concurrent_refresh_all(state, 4).await?;
    // The fetch/embed/classify steps all have their own internal error
    // handling; we don't propagate their `Result` upwards, only log
    // upstream failures (Ollama down, DB locked, etc.).
    if let Err(e) = actions::fetch_pending_content(state, 256).await {
        log::warn!("pipeline: fetch_pending_content failed: {e}");
    }
    if let Err(e) = crate::backend::ranking::embed_pending(state, 256).await {
        log::warn!("pipeline: embed_pending failed: {e}");
    }
    if let Err(e) = actions::classify_pending(state, 256).await {
        log::warn!("pipeline: classify_pending failed: {e}");
    }
    Ok(summary)
}

/// Try to claim the right to run a refresh pipeline. Returns `true` if
/// the caller now owns the right and must call `release_refresh_lock`
/// when done. Returns `false` if a refresh is already in flight, in
/// which case the caller should skip the run.
pub fn try_acquire_refresh_lock(flag: &std::sync::atomic::AtomicBool) -> bool {
    !flag.swap(true, Ordering::SeqCst)
}

pub fn release_refresh_lock(flag: &std::sync::atomic::AtomicBool) {
    flag.store(false, Ordering::SeqCst);
}

/// Run the background refresh loop forever. The future is cancelled
/// when the Dioxus runtime drops (app close), so there's no explicit
/// shutdown path. The interval is re-read from the settings on every
/// iteration, so changing it in the Settings dialog takes effect on
/// the next tick (no app restart).
pub async fn run_background_loop(
    state: AppState,
    mut status: dioxus::prelude::Signal<BackgroundStatus>,
    articles: dioxus::prelude::Signal<Vec<Article>>,
    category_counts: dioxus::prelude::Signal<Vec<(String, i64, i64)>>,
) {
    loop {
        let minutes = {
            let s = state.settings.lock().await;
            s.background_refresh_minutes
        };
        let sleep_for = if is_disabled(minutes) {
            // When disabled, re-check the setting every minute so
            // re-enabling takes effect quickly without restarting.
            Duration::from_secs(60)
        } else {
            Duration::from_secs(u64::from(minutes) * 60)
        };
        tokio::time::sleep(sleep_for).await;

        if is_disabled(minutes) {
            continue;
        }
        if !try_acquire_refresh_lock(&state.refresh_running) {
            // A manual refresh is already running; skip this tick. The
            // next tick will try again.
            continue;
        }
        *status.write() = BackgroundStatus::Refreshing;
        let result = full_pipeline(&state).await;
        release_refresh_lock(&state.refresh_running);

        match result {
            Ok(summary) => {
                if summary.feeds_failed == 0 && summary.new_articles == 0 {
                    // Nothing changed. We don't have a "last_refresh"
                    // timestamp worth surfacing until the loop actually
                    // fetches something. Leave status as `Idle`.
                    *status.write() = BackgroundStatus::Idle;
                } else {
                    *status.write() = BackgroundStatus::IdleWithUpdate {
                        last_refresh: Utc::now(),
                    };
                }
                // Re-rank and recount after the pipeline. We use the
                // current settings' half-life so the list reflects the
                // latest decay value if the user changed it in Settings.
                refresh_ui_lists(&state, articles, category_counts).await;
                if summary.feeds_failed > 0 {
                    log::warn!(
                        "background refresh: {}/{} feeds failed",
                        summary.feeds_failed,
                        summary.feeds_total
                    );
                }
                if summary.new_articles > 0 {
                    log::info!(
                        "background refresh: +{} new articles across {}/{} feeds",
                        summary.new_articles,
                        summary.feeds_succeeded,
                        summary.feeds_total
                    );
                }
            }
            Err(e) => {
                log::warn!("background refresh failed: {e}");
                *status.write() = BackgroundStatus::Error {
                    message: e.to_string(),
                };
            }
        }
    }
}

async fn list_feeds(state: &AppState) -> anyhow::Result<Vec<Feed>> {
    let db = state.db.clone();
    task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        db::list_feeds(&conn)
    })
    .await?
}

async fn refresh_ui_lists(
    state: &AppState,
    mut articles: dioxus::prelude::Signal<Vec<Article>>,
    mut category_counts: dioxus::prelude::Signal<Vec<(String, i64, i64)>>,
) {
    let half_life = {
        let s = state.settings.lock().await;
        s.time_half_life_hours
    };
    if let Ok(list) = actions::ranked_articles(state, None, half_life).await {
        *articles.write() = list;
    }
    if let Ok(counts) = actions::category_counts(state).await {
        *category_counts.write() = counts;
    }
}

/// Mutex used in tests to serialize access to the `refresh_running`
/// flag without going through `AppState`. The production code path
/// uses `Arc<AtomicBool>` directly; this alias is here so tests can
/// `use` the same name.
#[allow(dead_code)]
pub type RefreshLock = Arc<Mutex<bool>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_disabled_zero() {
        assert!(is_disabled(0));
    }

    #[test]
    fn is_disabled_nonzero() {
        assert!(!is_disabled(1));
        assert!(!is_disabled(15));
        assert!(!is_disabled(u32::MAX));
    }

    #[test]
    fn humanize_ago_formats_all_buckets() {
        assert_eq!(humanize_ago(0), "0s");
        assert_eq!(humanize_ago(59), "59s");
        assert_eq!(humanize_ago(60), "1m");
        assert_eq!(humanize_ago(3599), "59m");
        assert_eq!(humanize_ago(3600), "1h");
        assert_eq!(humanize_ago(86_399), "23h");
        assert_eq!(humanize_ago(86_400), "1d");
    }

    #[test]
    fn refresh_summary_default_is_zero() {
        let s = RefreshSummary::default();
        assert_eq!(s.feeds_total, 0);
        assert_eq!(s.feeds_succeeded, 0);
        assert_eq!(s.feeds_failed, 0);
        assert_eq!(s.new_articles, 0);
    }

    #[test]
    fn background_status_label_covers_all_variants() {
        let now = Utc::now();
        assert_eq!(BackgroundStatus::Idle.label(), "Background refresh: idle");
        assert!(BackgroundStatus::IdleWithUpdate { last_refresh: now }
            .label()
            .starts_with("Updated "));
        assert_eq!(BackgroundStatus::Refreshing.label(), "⟳ Updating…");
        assert!(BackgroundStatus::Error {
            message: "boom".to_string()
        }
        .label()
        .contains("boom"));
    }
}
