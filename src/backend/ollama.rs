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
        // Truncate to a reasonable length to avoid blowing up embedding context.
        const MAX_CHARS: usize = 4_000;
        if c.len() > MAX_CHARS {
            out.push_str("Content: ");
            out.push_str(&c[..MAX_CHARS]);
        } else {
            out.push_str("Content: ");
            out.push_str(c);
        }
    }
    out
}
