use crate::backend::AppState;
use anyhow::Context;
use std::io::Cursor;

/// `true` when the article's existing content field looks substantial enough
/// that we don't need to fetch the original URL. Heuristic: a fair amount of
/// visible text and not mostly a list of links.
pub fn content_is_substantial(content: Option<&str>) -> bool {
    const MIN_CHARS: usize = 400;
    let Some(text) = content else { return false };
    let stripped = strip_html(text);
    if stripped.len() < MIN_CHARS {
        return false;
    }
    // If more than a third of the content is `<a>` tags, it's probably just
    // a list of links (e.g. Lobsters' "Comments" entry).
    let link_chars: usize = text
        .match_indices("<a ")
        .map(|(i, _)| text[i..].find('>').unwrap_or(text.len() - i))
        .sum();
    link_chars * 3 < text.len()
}

/// `true` if a URL's scheme and host are safe to fetch. Used both as a
/// pre-flight check before issuing the request and as the predicate for
/// the reqwest redirect policy. Hostnames (vs. literal IPs) are accepted;
/// we can't resolve DNS synchronously, so DNS-rebinding is a known
/// residual risk (a feed item linking to `attacker.com` that resolves to
/// `127.0.0.1` would still be fetched).
pub fn url_is_safe(url: &url::Url) -> bool {
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(v4)) => !ip_is_blocked(&std::net::IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) => !ip_is_blocked(&std::net::IpAddr::V6(v6)),
        Some(url::Host::Domain(_)) => true,
        None => false,
    }
}

/// Same as [`url_is_safe`] but takes a string and returns a typed error.
pub fn validate_public_url(url_str: &str) -> anyhow::Result<url::Url> {
    let parsed = url::Url::parse(url_str).context("invalid url")?;
    if !url_is_safe(&parsed) {
        anyhow::bail!("refusing to fetch {url_str}: blocked scheme or host");
    }
    Ok(parsed)
}

fn ip_is_blocked(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // unique local (fc00::/7)
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // link-local (fe80::/10)
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

pub fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Heuristic: detect when a stored markdown body contains obvious noise
/// that the new `html_to_markdown` (with `skip_tags`) wouldn't produce
/// today — JSON-LD metadata, inline JavaScript, raw `<script>` / `<style>`
/// blocks. Articles saved before the noise-stripping change match this
/// pattern; the UI uses it to fall back to the clean plain-text body
/// rather than render the garbage.
pub fn markdown_looks_broken(md: &str) -> bool {
    let head: String = md.chars().take(800).collect();
    head.contains("\"@context\"")
        || head.contains("\"@type\"")
        || head.contains("document.documentElement")
        || head.contains("document.querySelector")
        || head.contains("document.getElementById")
        || head.contains("<script")
        || head.contains("<style")
}

/// Convert a body of HTML into Markdown. Used to give the article view a
/// rendered, image-preserving representation. `htmd` keeps `<img>` tags as
/// `![alt](src)` markdown. `base_url` is used to absolutize relative image
/// and link URLs in the output — necessary because the desktop webview has
/// its own base URL (e.g. `tauri://localhost`) and won't resolve
/// `images/foo.png` correctly without a hint of the article's origin.
///
/// Tags that contribute no readable prose — `<script>`, `<style>`,
/// `<noscript>`, `<template>`, `<iframe>` — are skipped at the conversion
/// level so inline JSON-LD metadata and JS that many feeds (Medium, MS
/// Research, …) include inside the article body don't leak into the
/// rendered view.
pub fn html_to_markdown(html: &str, base_url: &str) -> String {
    let raw = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript", "template", "iframe"])
        .scripting_enabled(false)
        .build()
        .convert(html)
        .unwrap_or_else(|_| html.to_string());
    resolve_relative_urls(&raw, base_url)
}

/// Resolve relative `img src` and link URLs in markdown to absolute URLs
/// using `base` as the base. Skips `data:`, `mailto:`, and absolute URLs.
/// Anchors (`#section`) are kept on the article URL.
pub fn resolve_relative_urls(md: &str, base: &str) -> String {
    let Ok(base_url) = url::Url::parse(base) else {
        return md.to_string();
    };
    let resolve = |target: &str| -> String {
        if target.starts_with("data:") || target.starts_with("mailto:") {
            return target.to_string();
        }
        if target.is_empty() {
            return target.to_string();
        }
        // Anchors stay on the article URL.
        if let Some(frag) = target.strip_prefix('#') {
            return format!("{base}#{frag}");
        }
        // Already absolute (has a scheme).
        if target.contains("://") {
            return target.to_string();
        }
        // Protocol-relative or scheme-only.
        if target.starts_with("//") {
            return format!("{}:{target}", base_url.scheme());
        }
        match base_url.join(target) {
            Ok(abs) => abs.to_string(),
            Err(_) => target.to_string(),
        }
    };
    rewrite_markdown_urls(md, &resolve)
}

fn rewrite_markdown_urls<F: Fn(&str) -> String>(md: &str, resolve: &F) -> String {
    // Markdown links: [text](url "optional title") and images: ![alt](url "t")
    static LINK_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static IMG_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let link_re = LINK_RE.get_or_init(|| {
        regex::Regex::new(r#"\[([^\]\n]*)\]\(([^\s)]+)(?:\s+\"([^\"]*)\")?\)"#).unwrap()
    });
    let img_re = IMG_RE.get_or_init(|| {
        regex::Regex::new(r#"!\[([^\]\n]*)\]\(([^\s)]+)(?:\s+\"([^\"]*)\")?\)"#).unwrap()
    });
    let rewrite_one = |caps: &regex::Captures, is_image: bool| -> String {
        let label = &caps[1];
        let url = resolve(&caps[2]);
        let title = caps.get(3).map(|m| m.as_str());
        let prefix = if is_image { "!" } else { "" };
        match title {
            Some(t) => format!("{prefix}[{label}]({url} \"{t}\")"),
            None => format!("{prefix}[{label}]({url})"),
        }
    };
    // Rewrite images first (longer match `![..](..)` would otherwise be picked
    // up by the link regex's `!` lookahead failure — but `pulldown-cmark`
    // handles them independently anyway).
    let md = img_re.replace_all(md, |c: &regex::Captures| rewrite_one(c, true));
    link_re
        .replace_all(&md, |c: &regex::Captures| rewrite_one(c, false))
        .into_owned()
}

/// One HTTP fetch → readability extraction → (cleaned HTML, plain text).
/// Using the cleaned HTML for markdown conversion (instead of re-fetching
/// the raw page) drops the JSON-LD / inline-JS / `<head>` noise that
/// Medium, MS Research, and similar sites embed inside the article body.
pub async fn extract_from_url(
    state: &AppState,
    url: &str,
) -> anyhow::Result<readability::extractor::Product> {
    // SSRF guard: refuse to fetch loopback, private, or link-local hosts.
    let parsed = validate_public_url(url).with_context(|| format!("refusing to fetch {url}"))?;
    let resp = state
        .http
        .get(parsed.clone())
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("GET {url} returned {status}");
    }
    // Cap the response size to avoid runaway memory.
    const MAX_BYTES: usize = 5 * 1024 * 1024;
    let bytes = resp.bytes().await?;
    let slice = if bytes.len() > MAX_BYTES {
        &bytes[..MAX_BYTES]
    } else {
        &bytes[..]
    };
    let mut cursor = Cursor::new(slice);
    let product = readability::extractor::extract(&mut cursor, &parsed)
        .map_err(|e| anyhow::anyhow!("readability: {e:?}"))?;
    if product.text.trim().is_empty() {
        anyhow::bail!("no extractable text");
    }
    Ok(product)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_removes_tags() {
        assert_eq!(strip_html("<p>hi <b>there</b></p>"), "hi there");
        assert_eq!(strip_html("plain text"), "plain text");
        assert_eq!(strip_html("<a href='x'>link</a>"), "link");
    }

    #[test]
    fn substantial_content_detection() {
        assert!(!content_is_substantial(None));
        assert!(!content_is_substantial(Some("")));
        let long = "word ".repeat(200);
        assert!(content_is_substantial(Some(&long)));
        let linky = "<a href='x'>link</a> ".repeat(50);
        assert!(!content_is_substantial(Some(&linky)));
        let mixed = format!("{} <a href='x'>link</a>", "word ".repeat(200));
        assert!(content_is_substantial(Some(&mixed)));
    }

    #[test]
    fn html_to_markdown_preserves_images() {
        let html = r#"<p>Look at this:</p><img src="https://x.example/cat.png" alt="a cat" />"#;
        let md = html_to_markdown(html, "https://x.example/post");
        assert!(
            md.contains("![a cat](https://x.example/cat.png)"),
            "got: {md}"
        );
    }

    #[test]
    fn html_to_markdown_preserves_links_and_emphasis() {
        let html = r#"<p>Hello <strong>world</strong> from <a href="https://x">a link</a></p>"#;
        let md = html_to_markdown(html, "https://x.example/post");
        assert!(md.contains("**world**"));
        assert!(md.contains("[a link](https://x)"));
    }

    #[test]
    fn resolve_relative_urls_for_images_and_links() {
        let md =
            r#"![logo](/images/logo.svg) [read more](more.html) [anchor](#why) [abs](https://x/y)"#;
        let out = resolve_relative_urls(md, "https://example.com/blog/post");
        assert!(
            out.contains("![logo](https://example.com/images/logo.svg)"),
            "got: {out}"
        );
        assert!(
            out.contains("[read more](https://example.com/blog/more.html)"),
            "got: {out}"
        );
        assert!(
            out.contains("[anchor](https://example.com/blog/post#why)"),
            "got: {out}"
        );
        assert!(out.contains("[abs](https://x/y)"), "got: {out}");
    }

    #[test]
    fn resolve_relative_urls_leaves_data_and_mailto() {
        let md = r#"![inline](data:image/png;base64,AAAA) [email](mailto:a@b.com)"#;
        let out = resolve_relative_urls(md, "https://x/");
        assert!(out.contains("data:image/png;base64,AAAA"), "got: {out}");
        assert!(out.contains("mailto:a@b.com"), "got: {out}");
    }

    #[test]
    fn html_to_markdown_strips_json_ld_and_inline_js() {
        let html = r#"
            <h1>My post</h1>
            <script type="application/ld+json">
              {"@context":"https://schema.org","@type":"Article","headline":"x"}
            </script>
            <script>document.documentElement.classList.remove('no-js');</script>
            <p>First paragraph of the post.</p>
            <style>p { color: red; }</style>
            <p>Second paragraph.</p>
            <noscript><p>JS-disabled fallback.</p></noscript>
        "#;
        let md = html_to_markdown(html, "https://example.com/post");
        assert!(md.contains("My post"), "missing title: {md}");
        assert!(md.contains("First paragraph"), "missing body: {md}");
        assert!(md.contains("Second paragraph"), "missing body: {md}");
        assert!(!md.contains("@context"), "JSON-LD leaked: {md}");
        assert!(
            !md.contains("document.documentElement"),
            "inline JS leaked: {md}"
        );
        assert!(!md.contains("color: red"), "<style> content leaked: {md}");
        assert!(
            !md.contains("JS-disabled fallback"),
            "<noscript> content leaked: {md}"
        );
    }

    #[test]
    fn markdown_looks_broken_detects_json_ld_and_inline_js() {
        let ok = "# Hello\nA clean post with [a link](https://x).";
        assert!(
            !markdown_looks_broken(ok),
            "clean md flagged as broken: {ok}"
        );
        let jsonld =
            r#"{"@context":"https://schema.org","@type":"Article","headline":"x"} post body"#;
        assert!(markdown_looks_broken(jsonld));
        let js = "document.documentElement.classList.remove('no-js');\n\nbody";
        assert!(markdown_looks_broken(js));
        let script = "before <script>alert(1)</script> after";
        assert!(markdown_looks_broken(script));
    }

    #[test]
    fn validate_public_url_accepts_http_and_https() {
        for s in [
            "http://example.com/post",
            "https://example.com/post",
            "https://example.com:8080/path?q=1",
        ] {
            let parsed = validate_public_url(s).expect(s);
            assert_eq!(parsed.as_str(), s);
        }
    }

    #[test]
    fn validate_public_url_rejects_non_http_schemes() {
        for s in [
            "file:///etc/passwd",
            "ftp://example.com/feed",
            "data:text/html,<script>alert(1)</script>",
            "javascript:alert(1)",
        ] {
            assert!(validate_public_url(s).is_err(), "should reject {s}");
        }
    }

    #[test]
    fn url_is_safe_blocks_loopback_and_private_ips() {
        for s in [
            "http://127.0.0.1/x",
            "http://127.0.0.1:8080/admin",
            "http://10.0.0.1/x",
            "http://10.255.255.255/x",
            "http://172.16.0.1/x",
            "http://192.168.1.1/x",
            "http://169.254.169.254/latest/meta-data/",
            "http://0.0.0.0/x",
            "http://255.255.255.255/x",
            "http://[::1]/x",
            "http://[::]/x",
            "http://[fc00::1]/x",
            "http://[fe80::1]/x",
            "http://[ff02::1]/x",
        ] {
            let parsed = url::Url::parse(s).expect(s);
            assert!(!url_is_safe(&parsed), "should block {s}");
        }
    }

    #[test]
    fn url_is_safe_accepts_hostnames_and_public_ips() {
        for s in [
            "http://example.com/x",
            "https://news.ycombinator.com/rss",
            "http://1.1.1.1/x",
            "http://8.8.8.8:53/x",
        ] {
            let parsed = url::Url::parse(s).expect(s);
            assert!(url_is_safe(&parsed), "should accept {s}");
        }
    }
}
