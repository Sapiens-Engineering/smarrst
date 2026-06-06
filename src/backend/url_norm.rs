//! Canonical URL form used for cross-feed deduplication.
//!
//! Two articles from different feeds that point to the same underlying
//! page should collapse to a single row in the `articles` table. The
//! original `url` column is preserved as-is (so the "Open in browser"
//! link keeps any tracking params the publisher expected), but the
//! `canonical_url` column is computed via [`canonicalize`] and is what
//! the unique partial index in `db.rs` enforces uniqueness on.
//!
//! Normalization rules, in order:
//! 1. Trim surrounding whitespace.
//! 2. Parse with `url::Url::parse` — invalid input returns `None`.
//! 3. Lowercase the scheme.
//! 4. Lowercase the host and strip a leading `www.`.
//! 5. Drop the fragment (`#section`).
//! 6. Drop common analytics / referrer query params, sort the rest
//!    alphabetically, and re-serialize (so `?a=1&b=2` and `?b=2&a=1`
//!    produce the same canonical form).
//! 7. Strip a trailing slash from the path, except when the path is
//!    just `/` (the canonical form for `https://example.com/`).

use url::Url;

/// Query params in this list are dropped before sorting. Covers the
/// usual analytics / referrer noise; not exhaustive, but the goal is
/// "two feeds linking to the same article collapse" rather than a
/// full URL-spec cleanup.
const TRACKING_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "utm_id",
    "fbclid",
    "gclid",
    "gclsrc",
    "mc_eid",
    "mc_cid",
    "yclid",
    "_ga",
    "_gl",
    "ref",
    "ref_src",
    "ref_url",
    "source",
    "src",
];

pub fn canonicalize(input: &str) -> Option<String> {
    let mut url = Url::parse(input.trim()).ok()?;

    // (3) Lowercase scheme.
    let scheme = url.scheme().to_ascii_lowercase();
    url.set_scheme(&scheme).ok()?;

    // (4) Lowercase host, strip leading `www.`.
    if let Some(host) = url.host_str().map(str::to_ascii_lowercase) {
        let stripped = host.strip_prefix("www.").unwrap_or(&host);
        let _ = url.set_host(Some(stripped));
    }

    // (5) Drop fragment.
    url.set_fragment(None);

    // (6) Drop tracking params, sort remaining.
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !TRACKING_PARAMS.contains(&k.as_ref()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.sort();
    let new_query: Option<String> = if pairs.is_empty() {
        None
    } else {
        Some(
            pairs
                .iter()
                .map(|(k, v)| {
                    if v.is_empty() {
                        k.clone()
                    } else {
                        format!("{k}={v}")
                    }
                })
                .collect::<Vec<_>>()
                .join("&"),
        )
    };
    if new_query.as_deref() != url.query() {
        url.set_query(new_query.as_deref());
    }

    // (7) Strip trailing slash from non-root path.
    let path = url.path().to_string();
    if path.len() > 1 && path.ends_with('/') {
        let trimmed = path.trim_end_matches('/').to_string();
        url.set_path(&trimmed);
    }

    Some(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases_scheme_and_host() {
        assert_eq!(
            canonicalize("HTTPS://Example.COM/path").as_deref(),
            Some("https://example.com/path")
        );
    }

    #[test]
    fn strips_www_prefix() {
        assert_eq!(
            canonicalize("https://www.example.com/path").as_deref(),
            Some("https://example.com/path")
        );
        // Don't strip `www` from inside a hostname (e.g. `wwwfoo.com`).
        assert_eq!(
            canonicalize("https://wwwfoo.example.com/x").as_deref(),
            Some("https://wwwfoo.example.com/x")
        );
    }

    #[test]
    fn strips_trailing_slash_from_path() {
        assert_eq!(
            canonicalize("https://example.com/path/").as_deref(),
            Some("https://example.com/path")
        );
        // Root path is kept as `/`.
        assert_eq!(
            canonicalize("https://example.com/").as_deref(),
            Some("https://example.com/")
        );
        // Multi-segment: only the trailing slash is removed.
        assert_eq!(
            canonicalize("https://example.com/a/b/").as_deref(),
            Some("https://example.com/a/b")
        );
    }

    #[test]
    fn drops_tracking_query_params() {
        assert_eq!(
            canonicalize("https://example.com/x?utm_source=hn&id=42").as_deref(),
            Some("https://example.com/x?id=42")
        );
        assert_eq!(
            canonicalize("https://example.com/x?fbclid=abc&gclid=def&q=rust").as_deref(),
            Some("https://example.com/x?q=rust")
        );
    }

    #[test]
    fn sorts_remaining_query_params() {
        assert_eq!(
            canonicalize("https://example.com/x?b=2&a=1").as_deref(),
            Some("https://example.com/x?a=1&b=2")
        );
    }

    #[test]
    fn drops_fragment() {
        assert_eq!(
            canonicalize("https://example.com/x#section").as_deref(),
            Some("https://example.com/x")
        );
    }

    #[test]
    fn collapses_two_feeds_pointing_at_same_article() {
        // Two different aggregators using different schemes for the
        // HN item: one with `?id=`, one without, one with `www.`,
        // one with `utm_source=`. All collapse to the same form.
        let a = canonicalize("https://news.ycombinator.com/item?id=123");
        let b = canonicalize("https://www.news.ycombinator.com/item?id=123");
        let c = canonicalize("https://news.ycombinator.com/item?id=123&utm_source=hn");
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn returns_none_for_invalid_input() {
        assert_eq!(canonicalize(""), None);
        assert_eq!(canonicalize("not a url"), None);
        assert_eq!(canonicalize("/relative/path"), None);
    }

    #[test]
    fn preserves_value_only_query_params() {
        // `?flag` with no `=` should be preserved (after sorting).
        assert_eq!(
            canonicalize("https://example.com/x?flag&a=1").as_deref(),
            Some("https://example.com/x?a=1&flag")
        );
    }
}
