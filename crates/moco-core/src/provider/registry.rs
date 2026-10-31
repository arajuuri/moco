//! Builds a client for every catalog provider that has credentials.
use super::{anthropic::Anthropic, openai_chat::OpenAiChat, Provider};
use crate::auth;
use crate::catalog::{Catalog, Protocol, ProviderInfo};
use std::collections::HashMap;
use std::path::Path;

/// API key for a provider, or `None` if none is configured.
pub fn key_for(p: &ProviderInfo, app_dir: &Path) -> Option<String> {
    auth::load_key_with(&p.id, &p.env, app_dir)
}

/// A provider is connected when moco speaks its protocol, it has a base URL (or is Anthropic),
/// and it has a key (config-defined providers such as local servers need none).
pub fn is_connected(p: &ProviderInfo, app_dir: &Path) -> bool {
    p.protocol().is_some() && (p.protocol() == Some(Protocol::Anthropic) || p.base_url().is_some()) && (p.keyless_ok || key_for(p, app_dir).is_some())
}

pub fn build(catalog: &Catalog, app_dir: &Path) -> HashMap<String, Box<dyn Provider>> {
    let mut out: HashMap<String, Box<dyn Provider>> = HashMap::new();
    for (id, p) in &catalog.providers {
        if !is_connected(p, app_dir) {
            continue;
        }
        let key = key_for(p, app_dir);
        let client: Box<dyn Provider> = match p.protocol() {
            Some(Protocol::Anthropic) => {
                // ANTHROPIC_BASE_URL is honored for proxies and test servers.
                let base = (id == "anthropic").then(|| std::env::var("ANTHROPIC_BASE_URL").ok()).flatten().or_else(|| p.base_url());
                let base = base.map(|b| b.trim_end_matches("/v1").to_string());
                Box::new(Anthropic::new(key.unwrap_or_default(), base))
            }
            Some(Protocol::OpenAiChat) => Box::new(OpenAiChat::new(key, p.base_url().unwrap_or_default(), p.headers.clone())),
            None => continue,
        };
        out.insert(id.clone(), client);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelConfig, ProviderConfig};
    use std::collections::BTreeMap;

    #[test]
    fn connected_means_protocol_plus_key_unless_keyless() {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-reg"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("auth.json"), r#"{"deepseek":{"key":"k"},"mistral":{"key":"k2"}}"#).unwrap();
        let mut cat = Catalog::embedded();
        cat.apply_config(&BTreeMap::from([(
            "ollama".to_string(),
            ProviderConfig { base_url: Some("http://localhost:11434/v1".into()), models: BTreeMap::from([("llama3".to_string(), ModelConfig::default())]), ..Default::default() },
        )]));
        let built = build(&cat, &dir);
        let mut ids: Vec<_> = built.keys().cloned().collect();
        ids.sort();
        for expected in ["deepseek", "mistral", "ollama"] {
            assert!(ids.contains(&expected.to_string()), "{expected} missing from {ids:?}");
        }
        assert!(!ids.contains(&"fireworks-ai".to_string()), "no key, not connected");
        let _ = std::fs::remove_dir_all(dir);
    }
}
