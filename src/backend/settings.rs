use crate::backend::models::Settings;
use rusqlite::{params, Connection};

pub fn load(conn: &Connection) -> anyhow::Result<Settings> {
    let parse_f32 = |key: &str, default: &str| -> anyhow::Result<f32> {
        Ok(read_string(conn, key, default)?
            .parse()
            .unwrap_or_else(|_| default.parse().unwrap_or(0.0)))
    };
    Ok(Settings {
        ollama_url: read_string(conn, "ollama_url", "http://localhost:11434")?,
        ollama_embed_model: read_string(conn, "ollama_embed_model", "nomic-embed-text")?,
        ollama_chat_model: read_string(conn, "ollama_chat_model", "llama3.2")?,
        vote_weight: parse_f32("vote_weight", "1.0")?,
        time_half_life_hours: parse_f32("time_half_life_hours", "168.0")?,
    })
}

pub fn save(conn: &Connection, s: &Settings) -> anyhow::Result<()> {
    write_string(conn, "ollama_url", &s.ollama_url)?;
    write_string(conn, "ollama_embed_model", &s.ollama_embed_model)?;
    write_string(conn, "ollama_chat_model", &s.ollama_chat_model)?;
    write_string(conn, "vote_weight", &s.vote_weight.to_string())?;
    write_string(
        conn,
        "time_half_life_hours",
        &s.time_half_life_hours.to_string(),
    )?;
    Ok(())
}

fn read_string(conn: &Connection, key: &str, default: &str) -> anyhow::Result<String> {
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .ok();
    Ok(v.unwrap_or_else(|| default.to_string()))
}

fn write_string(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}
