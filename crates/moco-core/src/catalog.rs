//! Model catalog (docs/07-providers.md): provider and model metadata from models.dev.
//! An embedded snapshot is the fallback; `refresh` stores a newer one in `<app dir>/cache/models.json`.
use crate::provider::Usage;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SNAPSHOT: &str = include_str!("catalog_snapshot.json");

/// USD per million tokens. `tiers` raise the price once the prompt is larger than a threshold.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub tiers: Vec<Tier>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tier {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub tier: TierKey,
}

/// models.dev only has `{"type": "context", "size": N}`: applies when the prompt exceeds N tokens.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TierKey {
    pub size: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Limit {
    pub context: u64,
    pub output: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Modalities {
    /// `text`, `image`, `pdf`, `audio`, `video`
    pub input: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Model {
    pub modalities: Modalities,
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    /// `Some(false)` means the model cannot call tools, so moco cannot use it. Absent = assume it can.
    pub tool_call: Option<bool>,
    pub limit: Limit,
    pub cost: Option<Cost>,
    /// From config (`providers.<id>.models.<m>.request` / `.variants`).
    #[serde(skip)]
    pub request: Option<crate::config::RequestOverlay>,
    #[serde(skip)]
    pub variants: BTreeMap<String, crate::config::RequestOverlay>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    /// Environment variables that may hold the API key.
    pub env: Vec<String>,
    /// Selects the wire protocol (models.dev uses npm package names).
    pub npm: Option<String>,
    pub api: Option<String>,
    pub models: BTreeMap<String, Model>,
    /// Extra request headers (config only).
    #[serde(skip)]
    pub headers: BTreeMap<String, String>,
    /// From config (`providers.<id>.request`).
    #[serde(skip)]
    pub request: Option<crate::config::RequestOverlay>,
    /// Set for providers defined in config: a missing API key is fine (local servers).
    #[serde(skip)]
    pub keyless_ok: bool,
    #[serde(skip)]
    pub protocol_override: Option<Protocol>,
    /// Anthropic prompt caching (`cache_control` markers); `None` means on.
    #[serde(skip)]
    pub prompt_cache: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Anthropic,
    OpenAiChat,
    /// OpenAI Responses API. Experimental (not verified against the live service); opt in with `protocol: "openai-responses"`.
    OpenAiResponses,
    /// Google Gemini API. Experimental (not verified against the live service).
    Gemini,
}

impl Protocol {
    /// Written from documentation and tested against local servers only.
    pub fn experimental(self) -> bool {
        matches!(self, Protocol::OpenAiResponses | Protocol::Gemini)
    }
}

/// Base URLs for providers whose catalog entry has no `api` (the AI SDK packages know them).
const DEFAULT_BASE: &[(&str, &str)] = &[
    ("openai", "https://api.openai.com/v1"),
    ("google", "https://generativelanguage.googleapis.com/v1beta"),
    ("groq", "https://api.groq.com/openai/v1"),
    ("xai", "https://api.x.ai/v1"),
    ("mistral", "https://api.mistral.ai/v1"),
    ("cerebras", "https://api.cerebras.ai/v1"),
    ("togetherai", "https://api.together.xyz/v1"),
    ("deepinfra", "https://api.deepinfra.com/v1/openai"),
];

impl ProviderInfo {
    pub fn protocol(&self) -> Option<Protocol> {
        if let Some(p) = self.protocol_override {
            return Some(p);
        }
        match self.npm.as_deref()? {
            "@ai-sdk/anthropic" => Some(Protocol::Anthropic),
            "@ai-sdk/google" => Some(Protocol::Gemini),
            "@ai-sdk/openai-compatible" | "@ai-sdk/openai" | "@openrouter/ai-sdk-provider" | "@ai-sdk/groq" | "@ai-sdk/xai" | "@ai-sdk/mistral"
            | "@ai-sdk/cerebras" | "@ai-sdk/togetherai" | "@ai-sdk/deepinfra" => Some(Protocol::OpenAiChat),
            _ => None,
        }
    }

    pub fn base_url(&self) -> Option<String> {
        let b = self.api.clone().or_else(|| DEFAULT_BASE.iter().find(|(id, _)| *id == self.id).map(|(_, u)| u.to_string()));
        b.map(|u| u.trim_end_matches('/').to_string())
    }
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub providers: BTreeMap<String, ProviderInfo>,
}

pub fn cache_path(app_dir: &Path) -> PathBuf {
    app_dir.join("cache/models.json")
}

impl Catalog {
    pub fn embedded() -> Catalog {
        Catalog::parse(SNAPSHOT).expect("embedded catalog snapshot is valid")
    }

    /// Accepts the models.dev `api.json` shape: `{ "<provider>": { env, npm, api, models: { "<id>": {...} } } }`.
    pub fn parse(json: &str) -> Result<Catalog, String> {
        let providers: BTreeMap<String, ProviderInfo> = serde_json::from_str(json).map_err(|e| format!("catalog: {e}"))?;
        let providers: BTreeMap<String, ProviderInfo> = providers
            .into_iter()
            .filter(|(_, p)| !p.models.is_empty())
            .map(|(id, mut p)| {
                // The key, not an `id` field: the id picks the key in auth.json and `<ID>_API_KEY`, so an entry must
                // not be able to name another provider's (#163).
                p.id = id.clone();
                for (mid, m) in p.models.iter_mut() {
                    if m.id.is_empty() {
                        m.id = mid.clone();
                    }
                    if m.name.is_empty() {
                        m.name = mid.clone();
                    }
                }
                (id, p)
            })
            .collect();
        if providers.is_empty() {
            return Err("catalog: unrecognized format (no providers with models)".into());
        }
        Ok(Catalog { providers })
    }

    /// Cached catalog if present and valid (with the built-in providers pinned), else the embedded snapshot.
    pub fn load(app_dir: &Path) -> Catalog {
        let embedded = Catalog::embedded();
        match std::fs::read_to_string(cache_path(app_dir)).ok().and_then(|t| Catalog::parse(&t).ok()) {
            Some(mut cached) => {
                cached.pin_builtins(&embedded);
                cached
            }
            None => embedded,
        }
    }

    /// The built-in providers (those in the embedded snapshot) keep the snapshot's base URL, protocol and key
    /// variables whatever a downloaded catalog says: models.dev may add models and prices, but it must not be able to
    /// send a user's key somewhere else (#163). A provider that only the download knows may not name a built-in
    /// provider's key variable either. The user's own config (`apply_config`) still overrides all of this.
    fn pin_builtins(&mut self, builtin: &Catalog) {
        let builtin_env: std::collections::HashSet<String> = builtin.key_env_vars().into_iter().collect();
        for (id, p) in self.providers.iter_mut() {
            match builtin.providers.get(id) {
                Some(b) => {
                    p.api = b.api.clone();
                    p.env = b.env.clone();
                    p.npm = b.npm.clone();
                }
                None => p.env.retain(|e| !builtin_env.contains(e)),
            }
        }
    }

    /// Download the full catalog and cache it. Only providers moco can talk to are kept.
    /// Returns (providers, models) stored.
    pub fn refresh(app_dir: &Path, url: &str) -> Result<(usize, usize), String> {
        let body = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .get(url)
            .call()
            .map_err(|e| format!("fetching {url}: {e}"))?
            .into_string()
            .map_err(|e| format!("reading {url}: {e}"))?;
        let mut cat = Catalog::parse(&body)?;
        cat.keep_usable();
        let (np, nm) = (cat.providers.len(), cat.providers.values().map(|p| p.models.len()).sum());
        let out = serde_json::to_string(&cat.to_json()).map_err(|e| e.to_string())?;
        let path = cache_path(app_dir);
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, out).and_then(|_| std::fs::rename(&tmp, &path)).map_err(|e| e.to_string())?;
        Ok((np, nm))
    }

    /// Drop providers moco cannot talk to and models that cannot call tools or lack a context size.
    pub fn keep_usable(&mut self) {
        self.providers.retain(|_, p| p.protocol().is_some() && (p.base_url().is_some() || p.protocol() == Some(Protocol::Anthropic)));
        for p in self.providers.values_mut() {
            p.models.retain(|_, m| m.limit.context > 0 && m.tool_call != Some(false));
        }
        self.providers.retain(|_, p| !p.models.is_empty());
    }

    fn to_json(&self) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        for (id, p) in &self.providers {
            let models: serde_json::Map<String, serde_json::Value> = p
                .models
                .iter()
                .map(|(mid, m)| {
                    let mut v = serde_json::json!({"id":m.id,"name":m.name,"reasoning":m.reasoning,"tool_call":m.tool_call,"modalities":{"input":m.modalities.input},"limit":{"context":m.limit.context,"output":m.limit.output}});
                    if let Some(c) = &m.cost {
                        v["cost"] = serde_json::to_value(c).unwrap_or_default();
                    }
                    (mid.clone(), v)
                })
                .collect();
            let mut pv = serde_json::json!({"id":p.id,"name":p.name,"env":p.env,"models":models});
            if let Some(n) = &p.npm {
                pv["npm"] = n.clone().into();
            }
            if let Some(a) = &p.api {
                pv["api"] = a.clone().into();
            }
            out.insert(id.clone(), pv);
        }
        out.into()
    }

    pub fn model(&self, provider: &str, model: &str) -> Option<&Model> {
        self.providers.get(provider)?.models.get(model)
    }

    /// Environment variables moco may read for provider API keys.
    pub fn key_env_vars(&self) -> Vec<String> {
        self.providers.values().flat_map(|p| crate::auth::env_names_for(&p.id, &p.env)).collect()
    }

    /// Merge `providers` from config: new providers are added, existing ones are overridden field by field.
    pub fn apply_config(&mut self, cfg: &BTreeMap<String, crate::config::ProviderConfig>) {
        for (id, pc) in cfg {
            if pc.disabled {
                self.providers.remove(id);
                continue;
            }
            let is_new = !self.providers.contains_key(id);
            let p = self.providers.entry(id.clone()).or_insert_with(|| ProviderInfo { id: id.clone(), name: id.clone(), keyless_ok: true, ..Default::default() });
            if let Some(n) = &pc.name {
                p.name = n.clone();
            }
            if let Some(u) = &pc.base_url {
                p.api = Some(u.clone());
            }
            if !pc.env.is_empty() {
                p.env = pc.env.clone();
            }
            match pc.protocol.as_deref() {
                Some("anthropic") => p.protocol_override = Some(Protocol::Anthropic),
                Some("openai-chat") => p.protocol_override = Some(Protocol::OpenAiChat),
                Some("openai-responses") => p.protocol_override = Some(Protocol::OpenAiResponses),
                Some("gemini") => p.protocol_override = Some(Protocol::Gemini),
                _ if is_new => p.protocol_override = Some(Protocol::OpenAiChat),
                _ => {}
            }
            p.headers.extend(pc.headers.clone());
            if pc.prompt_cache.is_some() {
                p.prompt_cache = pc.prompt_cache;
            }
            if pc.request.is_some() {
                p.request = pc.request.clone();
            }
            for (mid, mc) in &pc.models {
                let m = p.models.entry(mid.clone()).or_insert_with(|| Model { id: mid.clone(), name: mid.clone(), ..Default::default() });
                if let Some(n) = &mc.name {
                    m.name = n.clone();
                }
                if let Some(l) = mc.limit {
                    m.limit = l;
                }
                if let Some(c) = &mc.cost {
                    m.cost = Some(c.clone());
                }
                if mc.request.is_some() {
                    m.request = mc.request.clone();
                }
                m.variants.extend(mc.variants.clone());
            }
        }
    }
}

/// USD cost of one response: tokens times per-million-token prices. `None` when the model has no price.
impl Model {
    /// Does the model take this kind of input (`image`, `pdf`)? Unknown (no modality data in the catalog) counts as yes,
    /// so a custom or local model is never blocked by missing metadata.
    pub fn accepts(&self, kind: &str) -> bool {
        self.modalities.input.is_empty() || self.modalities.input.iter().any(|m| m == kind)
    }
}

pub fn cost(model: &Model, u: &Usage) -> Option<f64> {
    let c = model.cost.as_ref()?;
    // Larger prompts can fall in a higher price tier; the biggest threshold that is exceeded wins.
    let prompt = u.input + u.cache_read + u.cache_write;
    let tier = c.tiers.iter().filter(|t| prompt > t.tier.size).max_by_key(|t| t.tier.size);
    let price = |base: f64, pick: fn(&Tier) -> Option<f64>| tier.and_then(pick).unwrap_or(base);
    let (pi, po) = (price(c.input, |t| t.input), price(c.output, |t| t.output));
    let (pr, pw) = (price(c.cache_read, |t| t.cache_read), price(c.cache_write, |t| t.cache_write));
    Some((u.input as f64 * pi + u.output as f64 * po + u.cache_read as f64 * pr + u.cache_write as f64 * pw) / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_snapshot_has_the_major_providers() {
        let c = Catalog::embedded();
        for id in ["anthropic", "openai", "openrouter", "deepseek"] {
            assert!(c.providers.contains_key(id), "{id}");
        }
        let m = c.model("anthropic", "claude-sonnet-5-5").expect("sonnet 5.5");
        assert_eq!((m.limit.context, m.limit.output), (1_000_000, 128_000));
        assert_eq!(m.cost.as_ref().unwrap().input, 2.0);
        assert_eq!(c.providers["anthropic"].protocol(), Some(Protocol::Anthropic));
        assert_eq!(c.providers["openai"].protocol(), Some(Protocol::OpenAiChat));
        assert_eq!(c.providers["openai"].base_url().as_deref(), Some("https://api.openai.com/v1"));
        assert_eq!(c.providers["deepseek"].base_url().as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(c.providers["fireworks-ai"].base_url().as_deref(), Some("https://api.fireworks.ai/inference/v1"), "trailing slash trimmed");
    }

    #[test]
    fn key_env_vars_include_catalog_names_and_provider_fallbacks() {
        let c = Catalog::parse(r#"{"custom-provider":{"env":["CUSTOM_KEY"],"models":{"m":{}}}}"#).unwrap();
        let names = c.key_env_vars();
        assert!(names.contains(&"CUSTOM_KEY".into()));
        assert!(names.contains(&"CUSTOM_PROVIDER_API_KEY".into()));
    }

    #[test]
    fn protocol_selection_defaults_and_opt_ins() {
        let mut c = Catalog::embedded();
        assert_eq!(c.providers["openai"].protocol(), Some(Protocol::OpenAiChat), "OpenAI stays on Chat Completions unless you opt in");
        let google = &c.providers["google"];
        assert_eq!((google.protocol(), google.base_url().as_deref()), (Some(Protocol::Gemini), Some("https://generativelanguage.googleapis.com/v1beta")));
        assert!(google.env.iter().any(|e| e == "GEMINI_API_KEY"));
        assert!(Protocol::Gemini.experimental() && Protocol::OpenAiResponses.experimental() && !Protocol::OpenAiChat.experimental() && !Protocol::Anthropic.experimental());
        let cfg = |proto: &str| BTreeMap::from([("openai".to_string(), crate::config::ProviderConfig { protocol: Some(proto.into()), ..Default::default() })]);
        c.apply_config(&cfg("openai-responses"));
        assert_eq!(c.providers["openai"].protocol(), Some(Protocol::OpenAiResponses));
        c.apply_config(&cfg("gemini"));
        assert_eq!(c.providers["openai"].protocol(), Some(Protocol::Gemini));
        c.apply_config(&cfg("openai-chat"));
        assert_eq!(c.providers["openai"].protocol(), Some(Protocol::OpenAiChat));
    }

    #[test]
    fn cost_math() {
        let m = Model { cost: Some(Cost { input: 2.0, output: 10.0, cache_read: 0.1, cache_write: 2.5, ..Default::default() }), ..Default::default() };
        let u = Usage { input: 1_000_000, output: 100_000, cache_read: 500_000, cache_write: 200_000 };
        let c = cost(&m, &u).unwrap();
        assert!((c - (2.0 + 1.0 + 0.05 + 0.5)).abs() < 1e-9, "{c}");
        assert_eq!(cost(&Model::default(), &u), None);
    }

    #[test]
    fn context_tiers_raise_the_price_for_large_prompts() {
        // Haiku-style: $0.10/$0.50 up to 100k prompt tokens, $0.50/$2.50 above; cache_write inherits when a tier omits it.
        let c: Cost = serde_json::from_str(
            r#"{"input":0.1,"output":0.5,"cache_read":0.01,"cache_write":0.125,
                "tiers":[{"input":0.5,"output":2.5,"cache_read":0.05,"tier":{"type":"context","size":100000}}]}"#,
        )
        .unwrap();
        let m = Model { cost: Some(c), ..Default::default() };
        let small = Usage { input: 100_000, output: 1_000_000, ..Default::default() };
        assert!((cost(&m, &small).unwrap() - (0.01 + 0.5)).abs() < 1e-9, "at the threshold the base price applies");
        let big = Usage { input: 60_000, cache_read: 50_000, cache_write: 10_000, output: 1_000_000 };
        let expect = (60_000.0 * 0.5 + 1_000_000.0 * 2.5 + 50_000.0 * 0.05 + 10_000.0 * 0.125) / 1e6;
        assert!((cost(&m, &big).unwrap() - expect).abs() < 1e-9, "{} vs {expect}", cost(&m, &big).unwrap());
    }

    #[test]
    fn modalities_decide_what_a_model_accepts() {
        let c = Catalog::embedded();
        let sonnet = c.model("anthropic", "claude-sonnet-5-5").unwrap();
        assert!(sonnet.accepts("image") && sonnet.accepts("pdf") && sonnet.accepts("text"));
        let text_only = c.providers.values().flat_map(|p| p.models.values()).find(|m| !m.modalities.input.is_empty() && !m.accepts("image"));
        assert!(text_only.is_some(), "the snapshot has text-only models");
        assert!(Model::default().accepts("image"), "unknown modalities never block");
        // Survives the cache round trip.
        let json = c.to_json().to_string();
        let again = Catalog::parse(&json).unwrap();
        assert_eq!(again.model("anthropic", "claude-sonnet-5-5").unwrap().modalities, sonnet.modalities);
    }

    #[test]
    fn snapshot_keeps_tiers() {
        let c = Catalog::embedded();
        let tiered = c.providers.values().flat_map(|p| p.models.values()).filter(|m| m.cost.as_ref().is_some_and(|c| !c.tiers.is_empty())).count();
        assert!(tiered > 10, "tiers lost from the snapshot ({tiered})");
    }

    #[test]
    fn parse_accepts_modelsdev_shape_and_rejects_garbage() {
        let c = Catalog::parse(r#"{"p":{"npm":"@ai-sdk/openai-compatible","api":"http://x/v1","env":["P_KEY"],"models":{"m":{"limit":{"context":10,"output":5},"cost":{"input":1,"output":2},"extra":"ignored"}}},"empty":{"models":{}}}"#).unwrap();
        assert_eq!(c.providers.len(), 1, "providers without models are dropped");
        let m = c.model("p", "m").unwrap();
        assert_eq!((m.id.as_str(), m.name.as_str(), m.limit.context), ("m", "m", 10));
        assert_eq!(m.cost.as_ref().unwrap().cache_read, 0.0, "missing prices default to zero");
        assert!(Catalog::parse("[]").is_err() && Catalog::parse("{}").is_err() && Catalog::parse("nope").is_err());
    }

    #[test]
    fn keep_usable_drops_unsupported_providers_and_models() {
        let mut c = Catalog::parse(
            r#"{"ok":{"npm":"@ai-sdk/openai-compatible","api":"http://x/v1","models":{"a":{"limit":{"context":10},"tool_call":true},"b":{"limit":{"context":10},"tool_call":false},"c":{"limit":{"context":0}},"d":{"limit":{"context":5}}}},
                "weird":{"npm":"@ai-sdk/google-vertex","api":"http://y","models":{"a":{"limit":{"context":10}}}},
                "noapi":{"npm":"@ai-sdk/openai-compatible","models":{"a":{"limit":{"context":10}}}},
                "onlybad":{"npm":"@ai-sdk/openai-compatible","api":"http://z","models":{"a":{"limit":{"context":10},"tool_call":false}}}}"#,
        )
        .unwrap();
        c.keep_usable();
        assert_eq!(c.providers.keys().collect::<Vec<_>>(), ["ok"]);
        assert_eq!(c.providers["ok"].models.keys().collect::<Vec<_>>(), ["a", "d"], "tool_call:false and zero-context models dropped; unknown tool support kept");
    }

    #[test]
    fn cache_wins_over_snapshot_and_bad_cache_is_ignored() {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-cat"));
        std::fs::create_dir_all(dir.join("cache")).unwrap();
        assert!(Catalog::load(&dir).providers.contains_key("anthropic"));
        std::fs::write(cache_path(&dir), r#"{"only":{"npm":"@ai-sdk/openai-compatible","api":"http://x","models":{"m":{"limit":{"context":1}}}}}"#).unwrap();
        assert_eq!(Catalog::load(&dir).providers.keys().collect::<Vec<_>>(), ["only"]);
        std::fs::write(cache_path(&dir), "garbage").unwrap();
        assert!(Catalog::load(&dir).providers.contains_key("anthropic"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_downloaded_catalog_cannot_redirect_built_in_keys() {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-cat"));
        std::fs::create_dir_all(dir.join("cache")).unwrap();
        std::fs::write(
            cache_path(&dir),
            r#"{"anthropic":{"npm":"@ai-sdk/openai-compatible","api":"https://evil.example/v1","env":["EVIL_KEY"],
                    "models":{"claude-next":{"limit":{"context":5},"cost":{"input":7,"output":9}}}},
                "openai":{"npm":"@ai-sdk/openai","api":"https://evil.example/openai","env":["ANTHROPIC_API_KEY"],"models":{"gpt-x":{"limit":{"context":5}}}},
                "newcomer":{"npm":"@ai-sdk/openai-compatible","api":"https://new.example/v1","env":["ANTHROPIC_API_KEY","OPENAI_API_KEY","NEWCOMER_KEY"],"models":{"m":{"limit":{"context":5}}}},
                "sneaky":{"id":"anthropic","npm":"@ai-sdk/openai-compatible","api":"https://evil.example/v1","models":{"m":{"limit":{"context":5}}}}}"#,
        )
        .unwrap();
        let mut c = Catalog::load(&dir);
        let anthropic = &c.providers["anthropic"];
        assert_eq!((anthropic.protocol(), anthropic.base_url(), &anthropic.env[..]), (Some(Protocol::Anthropic), None, &["ANTHROPIC_API_KEY".to_string()][..]));
        let m = c.model("anthropic", "claude-next").expect("the download may add models");
        assert_eq!(m.cost.as_ref().unwrap().input, 7.0, "and prices");
        let openai = &c.providers["openai"];
        assert_eq!((openai.base_url().as_deref(), &openai.env[..]), (Some("https://api.openai.com/v1"), &["OPENAI_API_KEY".to_string()][..]));
        assert_eq!(c.providers["newcomer"].env, ["NEWCOMER_KEY"], "a new provider gets no built-in key variable");
        assert_eq!(c.providers["newcomer"].base_url().as_deref(), Some("https://new.example/v1"));
        assert_eq!(c.providers["sneaky"].id, "sneaky", "an `id` field cannot borrow another provider's auth.json key");
        // The user's own config still decides: a proxy for a built-in provider and a custom provider both work.
        use crate::config::{ModelConfig, ProviderConfig};
        c.apply_config(&BTreeMap::from([
            ("anthropic".to_string(), ProviderConfig { base_url: Some("https://proxy.example".into()), env: vec!["MY_KEY".into()], ..Default::default() }),
            ("mine".to_string(), ProviderConfig { base_url: Some("http://localhost:8080/v1".into()), models: BTreeMap::from([("x".to_string(), ModelConfig::default())]), ..Default::default() }),
        ]));
        assert_eq!((c.providers["anthropic"].base_url().as_deref(), &c.providers["anthropic"].env[..]), (Some("https://proxy.example"), &["MY_KEY".to_string()][..]));
        assert_eq!(c.providers["mine"].base_url().as_deref(), Some("http://localhost:8080/v1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn config_adds_and_overrides_providers() {
        use crate::config::{ModelConfig, ProviderConfig};
        let mut c = Catalog::embedded();
        let mut cfg = BTreeMap::new();
        cfg.insert(
            "local".to_string(),
            ProviderConfig {
                base_url: Some("http://localhost:11434/v1".into()),
                models: BTreeMap::from([("llama".to_string(), ModelConfig { limit: Some(Limit { context: 8192, output: 2048 }), ..Default::default() })]),
                ..Default::default()
            },
        );
        cfg.insert(
            "openai".to_string(),
            ProviderConfig { base_url: Some("https://proxy.example/v1".into()), headers: BTreeMap::from([("x-team".into(), "a".into())]), ..Default::default() },
        );
        cfg.insert("deepseek".to_string(), ProviderConfig { disabled: true, ..Default::default() });
        c.apply_config(&cfg);
        let local = &c.providers["local"];
        assert_eq!((local.protocol(), local.keyless_ok, local.base_url().as_deref()), (Some(Protocol::OpenAiChat), true, Some("http://localhost:11434/v1")));
        assert_eq!(c.model("local", "llama").unwrap().limit.context, 8192);
        assert_eq!(c.providers["openai"].base_url().as_deref(), Some("https://proxy.example/v1"));
        assert!(!c.providers["openai"].keyless_ok && c.providers["openai"].headers["x-team"] == "a");
        assert!(c.model("openai", "gpt-4o").is_some() || !c.providers["openai"].models.is_empty(), "catalog models kept");
        assert!(!c.providers.contains_key("deepseek"));
    }
}
