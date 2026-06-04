# smarrst

Smart RSS reader with AI-powered preference learning. Pure Rust, Tauri/wry-based desktop UI, Ollama for embeddings.

Smarrst is a small, opinionated feed reader. You add RSS feeds, the app fetches and embeds the articles through a local Ollama model, and you vote each article up or down. The running vote history becomes a *preference vector*; new articles are ranked by cosine similarity to that vector plus a time decay so fresher things float up unless you consistently vote them down.

## Highlights

- **Pure Rust stack.** RSS parsing, persistence, networking, UI — Rust end to end. The UI is [Dioxus 0.7](https://dioxuslabs.com) running on the same wry/tao stack Tauri uses.
- **Local-first.** All data lives in a single SQLite file under your platform's data directory. No accounts, no servers.
- **Ollama for embeddings.** The app calls Ollama's `/api/embeddings` endpoint for each article. The preference vector is updated with a running weighted mean, so it learns from every vote.
- **Full-content extraction.** When an RSS feed only includes a teaser (e.g. Lobsters' `<p><a>Comments</a></p>`), smarrst fetches the article URL in the background and runs it through Mozilla Readability to extract the main text. The extracted text is what gets embedded and what you read.
- **Time decay ranking.** Each article's score is `cosine_similarity(article, prefs) + 0.5^(age_hours / half_life)`. Default half-life is one week; tune in Settings.

## Architecture

```
src/
├── main.rs                # Binary entry point — initializes state, launches Dioxus
├── lib.rs                 # Library entry point — re-exports backend for tests
├── backend/
│   ├── mod.rs             # AppState (DB + HTTP client + settings)
│   ├── db.rs              # SQLite schema, queries, migration
│   ├── models.rs          # Feed, Article, Vote, ContentStatus, Settings
│   ├── rss.rs             # feed-rs based fetching
│   ├── content.rs         # Fetch original URL + Mozilla Readability extraction
│   ├── ollama.rs          # Embeddings, ping, cosine similarity
│   ├── ranking.rs         # Score computation and preference update
│   ├── settings.rs        # Persisted user settings
│   └── actions.rs         # Public API the UI calls (all async, spawn_blocking SQLite)
└── ui/
    ├── mod.rs             # Dioxus components (App, ArticleListItem, dialogs, ...)
    └── context.rs         # AppContext provider
assets/
└── styles.css             # Dark UI styling
tests/
├── backend.rs             # 13 unit tests for DB, settings, ranking, content
└── e2e_ollama.rs          # End-to-end tests against a real local Ollama
```

### Content pipeline

When a feed is added or refreshed:

1. RSS parser extracts every item (title, link, summary, `<content:encoded>`).
2. Each article is inserted with `content_status = 'none'`.
3. A background task walks the new articles. For each one whose stored body is "thin" (under 400 chars of visible text, or more than 1/3 of the raw HTML is `<a>` tags — Lobsters' "Comments" stub, for example), the article URL is fetched with the shared `reqwest::Client` and run through [`readability`](https://crates.io/crates/readability) (Mozilla's algorithm). The extracted text replaces the `content` column, `content_status` flips to `loaded`, and the article is re-embedded.
4. The ranking view picks up the new embedding on the next refresh.

The article view in the UI also has a **Refresh content** button for forcing a re-extraction (e.g. if the server was down the first time).

### Ranking

The score for each article is:

```
score = ((cosine_similarity(article_embedding, preference_vector) + 1) / 2) + 0.5^(age_hours / half_life)
```

- `preference_vector` starts as zero. The first vote seeds it with the article's embedding (signed positive for upvote, negative for downvote).
- Each subsequent vote updates the running mean: `new_pref = (old_pref * (n - 1) ± embed) / n` where `n` is the total number of votes cast.
- Time decay uses an exponential half-life so older articles fade out gradually.
- When you have no votes yet, all articles tie on similarity (0) and only time decay differentiates them — newest first.

## Setup

1. **Install Ollama** and start the server. The default is `http://localhost:11434`.

   ```sh
   brew install ollama
   ollama serve   # in a separate terminal
   ```

2. **Pull an embedding model.** The default is `nomic-embed-text`:

   ```sh
   ollama pull nomic-embed-text
   ```

   Any model that supports `/api/embeddings` works. Change the model name in Settings → Embedding model.

3. **Build and run.**

   ```sh
   cargo run --release
   ```

   The release binary lands at `target/release/smarrst`.

## Using the app

- Click **Add feed**, paste an RSS/Atom URL, hit **Add**. The feed is fetched immediately; the new articles are then walked in the background to fetch their full content (when the RSS body is just a teaser) and to compute embeddings.
- Click **Refresh all** to re-fetch every feed (and re-run the content/embedding pipeline for new items).
- Click any article in the middle column to read it. If the body is still being fetched from the web you'll see a "Fetching full content..." status; it will refresh in place once the extraction is done. Use the **Refresh content** button to force a re-extraction.
- Use the **Up** / **Down** buttons to vote; the preference vector is updated in the background. Re-ranking happens automatically after every vote.
- Use **Settings** to change the Ollama URL, the embedding model, the chat model (reserved for future use), or the time-decay half-life. The **Test Ollama** button pings the server.
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
| Chat model | `llama3.2` | Reserved; not used in the current MVP |
| Time-decay half-life | `168.0` (hours, ≈ 1 week) | Smaller values push newer articles up faster |

## Known limitations / next steps

- The chat model is wired into the codebase but not yet used. The "process article and vote" pipeline is currently embeddings-only; the next iteration will mix in an LLM call that summarizes *why* an upvote happened and stores the result alongside the embedding.
- For "discussion" feeds (Hacker News, Lobsters, Reddit) the RSS item URL points to the discussion page, not the linked article. Readability extracts whatever is on the discussion page; the actual article is one click away via **Open in browser**. Following the canonical link inside the page is a v2 feature.
- The content and embedding loops are sequential inside the background task; under heavy load they would benefit from batching and a concurrency cap.
- There is no pagination; long feed lists live in a scrollable container.

## License

TBD.
