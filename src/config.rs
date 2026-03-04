use std::fs;
use std::path::PathBuf;

use uuid::Uuid;

use crate::constants::{DEFAULT_CLIENT_NAME, DEFAULT_SERVER_PATH};

pub(crate) fn default_client_name() -> String {
    if let Ok(value) = std::env::var("SENDSPIN_CLIENT_NAME") {
        if let Some(normalized) = normalize_client_name(&value) {
            return normalized;
        }
    }

    DEFAULT_CLIENT_NAME.to_string()
}

pub(crate) fn normalize_client_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Keep the wire payload bounded and readable.
    let max_chars = 80;
    let normalized: String = trimmed.chars().take(max_chars).collect();
    if normalized.is_empty() {
        return None;
    }

    Some(normalized)
}

pub(crate) fn normalize_server_url(raw: &str) -> Option<String> {
    let mut candidate = raw.trim().to_string();
    if candidate.is_empty() {
        return None;
    }

    if !candidate.contains("://") {
        candidate = format!("ws://{candidate}");
    } else if candidate.starts_with("http://") {
        candidate = format!("ws://{}", candidate.trim_start_matches("http://"));
    } else if candidate.starts_with("https://") {
        candidate = format!("wss://{}", candidate.trim_start_matches("https://"));
    }

    let mut parsed = url::Url::parse(&candidate).ok()?;
    if !matches!(parsed.scheme(), "ws" | "wss") || parsed.host_str().is_none() {
        return None;
    }

    if parsed.path().is_empty() || parsed.path() == "/" {
        parsed.set_path(DEFAULT_SERVER_PATH);
    }

    Some(parsed.to_string())
}

pub(crate) fn load_or_create_client_id() -> String {
    let path = client_id_path();

    if let Ok(existing) = fs::read_to_string(&path) {
        let candidate = existing.trim();
        if Uuid::parse_str(candidate).is_ok() {
            return candidate.to_string();
        }
    }

    let new_id = Uuid::new_v4().to_string();

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, &new_id);

    new_id
}

fn client_id_path() -> PathBuf {
    if let Some(mut config_dir) = dirs::config_dir() {
        config_dir.push("sendspin-vst3");
        config_dir.push("client_id");
        return config_dir;
    }

    PathBuf::from("sendspin-vst3-client-id")
}

#[cfg(test)]
mod tests {
    use super::{normalize_client_name, normalize_server_url};

    #[test]
    fn normalize_client_name_rejects_empty() {
        assert!(normalize_client_name("   ").is_none());
    }

    #[test]
    fn normalize_server_url_adds_default_path_and_scheme() {
        let url = normalize_server_url("localhost:8927").expect("url should normalize");
        assert_eq!(url, "ws://localhost:8927/sendspin");
    }

    #[test]
    fn normalize_server_url_rewrites_https() {
        let url = normalize_server_url("https://example.com").expect("url should normalize");
        assert_eq!(url, "wss://example.com/sendspin");
    }
}
