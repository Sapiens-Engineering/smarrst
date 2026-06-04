use crate::backend::models::Article;
use crate::ui::context::{use_app_state, AppContext};
use dioxus::desktop::{use_window, LogicalSize};
use dioxus::prelude::*;

use crate::backend;

pub mod context;
pub mod markdown;

fn category_slug(c: &str) -> String {
    c.to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

#[component]
pub fn App() -> Element {
    let feeds = use_signal(Vec::<crate::backend::models::Feed>::new);
    let articles = use_signal(Vec::<Article>::new);
    let mut selected_feed = use_signal(|| None::<i64>);
    let mut selected_article = use_signal(|| None::<i64>);
    let mut selected_category = use_signal(|| None::<String>);
    let mut show_add_feed = use_signal(|| false);
    let mut show_settings = use_signal(|| false);
    let mut status_message = use_signal(|| "Loading...".to_string());
    let mut refreshing = use_signal(|| false);
    let category_counts = use_signal(Vec::<(String, i64, i64)>::new);

    let _app: AppContext = use_context();
    use_effect(move || {
        let state = use_app_state();
        spawn(async move {
            refresh_feeds_list(state.clone(), feeds, status_message).await;
            refresh_articles(
                state.clone(),
                selected_feed(),
                selected_category(),
                articles,
                status_message,
            )
            .await;
            refresh_category_counts(state.clone(), category_counts, status_message).await;

            // The fetch/embed/classify pipeline is normally kicked off by
            // add_feed / refresh_all, but on a fresh launch any articles
            // already in the database (e.g. from before the category
            // feature shipped) would sit with `category = NULL` forever.
            //
            // Classify runs FIRST because it only needs title + summary
            // (no content fetch, no embed). Running it before the
            // network-heavy fetch step means the Categories sidebar
            // becomes visible within seconds rather than after several
            // minutes of article fetching.
            *status_message.write() = "Classifying existing articles…".to_string();
            if let Err(e) = backend::actions::classify_pending(&state, 256).await {
                log::warn!("startup classify failed: {e}");
            }
            // Show categories in the UI as soon as classify finishes.
            let cat = selected_category();
            refresh_articles(
                state.clone(),
                selected_feed(),
                cat.clone(),
                articles,
                status_message,
            )
            .await;
            refresh_category_counts(state.clone(), category_counts, status_message).await;
            *status_message.write() = "Categories ready.".to_string();

            // Fetch + embed can finish in the background. They update
            // embeddings (and content for thin RSS bodies) but don't
            // affect the category sidebar.
            if let Err(e) = backend::actions::fetch_pending_content(&state, 256).await {
                log::warn!("startup content fetch failed: {e}");
            }
            if let Err(e) = backend::ranking::embed_pending(&state, 256).await {
                log::warn!("startup embedding failed: {e}");
            }
            refresh_articles(state, selected_feed(), cat, articles, status_message).await;
        });
    });

    use_effect(move || {
        let window = use_window();
        if let Some(monitor) = window.window.primary_monitor() {
            let size = monitor.size();
            let scale = monitor.scale_factor();
            let logical_w = size.width as f64 / scale;
            let logical_h = size.height as f64 / scale;
            let target_w = (logical_w * 2.0 / 3.0).round().max(640.0);
            let target_h = (logical_h * 0.85).round().max(480.0);
            window
                .window
                .set_inner_size(LogicalSize::new(target_w, target_h));
        }
    });

    rsx! {
        style { {include_str!("../../assets/styles.css")} }
        div { class: "app",
            header { class: "topbar",
                h1 { "smarrst" }
                div { class: "topbar-actions",
                    button {
                        class: "btn btn-primary",
                        disabled: refreshing(),
                        onclick: move |_| {
                            let state = use_app_state();
                            spawn(async move {
                                *refreshing.write() = true;
                                *status_message.write() = "Refreshing feeds…".to_string();
                                match backend::actions::refresh_all(&state).await {
                                    Ok(n) => {
                                        *status_message.write() = format!("Refreshed {n} new articles");
                                        let cat = selected_category();
                                        refresh_articles(state.clone(), selected_feed(), cat, articles, status_message).await;
                                        refresh_category_counts(state.clone(), category_counts, status_message).await;
                                    }
                                    Err(e) => {
                                        *status_message.write() = format!("Refresh failed: {e}");
                                    }
                                }
                                *refreshing.write() = false;
                            });
                        },
                        if refreshing() { "Refreshing…" } else { "Refresh all" }
                    }
                    button {
                        class: "btn",
                        onclick: move |_| *show_add_feed.write() = true,
                        "Add feed"
                    }
                    button {
                        class: "btn",
                        onclick: move |_| *show_settings.write() = true,
                        "Settings"
                    }
                }
            }
            div { class: "status-bar", "{status_message()}" }
            div { class: "main",
                aside { class: "sidebar",
                    h2 { "Feeds" }
                    ul { class: "feed-list",
                        li {
                            class: if selected_feed().is_none() { "feed-item active" } else { "feed-item" },
                            onclick: move |_| {
                                *selected_feed.write() = None;
                                let state = use_app_state();
                                let cat = selected_category();
                                spawn(async move {
                                    refresh_articles(state.clone(), None, cat, articles, status_message).await;
                                });
                            },
                            "All articles"
                        }
                        for f in feeds() {
                            FeedItem {
                                feed: f.clone(),
                                active: selected_feed() == Some(f.id),
                                on_select: move |id: i64| {
                                    *selected_feed.write() = Some(id);
                                    let state = use_app_state();
                                    let cat = selected_category();
                                    spawn(async move {
                                        refresh_articles(state.clone(), Some(id), cat, articles, status_message).await;
                                    });
                                },
                                on_delete: move |id: i64| {
                                    let state = use_app_state();
                                    let cat = selected_category();
                                    spawn(async move {
                                        if let Err(e) = backend::actions::delete_feed(&state, id).await {
                                            *status_message.write() = format!("Delete failed: {e}");
                                        }
                                        refresh_feeds_list(state.clone(), feeds, status_message).await;
                                        if selected_feed() == Some(id) {
                                            *selected_feed.write() = None;
                                        }
                                        refresh_articles(state.clone(), selected_feed(), cat, articles, status_message).await;
                                    });
                                },
                            }
                        }
                    }
                    h2 { class: "sidebar-section", "Categories" }
                    if category_counts().is_empty() {
                        div { class: "sidebar-empty", "Classifying articles…" }
                    } else {
                        ul { class: "category-list",
                            li {
                                class: if selected_category().is_none() { "category-item active" } else { "category-item" },
                                onclick: move |_| {
                                    *selected_category.write() = None;
                                    let state = use_app_state();
                                    let fid = selected_feed();
                                    spawn(async move {
                                        refresh_articles(state.clone(), fid, None, articles, status_message).await;
                                    });
                                },
                                "All"
                            }
                            for (name, unread, total) in category_counts() {
                                CategoryItem {
                                    key: "{name}",
                                    name: name.clone(),
                                    unread,
                                    total,
                                    active: selected_category().as_deref() == Some(name.as_str()),
                                    on_select: move |n: String| {
                                        *selected_category.write() = Some(n.clone());
                                        let state = use_app_state();
                                        let fid = selected_feed();
                                        spawn(async move {
                                            refresh_articles(state.clone(), fid, Some(n), articles, status_message).await;
                                        });
                                    },
                                }
                            }
                        }
                    }
                }
                section { class: "article-list",
                    h2 {
                        {
                            let id = selected_feed();
                            let cat = selected_category();
                            let feed_label = match id {
                                Some(id) => feeds().iter().find(|f| f.id == id).map(|f| f.title.clone()).unwrap_or_else(|| "...".to_string()),
                                None => "All articles".to_string(),
                            };
                            match cat {
                                Some(c) => format!("{feed_label} · {c}"),
                                None => feed_label,
                            }
                        }
                    }
                    if articles().is_empty() {
                        div { class: "empty", "No articles yet. Add an RSS feed and hit Refresh." }
                    } else {
                        for a in articles() {
                            ArticleListItem {
                                key: "{a.id}",
                                article: a.clone(),
                                active: selected_article() == Some(a.id),
                                on_select: move |id: i64| *selected_article.write() = Some(id),
                            }
                        }
                    }
                }
                section { class: "article-view",
                    if let Some(id) = selected_article() {
                        ArticleView {
                            key: "{id}",
                            article_id: id,
                            on_change: move |_| {
                                let state = use_app_state();
                                let cat = selected_category();
                                spawn(async move {
                                    refresh_articles(state.clone(), selected_feed(), cat, articles, status_message).await;
                                    refresh_category_counts(state.clone(), category_counts, status_message).await;
                                });
                            },
                        }
                    } else {
                        div { class: "empty", "Select an article to read it." }
                    }
                }
            }
        }
        if show_add_feed() {
            AddFeedDialog {
                on_close: move |_| *show_add_feed.write() = false,
                on_added: move |_| {
                    *show_add_feed.write() = false;
                    let state = use_app_state();
                    let cat = selected_category();
                    spawn(async move {
                        refresh_feeds_list(state.clone(), feeds, status_message).await;
                        refresh_articles(state.clone(), selected_feed(), cat, articles, status_message).await;
                    });
                },
            }
        }
        if show_settings() {
            SettingsDialog {
                on_close: move |_| *show_settings.write() = false,
            }
        }
    }
}

async fn refresh_feeds_list(
    state: crate::backend::AppState,
    mut feeds: Signal<Vec<crate::backend::models::Feed>>,
    mut status: Signal<String>,
) {
    match backend::actions::list_feeds(&state).await {
        Ok(list) => {
            *feeds.write() = list;
            *status.write() = format!("{} feed(s) loaded", feeds().len());
        }
        Err(e) => {
            *status.write() = format!("Failed to load feeds: {e}");
        }
    }
}

async fn refresh_articles(
    state: crate::backend::AppState,
    feed_filter: Option<i64>,
    category_filter: Option<String>,
    mut articles: Signal<Vec<Article>>,
    mut status: Signal<String>,
) {
    let half_life = {
        let s = state.settings.lock().await;
        s.time_half_life_hours
    };
    match backend::actions::ranked_articles(&state, feed_filter, half_life).await {
        Ok(list) => {
            let filtered: Vec<Article> = if let Some(ref c) = category_filter {
                list.into_iter()
                    .filter(|a| a.category.as_deref() == Some(c.as_str()))
                    .collect()
            } else {
                list
            };
            *status.write() = format!("{} article(s) shown", filtered.len());
            *articles.write() = filtered;
        }
        Err(e) => {
            *status.write() = format!("Ranking failed: {e}");
        }
    }
}

async fn refresh_category_counts(
    state: crate::backend::AppState,
    mut counts: Signal<Vec<(String, i64, i64)>>,
    _status: Signal<String>,
) {
    if let Ok(list) = backend::actions::category_counts(&state).await {
        *counts.write() = list;
    }
}

#[component]
fn FeedItem(
    feed: crate::backend::models::Feed,
    active: bool,
    on_select: EventHandler<i64>,
    on_delete: EventHandler<i64>,
) -> Element {
    let id = feed.id;
    let class = if active {
        "feed-item active"
    } else {
        "feed-item"
    };
    rsx! {
        li { class: "{class}",
            div {
                class: "feed-item-title",
                onclick: move |_| on_select.call(id),
                "{feed.title}"
            }
            button {
                class: "feed-item-delete",
                title: "Delete feed",
                onclick: move |_| on_delete.call(id),
                "×"
            }
        }
    }
}

#[component]
fn CategoryItem(
    name: String,
    unread: i64,
    total: i64,
    active: bool,
    on_select: EventHandler<String>,
) -> Element {
    let slug = category_slug(&name);
    let class = if active {
        "category-item active"
    } else {
        "category-item"
    };
    // When every article in the category has been read, dim the row
    // (and the dot) so the user can still see the category exists but
    // there's nothing new. Hover/active styles override.
    let all_read = unread == 0 && total > 0;
    let item_class = if all_read {
        format!("{class} all-read")
    } else {
        class.to_string()
    };
    let dot_class = if all_read {
        format!("category-dot category-{slug} dim")
    } else {
        format!("category-dot category-{slug}")
    };
    let count_label = if unread == 0 {
        format!("{total}")
    } else {
        format!("{unread}/{total}")
    };
    rsx! {
        li { class: "{item_class}", onclick: move |_| on_select.call(name.clone()),
            span { class: "{dot_class}" }
            span { class: "category-name", "{name}" }
            span { class: "category-count", "{count_label}" }
        }
    }
}

#[component]
fn ArticleListItem(article: Article, active: bool, on_select: EventHandler<i64>) -> Element {
    let id = article.id;
    let is_read = article.read_at.is_some();
    let class = match (active, is_read) {
        (true, true) => "article-list-item active read",
        (true, false) => "article-list-item active",
        (false, true) => "article-list-item read",
        (false, false) => "article-list-item",
    };
    let vote_class = match article.vote {
        1 => "vote vote-up",
        -1 => "vote vote-down",
        _ => "vote",
    };
    let pub_str = article
        .published
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string());
    let vote_glyph = match article.vote {
        1 => "^",
        -1 => "v",
        _ => ".",
    };
    rsx! {
        div { class: "{class}", onclick: move |_| on_select.call(id),
            div { class: "article-list-header",
                span { class: "{vote_class}", "{vote_glyph}" }
                h3 { class: "article-list-title", "{article.title}" }
            }
            div { class: "article-list-meta",
                span { class: "article-list-feed", "{article.feed_title}" }
                span { class: "article-list-date", "{pub_str}" }
                if let Some(d) = article.display_score {
                    span {
                        class: "article-list-score",
                        title: "AI preference rank (0–10). 10 = highest-ranked, 0 = lowest-ranked. List position may differ from rank when read articles are grouped below unread ones.",
                        "{d:.1}"
                    }
                }
                if let Some(cat) = &article.category {
                    span { class: "category-pill category-{category_slug(cat)}", "{cat}" }
                }
            }
        }
    }
}

#[component]
fn ArticleView(article_id: i64, on_change: EventHandler<()>) -> Element {
    use crate::backend::models::ContentStatus;
    let state = use_app_state();
    let article = use_signal::<Option<Article>>(|| None);
    let load_state = state.clone();
    let load_id = article_id;
    let mut article_sig = article;
    use_effect(move || {
        let state = load_state.clone();
        let id = load_id;
        article_sig.set(None);
        spawn(async move {
            let a = backend::actions::get_article(&state, id)
                .await
                .ok()
                .flatten();
            // Auto-trigger content fetch if the article's stored body is not
            // substantial and we haven't already loaded it.
            if let Some(ref art) = a {
                let needs_fetch =
                    matches!(
                        art.content_status,
                        ContentStatus::None | ContentStatus::Failed
                    ) && !backend::content::content_is_substantial(art.content.as_deref());
                if needs_fetch {
                    let _ = backend::actions::fetch_article_content(&state, id).await;
                    let a2 = backend::actions::get_article(&state, id)
                        .await
                        .ok()
                        .flatten();
                    article_sig.set(a2.clone());
                    // Auto-mark as read on first visit if not already read.
                    if a2.as_ref().is_some_and(|a| a.read_at.is_none()) {
                        let _ = backend::actions::mark_article_read(&state, id).await;
                        let a3 = backend::actions::get_article(&state, id)
                            .await
                            .ok()
                            .flatten();
                        article_sig.set(a3);
                        on_change.call(());
                    }
                    return;
                }
            }
            article_sig.set(a.clone());
            // Auto-mark as read on first visit if not already read.
            if a.as_ref().is_some_and(|a| a.read_at.is_none()) {
                let _ = backend::actions::mark_article_read(&state, id).await;
                let a2 = backend::actions::get_article(&state, id)
                    .await
                    .ok()
                    .flatten();
                article_sig.set(a2);
                on_change.call(());
            }
        });
    });

    // Allow the "Mark unread" button to refresh the local article signal so
    // the button disappears (and the list updates) without re-running the
    // load effect (which would re-mark as read). Also notifies the parent so
    // the article list's read-state CSS class updates.
    let reload_state = state.clone();
    let mut reload_sig = article;
    let on_read_changed = move |_: ()| {
        let state = reload_state.clone();
        let id = article_id;
        spawn(async move {
            if let Ok(Some(a)) = backend::actions::get_article(&state, id).await {
                reload_sig.set(Some(a));
            }
        });
        on_change.call(());
    };

    // Same as `on_read_changed` but for vote changes. The vote buttons need
    // to refresh the local article signal so the disabled state on the
    // buttons (which depends on the article's current `vote` field) updates
    // after Up/Down/Clear. Without this, clicking "Clear" on a Down-voted
    // article would clear the vote in the DB but the Down button would
    // stay disabled because the component is still rendering the stale
    // `article` prop.
    let vote_state = state.clone();
    let mut vote_sig = reload_sig;
    let on_vote_changed = move |_: ()| {
        let state = vote_state.clone();
        let id = article_id;
        spawn(async move {
            if let Ok(Some(a)) = backend::actions::get_article(&state, id).await {
                vote_sig.set(Some(a));
            }
        });
        on_change.call(());
    };

    rsx! {
        div { class: "article-view-inner",
            match article() {
                Some(a) => rsx! {
                    ArticleContent {
                        article: a,
                        on_vote: on_vote_changed,
                        on_content_changed: on_change,
                        on_read_changed: on_read_changed,
                    }
                },
                None => rsx! { div { class: "empty", "Loading..." } },
            }
        }
    }
}

#[component]
fn ArticleContent(
    article: Article,
    on_vote: EventHandler<()>,
    on_content_changed: EventHandler<()>,
    on_read_changed: EventHandler<()>,
) -> Element {
    use crate::backend::models::ContentStatus;
    let state = use_app_state();
    let up_state = state.clone();
    let down_state = state.clone();
    let clear_state = state.clone();
    let refresh_state = state.clone();
    let read_state = state.clone();
    let id = article.id;
    let vote = article.vote;
    let url = article.url.clone();
    let content_status = article.content_status;
    let is_read = article.read_at.is_some();
    let pub_str = article
        .published
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "-".to_string());

    let body = match content_status {
        ContentStatus::Loaded => article
            .content
            .clone()
            .or_else(|| article.summary.clone())
            .unwrap_or_else(|| "(no extractable text)".to_string()),
        ContentStatus::Fetching => "(fetching content from the web...)".to_string(),
        ContentStatus::None | ContentStatus::Failed => article
            .summary
            .clone()
            .unwrap_or_else(|| "(no preview available yet)".to_string()),
    };

    // Prefer the rendered Markdown (with images, links, code blocks) when
    // available; fall back to converting the raw HTML body on the fly
    // (covers articles that were saved before the markdown column existed).
    // If the stored markdown looks like it still contains JSON-LD / inline
    // JS (saved before we started skipping those tags), fall back to the
    // clean plain-text body instead of rendering the garbage.
    let markdown_source: Option<String> = article
        .content_markdown
        .as_deref()
        .filter(|md| !md.trim().is_empty())
        .filter(|md| !backend::content::markdown_looks_broken(md))
        .map(str::to_string)
        .or_else(|| {
            article
                .content
                .as_deref()
                .filter(|c| c.contains('<'))
                .map(|c| backend::content::html_to_markdown(c, &article.url))
        });
    let rendered_body: Option<Element> = markdown_source
        .as_deref()
        .map(crate::ui::markdown::render_markdown);

    rsx! {
        div { class: "article-content",
            div { class: "article-meta-row",
                span { class: "article-feed", "{article.feed_title}" }
                span { class: "article-date", "{pub_str}" }
                if let Some(a) = &article.author {
                    span { class: "article-author", "by {a}" }
                }
                if let Some(cat) = &article.category {
                    span { class: "category-pill category-{category_slug(cat)}", "{cat}" }
                }
            }
            h2 { class: "article-title", "{article.title}" }
            div { class: "article-vote-row",
                button {
                    class: if vote == 1 { "btn btn-vote active" } else { "btn btn-vote" },
                    disabled: vote == 1,
                    onclick: move |_| {
                        let state = up_state.clone();
                        let id = id;
                        spawn(async move {
                            let _ = backend::actions::vote(&state, id, 1).await;
                            on_vote.call(());
                        });
                    },
                    "Up"
                }
                button {
                    class: if vote == -1 { "btn btn-vote active" } else { "btn btn-vote" },
                    disabled: vote == -1,
                    onclick: move |_| {
                        let state = down_state.clone();
                        let id = id;
                        spawn(async move {
                            let _ = backend::actions::vote(&state, id, -1).await;
                            on_vote.call(());
                        });
                    },
                    "Down"
                }
                button {
                    class: "btn",
                    disabled: vote == 0,
                    onclick: move |_| {
                        let state = clear_state.clone();
                        let id = id;
                        spawn(async move {
                            let _ = backend::actions::vote(&state, id, 0).await;
                            on_vote.call(());
                        });
                    },
                    "Clear"
                }
                a { class: "btn", href: "{url}", target: "_blank", "Open in browser" }
                if is_read {
                    button {
                        class: "btn",
                        onclick: move |_| {
                            let state = read_state.clone();
                            let id = id;
                            let on_change = on_read_changed;
                            spawn(async move {
                                let _ = backend::actions::mark_article_unread(&state, id).await;
                                on_change.call(());
                            });
                        },
                        "Mark unread"
                    }
                }
            }
            div { class: "article-status",
                match content_status {
                    ContentStatus::Loaded => rsx! { span { class: "status-ok", "Content loaded" } },
                    ContentStatus::Fetching => rsx! { span { class: "status-info", "Fetching full content..." } },
                    ContentStatus::None => rsx! { span { class: "status-info", "No full content cached yet." } },
                    ContentStatus::Failed => rsx! { span { class: "status-err", "Failed to fetch content from the web." } },
                }
                button {
                    class: "btn btn-small",
                    disabled: matches!(content_status, ContentStatus::Fetching),
                    onclick: move |_| {
                        let state = refresh_state.clone();
                        let id = id;
                        let on_change = on_content_changed;
                        spawn(async move {
                            let _ = backend::actions::fetch_article_content(&state, id).await;
                            on_change.call(());
                        });
                    },
                    match content_status {
                        ContentStatus::Failed => "Retry fetch",
                        _ => "Refresh content",
                    }
                }
            }
            div { class: "article-body",
                if let Some(rendered) = rendered_body {
                    {rendered}
                } else {
                    p { class: "md-p", {body} }
                }
            }
        }
    }
}

#[component]
fn AddFeedDialog(on_close: EventHandler<()>, on_added: EventHandler<()>) -> Element {
    let mut url = use_signal(String::new);
    let mut error = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let state = use_app_state();

    rsx! {
        div { class: "modal-backdrop", onclick: move |_| on_close.call(()),
            div { class: "modal", onclick: move |e| e.stop_propagation(),
                h2 { "Add RSS feed" }
                label { "Feed URL" }
                input {
                    r#type: "url",
                    placeholder: "https://example.com/feed.xml",
                    value: "{url}",
                    oninput: move |e| url.set(e.value()),
                }
                if !error().is_empty() {
                    div { class: "error", "{error()}" }
                }
                div { class: "modal-actions",
                    button { class: "btn", onclick: move |_| on_close.call(()), "Cancel" }
                    button {
                        class: "btn btn-primary",
                        disabled: busy() || url().trim().is_empty(),
                        onclick: move |_| {
                            let state = state.clone();
                            let url_val = url().trim().to_string();
                            spawn(async move {
                                *busy.write() = true;
                                *error.write() = String::new();
                                match backend::actions::add_feed(&state, &url_val).await {
                                    Ok(()) => {
                                        *busy.write() = false;
                                        on_added.call(());
                                    }
                                    Err(e) => {
                                        *error.write() = format!("{e}");
                                        *busy.write() = false;
                                    }
                                }
                            });
                        },
                        if busy() { "Adding…" } else { "Add" }
                    }
                }
            }
        }
    }
}

#[component]
fn SettingsDialog(on_close: EventHandler<()>) -> Element {
    let state = use_app_state();
    let init_state = state.clone();
    let ping_state = state.clone();
    let save_state = state.clone();
    let mut ollama_url = use_signal(String::new);
    let mut embed_model = use_signal(String::new);
    let mut chat_model = use_signal(String::new);
    let mut half_life = use_signal(String::new);
    let mut error = use_signal(String::new);
    let mut ping_status = use_signal(String::new);
    let mut busy = use_signal(|| false);

    use_effect(move || {
        let state = init_state.clone();
        spawn(async move {
            let s = state.settings.lock().await.clone();
            ollama_url.set(s.ollama_url);
            embed_model.set(s.ollama_embed_model);
            chat_model.set(s.ollama_chat_model);
            half_life.set(s.time_half_life_hours.to_string());
        });
    });

    rsx! {
        div { class: "modal-backdrop", onclick: move |_| on_close.call(()),
            div { class: "modal", onclick: move |e| e.stop_propagation(),
                h2 { "Settings" }
                label { "Ollama URL" }
                input {
                    value: "{ollama_url}",
                    oninput: move |e| ollama_url.set(e.value()),
                }
                label { "Embedding model" }
                input {
                    value: "{embed_model}",
                    oninput: move |e| embed_model.set(e.value()),
                }
                label { "Chat model" }
                input {
                    value: "{chat_model}",
                    oninput: move |e| chat_model.set(e.value()),
                }
                label { "Time decay half-life (hours)" }
                input {
                    r#type: "number",
                    step: "0.5",
                    value: "{half_life}",
                    oninput: move |e| half_life.set(e.value()),
                }
                if !error().is_empty() {
                    div { class: "error", "{error()}" }
                }
                if !ping_status().is_empty() {
                    div { class: "info", "{ping_status()}" }
                }
                div { class: "modal-actions",
                    button {
                        class: "btn",
                        onclick: move |_| {
                            let state = ping_state.clone();
                            spawn(async move {
                                *ping_status.write() = "Pinging Ollama...".to_string();
                                match backend::actions::ping_ollama(&state).await {
                                    Ok(true) => *ping_status.write() = "Ollama is reachable.".to_string(),
                                    Ok(false) => *ping_status.write() = "Ollama responded with an error.".to_string(),
                                    Err(e) => *ping_status.write() = format!("Ollama unreachable: {e}"),
                                }
                            });
                        },
                        "Test Ollama"
                    }
                    button { class: "btn", onclick: move |_| on_close.call(()), "Cancel" }
                    button {
                        class: "btn btn-primary",
                        disabled: busy(),
                        onclick: move |_| {
                            let state = save_state.clone();
                            spawn(async move {
                                *busy.write() = true;
                                *error.write() = String::new();
                                let new_settings = crate::backend::models::Settings {
                                    ollama_url: ollama_url(),
                                    ollama_embed_model: embed_model(),
                                    ollama_chat_model: chat_model(),
                                    vote_weight: 1.0,
                                    time_half_life_hours: half_life().parse().unwrap_or(168.0),
                                    category_labels: crate::backend::models::DEFAULT_CATEGORIES
                                        .iter()
                                        .map(|s| s.to_string())
                                        .collect(),
                                    category_weight: 1.0,
                                };
                                match backend::actions::save_settings(&state, &new_settings).await {
                                    Ok(()) => {
                                        *busy.write() = false;
                                        on_close.call(());
                                    }
                                    Err(e) => {
                                        *error.write() = format!("{e}");
                                        *busy.write() = false;
                                    }
                                }
                            });
                        },
                        "Save"
                    }
                }
            }
        }
    }
}
