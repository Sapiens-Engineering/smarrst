use crate::backend::{content, AppState};
use anyhow::Context;
use chrono::Utc;
use feed_rs::parser;

pub struct FetchedEntry {
    pub guid: String,
    pub title: String,
    pub url: String,
    pub author: Option<String>,
    pub summary: Option<String>,
    pub content: Option<String>,
    pub content_markdown: Option<String>,
    pub published: Option<chrono::DateTime<Utc>>,
}

pub struct FetchedFeed {
    pub title: String,
    pub description: Option<String>,
    pub entries: Vec<FetchedEntry>,
}

pub async fn fetch_feed(state: &AppState, url: &str) -> anyhow::Result<FetchedFeed> {
    let body = state
        .http
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .bytes()
        .await?;
    let feed = parser::parse(&body[..]).with_context(|| format!("parsing feed from {url}"))?;

    let title = feed
        .title
        .map(|t| t.content)
        .unwrap_or_else(|| url.to_string());
    let description = feed.description.map(|t| t.content);

    let entries = feed
        .entries
        .into_iter()
        .map(|e| {
            let guid = e.id.clone();
            let title = e
                .title
                .map(|t| t.content)
                .unwrap_or_else(|| "(untitled)".to_string());
            let url = e.links.first().map(|l| l.href.clone()).unwrap_or_default();
            let author = e.authors.first().map(|a| a.name.clone());
            let summary = e.summary.map(|t| t.content);
            let content = e.content.and_then(|c| c.body);
            // If the RSS body is HTML, pre-compute a Markdown copy for the
            // article view. Plain-text bodies pass through unchanged.
            let content_markdown = content.as_deref().map(|html| {
                if html.contains('<') {
                    content::html_to_markdown(html, &url)
                } else {
                    html.to_string()
                }
            });
            let published = e.published.or(e.updated);
            FetchedEntry {
                guid,
                title,
                url,
                author,
                summary,
                content,
                content_markdown,
                published,
            }
        })
        .collect();

    Ok(FetchedFeed {
        title,
        description,
        entries,
    })
}

pub async fn refresh_feed(state: &AppState, feed_id: i64) -> anyhow::Result<usize> {
    let url = {
        let conn = state.db.lock().await;
        let feed = crate::backend::db::get_feed(&conn, feed_id)?
            .ok_or_else(|| anyhow::anyhow!("feed {feed_id} not found"))?;
        feed.url
    };

    let parsed = fetch_feed(state, &url).await?;
    let count = parsed.entries.len();
    {
        let conn = state.db.lock().await;
        if let Some(desc) = &parsed.description {
            conn.execute(
                "UPDATE feeds SET description = ?1 WHERE id = ?2",
                rusqlite::params![desc, feed_id],
            )?;
        }
        for e in parsed.entries {
            let _ = crate::backend::db::insert_article(
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
        crate::backend::db::mark_feed_fetched(&conn, feed_id)?;
    }
    Ok(count)
}
