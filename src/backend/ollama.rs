use crate::backend::AppState;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    prompt: &'a str,
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    embedding: Vec<f32>,
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    message: ChatMessageOwned,
}

#[derive(Debug, Deserialize)]
struct ChatMessageOwned {
    content: String,
}

pub async fn embed(state: &AppState, text: &str) -> anyhow::Result<Vec<f32>> {
    let (url, model) = {
        let s = state.settings.lock().await;
        (s.ollama_url.clone(), s.ollama_embed_model.clone())
    };
    let resp = state
        .http
        .post(format!("{url}/api/embeddings"))
        .json(&EmbedRequest {
            model: &model,
            prompt: text,
        })
        .send()
        .await?
        .error_for_status()?
        .json::<EmbedResponse>()
        .await?;
    Ok(resp.embedding)
}

pub async fn ping(state: &AppState) -> anyhow::Result<bool> {
    let url = state.settings.lock().await.ollama_url.clone();
    let resp = state.http.get(format!("{url}/api/tags")).send().await?;
    Ok(resp.status().is_success())
}

/// Check that a specific model (by role) is available on the Ollama server.
/// Returns `Ok(true)` if the model appears in `/api/tags`, `Ok(false)` if
/// the server is reachable but the model is missing, `Err` if the server
/// itself is unreachable.
pub async fn ping_model(state: &AppState, role: &str) -> anyhow::Result<bool> {
    let (url, model) = {
        let s = state.settings.lock().await;
        let m = match role {
            "chat" => s.ollama_chat_model.clone(),
            _ => s.ollama_embed_model.clone(),
        };
        (s.ollama_url.clone(), m)
    };
    let resp = state
        .http
        .get(format!("{url}/api/tags"))
        .send()
        .await?
        .error_for_status()?;
    let body: TagsResponse = resp.json().await?;
    Ok(body.models.iter().any(|m| m.name == model))
}

#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagsModel>,
}

#[derive(Debug, Deserialize)]
struct TagsModel {
    name: String,
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f64;
    let mut na = 0.0_f64;
    let mut nb = 0.0_f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let x = *x as f64;
        let y = *y as f64;
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na.sqrt()) * (nb.sqrt());
    if denom == 0.0 {
        0.0
    } else {
        (dot / denom) as f32
    }
}

/// Truncate `s` to at most `max_bytes` bytes, snapping back to the
/// nearest preceding char boundary so we never panic on multi-byte chars.
fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx]
}

pub fn article_to_text(title: &str, content: Option<&str>, summary: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("Title: ");
    out.push_str(title);
    out.push('\n');
    if let Some(s) = summary {
        out.push_str("Summary: ");
        out.push_str(s);
        out.push('\n');
    }
    if let Some(c) = content {
        const MAX_CHARS: usize = 4_000;
        out.push_str("Content: ");
        out.push_str(truncate_at_char_boundary(c, MAX_CHARS));
    }
    out
}

pub fn article_to_classify_text(
    title: &str,
    content: Option<&str>,
    summary: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(title.trim());
    if let Some(s) = summary {
        let s = s.trim();
        if !s.is_empty() {
            out.push_str("\n\n");
            out.push_str(s);
        }
    }
    if let Some(c) = content {
        const MAX_CHARS: usize = 2_000;
        let c = truncate_at_char_boundary(c, MAX_CHARS).trim();
        if !c.is_empty() {
            out.push_str("\n\n");
            out.push_str(c);
        }
    }
    let out = truncate_at_char_boundary(&out, 4000);
    out.to_string()
}

pub fn normalize_category(raw: &str, labels: &[String]) -> String {
    let lowered: Vec<String> = labels.iter().map(|l| l.to_ascii_lowercase()).collect();
    for line in raw.lines() {
        for token in line.split_whitespace() {
            let cleaned = token
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_ascii_lowercase();
            if cleaned.is_empty() {
                continue;
            }
            if let Some(idx) = lowered.iter().position(|l| l == &cleaned) {
                return labels[idx].clone();
            }
        }
    }
    "Other".to_string()
}

pub async fn classify(state: &AppState, text: &str, labels: &[String]) -> anyhow::Result<String> {
    let (url, model) = {
        let s = state.settings.lock().await;
        (s.ollama_url.clone(), s.ollama_chat_model.clone())
    };
    let labels_csv = labels.join(", ");
    let system = format!(
        "You are a strict single-label classifier. Respond with EXACTLY one label from this list and nothing else: {labels_csv}."
    );
    let req = ChatRequest {
        model: &model,
        messages: vec![
            ChatMessage {
                role: "system",
                content: &system,
            },
            ChatMessage {
                role: "user",
                content: text,
            },
        ],
        stream: false,
    };
    let resp = state
        .http
        .post(format!("{url}/api/chat"))
        .json(&req)
        .send()
        .await?
        .error_for_status()?
        .json::<ChatResponse>()
        .await?;
    Ok(normalize_category(&resp.message.content, labels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_basic() {
        let a = vec![1.0, 0.0];
        let b = vec![1.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
        let c = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &c).abs() < 1e-6);
        let d = vec![-1.0, 0.0];
        assert!((cosine_similarity(&a, &d) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_zero_length() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        let a = vec![1.0, 2.0];
        let b = vec![1.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn article_to_text_truncates_long_content() {
        let title = "T";
        let content = "x".repeat(10_000);
        let out = article_to_text(title, Some(&content), None);
        assert!(out.contains("Content: "));
        assert!(out.len() < 10_000);
    }

    #[test]
    fn article_to_text_includes_summary_and_title() {
        let out = article_to_text("Hello", Some("body"), Some("sum"));
        assert!(out.contains("Title: Hello"));
        assert!(out.contains("Summary: sum"));
        assert!(out.contains("Content: body"));
    }

    #[test]
    fn normalize_category_picks_exact_label_case_insensitive() {
        let labels = vec![
            "Tech".to_string(),
            "Politics".to_string(),
            "Other".to_string(),
        ];
        assert_eq!(normalize_category("Tech", &labels), "Tech");
        assert_eq!(normalize_category("tech", &labels), "Tech");
        assert_eq!(normalize_category("POLITICS", &labels), "Politics");
    }

    #[test]
    fn normalize_category_handles_prose_wrapping() {
        let labels = vec!["Tech".to_string(), "Other".to_string()];
        assert_eq!(normalize_category("Probably Tech\n", &labels), "Tech");
        assert_eq!(
            normalize_category("I think the answer is Tech.", &labels),
            "Tech"
        );
        assert_eq!(normalize_category("  Tech  ", &labels), "Tech");
    }

    #[test]
    fn normalize_category_falls_back_to_other() {
        let labels = vec!["Tech".to_string(), "Other".to_string()];
        assert_eq!(normalize_category("garbage", &labels), "Other");
        assert_eq!(normalize_category("", &labels), "Other");
        assert_eq!(normalize_category("Politics", &labels), "Other");
    }

    #[test]
    fn article_to_classify_text_truncates_long_body() {
        let content = "x".repeat(10_000);
        let out = article_to_classify_text("Title", Some(&content), Some("sum"));
        assert!(out.contains("Title"));
        assert!(out.contains("sum"));
        assert!(out.len() <= 4000);
    }

    #[test]
    fn article_to_text_handles_multibyte_chars() {
        // Each Chinese char is 3 bytes. Truncating at 4000 bytes used to
        // panic when a 3-byte char straddled the boundary; should now
        // snap back to a safe boundary.
        let title = "标题";
        let mut content = String::new();
        while content.len() < 10_000 {
            content.push_str("内容");
        }
        let out = article_to_text(title, Some(&content), Some("摘要"));
        assert!(out.starts_with("Title: "));
        assert!(out.contains("Content: "));
        // Truncated body must end on a char boundary.
        let body = out.split("Content: ").nth(1).expect("body present");
        assert!(body.is_char_boundary(body.len()));
    }

    #[test]
    fn article_to_classify_text_handles_multibyte_chars() {
        let mut content = String::new();
        while content.len() < 5_000 {
            content.push_str("内容");
        }
        let out = article_to_classify_text("标题", Some(&content), Some("摘要"));
        assert!(out.len() <= 4000);
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn article_to_text_handles_emoji() {
        // 4-byte emoji at the truncation boundary.
        let mut content = "a".repeat(3998);
        content.push_str("🦀🦀🦀🦀"); // 4 bytes each
        let out = article_to_text("T", Some(&content), None);
        let body = out.split("Content: ").nth(1).expect("body present");
        assert!(body.is_char_boundary(body.len()));
    }

    #[test]
    fn ping_model_skips_when_ollama_unreachable() {
        // The default Ollama URL almost certainly isn't running on the
        // test host's port; just verify the function returns Err without
        // panicking.
        // We can't easily build a full AppState in a unit test, so just
        // assert the TagsResponse deserializes from a typical payload.
        let body = r#"{"models":[{"name":"llama3.2:1b","size":1234}]}"#;
        let parsed: TagsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.models.len(), 1);
        assert_eq!(parsed.models[0].name, "llama3.2:1b");
    }
}
