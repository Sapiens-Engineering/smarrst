# smarrst

Smart RSS reader with AI-powered preference learning. Pure Rust, Tauri/wry-based desktop UI, Ollama for embeddings.

Smarrst is a small, opinionated feed reader. You add RSS feeds, the app fetches and embeds the articles through a local Ollama model, and you vote each article up or down. The running vote history becomes a *preference vector*; new articles are ranked by cosine similarity to that vector plus a time decay so fresher things float up unless you consistently vote them down. Articles are also auto-classified into one of a fixed list of categories (AI, Cryptography, Philosophy, Psychology, Tech, Politics, Science, Business, Culture, Sports, Gaming, Other), and the app keeps a *separate* preference vector per category so your votes on, say, Philosophy articles don't bleed into your AI rankings.

## Highlights

- **Pure Rust stack.** RSS parsing, persistence, networking, UI — Rust end to end. The UI is [Dioxus 0.7](https://dioxuslabs.com) running on the same wry/tao stack Tauri uses.
- **Local-first.** All data lives in a single SQLite file under your platform's data directory. No accounts, no servers.
- **Ollama for embeddings + classification.** The app calls Ollama's `/api/embeddings` endpoint for each article and the `/api/chat` endpoint to assign a category. The preference vector is updated with a running weighted mean, so it learns from every vote.
- **Per-category preference vectors.** A single global preference vector is great for "I like tech news" but terrible for "I love cooking *and* psychology but not politics". smarrst keeps a separate pref vector per category and falls back to the global one when a category is new.
- **Full-content extraction.** When an RSS feed only includes a teaser (e.g. Lobsters' `<p><a>Comments</a></p>`), smarrst fetches the article URL in the background and runs it through Mozilla Readability to extract the main text. The extracted text is what gets embedded, classified, and what you read.
- **Markdown rendering with image + link support.** Article bodies are converted to Markdown (preserving images, links, code blocks, lists) and rendered in the view pane. `<script>`, `<style>`, `<noscript>`, `<template>`, and `<iframe>` are stripped at conversion time, so JSON-LD metadata embedded in feeds (Medium, MS Research) doesn't leak into the view.
- **Time decay ranking.** Each article's score is `cosine_similarity(article, category_or_global_pref) + 0.5^(age_hours / half_life)`. Default half-life is one week; tune in Settings.

## Architecture

```
src/
├── main.rs                # Binary entry point — initializes state, launches Dioxus
├── lib.rs                 # Library entry point — re-exports backend for tests
├── backend/
│   ├── mod.rs             # AppState (DB + HTTP client + settings)
│   ├── db.rs              # SQLite schema, queries, migration
│   ├── models.rs          # Feed, Article, Vote, ContentStatus, Settings, DEFAULT_CATEGORIES
│   ├── rss.rs             # feed-rs based fetching
│   ├── content.rs         # Fetch original URL + Readability extraction + Markdown conversion
│   ├── ollama.rs          # Embeddings, chat-based classify, ping, cosine similarity
│   ├── ranking.rs         # Score computation, per-category pref vectors, preference update
│   ├── settings.rs        # Persisted user settings
│   └── actions.rs         # Public API the UI calls (all async, spawn_blocking SQLite)
└── ui/
    ├── mod.rs             # Dioxus components (App, ArticleListItem, CategoryItem, dialogs, ...)
    ├── markdown.rs        # pulldown-cmark event stream → Dioxus elements
    └── context.rs         # AppContext provider
assets/
└── styles.css             # Dark UI styling, category pills
tests/
├── backend.rs             # 20 unit tests for DB, settings, ranking, content, categories
└── e2e_ollama.rs          # End-to-end tests against a real local Ollama
```

### Content pipeline

When a feed is added or refreshed:

1. RSS parser extracts every item (title, link, summary, `<content:encoded>`).
2. Each article is inserted with `content_status = 'none'`, `category = NULL`.
3. A background task walks the new articles. For each one whose stored body is "thin" (under 400 chars of visible text, or more than 1/3 of the raw HTML is `<a>` tags — Lobsters' "Comments" stub, for example), the article URL is fetched with the shared `reqwest::Client` and run through [`readability`](https://crates.io/crates/readability) (Mozilla's algorithm). The extracted text replaces the `content` column, `content_status` flips to `loaded`, and the article is re-embedded.
4. After the embedding step, a second background pass calls Ollama's `/api/chat` endpoint with a strict single-label prompt. The response is parsed and stored in the `category` column.
5. The ranking view picks up the new embedding + category on the next refresh.

The article view in the UI also has a **Refresh content** button for forcing a re-extraction (e.g. if the server was down the first time).

### Categories

The default category list lives in `Settings::category_labels` (initially `["AI", "Cryptography", "Philosophy", "Psychology", "Tech", "Politics", "Science", "Business", "Culture", "Sports", "Gaming", "Other"]`). The classifier is a single chat-model call per article with a system prompt that lists the allowed labels and instructs the model to respond with *exactly one* of them. The response is then defensively normalized: we walk all whitespace-separated tokens, strip non-alphanumeric characters, and return the first one that matches a known label. Anything that doesn't match falls back to `"Other"`.

Each category has its own preference vector in the `settings` table under the key `pref_vec:<Category>`. When a vote is cast on an article, **both** the global vector and the article's category vector are updated with the same running weighted mean, weighted by their respective vote counts. A new category therefore starts with no signal and uses the global vector as a fallback until it has a few votes of its own.

### Ranking

The score for each article is:

```
score = ((cosine_similarity(article_embedding, category_or_global_pref) + 1) / 2) + 0.5^(age_hours / half_life)
```

- The preference vector used is the article's category's vector if it exists; otherwise the global one.
- The global `preference_vector` starts as zero. The first vote seeds it with the article's embedding (signed positive for upvote, negative for downvote). Same for the per-category vector.
- Each subsequent vote updates the running mean: `new_pref = (old_pref * (n - 1) ± embed) / n` where `n` is the total number of votes cast in that vector's scope (global or category).
- Time decay uses an exponential half-life so older articles fade out gradually.
- When you have no votes yet, all articles tie on similarity (0) and only time decay differentiates them — newest first.

## Setup

1. **Install Ollama** and start the server. The default is `http://localhost:11434`.

   ```sh
   brew install ollama
   ollama serve   # in a separate terminal
   ```

2. **Pull the models.** The default embedding model is `nomic-embed-text` and the default chat model is `llama3.2`:

   ```sh
   ollama pull nomic-embed-text
   ollama pull llama3.2
   ```

   Any model that supports `/api/embeddings` works for embedding. For classification, a 1B–3B chat model is fast and accurate enough for single-label tasks.

3. **Build and run.**

   ```sh
   cargo run --release
   ```

   The release binary lands at `target/release/smarrst`.

## Using the app

- Click **Add feed**, paste an RSS/Atom URL, hit **Add**. The feed is fetched immediately; the new articles are then walked in the background to fetch their full content (when the RSS body is just a teaser), compute embeddings, and assign categories.
- Click **Refresh all** to re-fetch every feed (and re-run the content/embedding/classification pipeline for new items).
- Click any article in the middle column to read it. If the body is still being fetched from the web you'll see a "Fetching full content..." status; it will refresh in place once the extraction is done. Use the **Refresh content** button to force a re-extraction.
- Use the **Up** / **Down** buttons to vote; both the global and the article's category preference vectors are updated in the background. Re-ranking happens automatically after every vote.
- The **Categories** section in the left sidebar lists every category that has at least one article, with its unread count. Click a category to filter the article list to that topic. Categories and feeds are orthogonal — you can combine a feed filter with a category filter.
- Use **Settings** to change the Ollama URL, the embedding model, the chat model, or the time-decay half-life. The **Test Ollama** button pings the server.
- Click the **×** next to a feed to remove it. Cascade-deletes all its articles and votes.

## Data location

The SQLite database lives at:

| Platform | Path |
|----------|------|
| macOS    | `~/Library/Application Support/smarrst/smarrst.db` |
| Linux    | `~/.local/share/smarrst/smarrst.db` |
| Windows  | `%APPDATA%\smarrst\smarrst.db` |

Delete the file to reset the app to a clean state.

## Development

```sh
# Run with hot-reload-ish behavior (the dev build prints log lines):
cargo run

# Run unit + integration tests:
cargo test

# Run only the end-to-end test that talks to Ollama:
cargo test --test e2e_ollama -- --nocapture

# Format, lint:
cargo fmt --all
cargo clippy --all --all-targets -- -Dwarnings
```

The e2e test is skipped automatically when Ollama is not reachable on `localhost:11434`, so CI without Ollama still passes.

## Configuration

The defaults in Settings are loaded the first time the app starts:

| Key | Default | Notes |
|-----|---------|-------|
| Ollama URL | `http://localhost:11434` | |
| Embedding model | `nomic-embed-text` | Any model with `/api/embeddings` works |
| Chat model | `llama3.2` | Used for per-article category classification |
| Time-decay half-life | `168.0` (hours, ≈ 1 week) | Smaller values push newer articles up faster |
| Category labels | `AI, Cryptography, Philosophy, Psychology, Tech, Politics, Science, Business, Culture, Sports, Gaming, Other` | JSON array in the settings table; the classifier is given this list in the system prompt |

## Known limitations / next steps

- The category list is hardcoded in the binary; the settings table has a `category_labels` JSON column ready for a v2 settings UI to edit it. The classifier gracefully falls back to `"Other"` when the chat model picks a label not in the list.
- For "discussion" feeds (Hacker News, Lobsters, Reddit) the RSS item URL points to the discussion page, not the linked article. Readability extracts whatever is on the discussion page; the actual article is one click away via **Open in browser**. Following the canonical link inside the page is a v2 feature.
- The content, embedding, and classification loops are sequential inside the background task; under heavy load they would benefit from batching and a concurrency cap.
- There is no per-category time-decay knob; both the global and per-category scoring use the same `time_half_life_hours`.
- Per-category vote counts are recomputed by a SQL join on every vote (cheap; no caching needed for our scale).
- There is no pagination; long feed lists live in a scrollable container.

## License

TBD.
