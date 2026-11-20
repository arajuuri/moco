//! Reasoning effort ("variants"): which levels a model offers, and how each protocol asks for them.
//!
//! A level is a plain name (`low`, `medium`, `high`, `xhigh`, `max`, `minimal`, `none`). Which names exist
//! depends on the model; asking for one the model lacks is ignored by the engine. Config can also define
//! custom variants per model as body overlays (see `config::ModelConfig::variants`).
use super::*;
use crate::catalog::Protocol;
use serde_json::json;

/// How Anthropic models take thinking settings (docs of the Claude 5 family, 2026).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Off {
    /// Thinking cannot be switched off.
    No,
    /// `thinking: {type: "disabled"}`.
    Disabled,
    /// `thinking: {type: "between_tools"}` (Sonnet 5.5).
    BetweenTools,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicKind {
    /// `thinking: {type: "adaptive"}` plus `output_config.effort`. `always_on`: thinking happens even when the
    /// request says nothing, so moco asks for readable summaries.
    Adaptive { levels: &'static [&'static str], always_on: bool, off: Off },
    /// Older models: `thinking: {type: "enabled", budget_tokens}`.
    Budget,
}

const FIVE: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const FOUR_SIX: &[&str] = &["low", "medium", "high", "max"];

pub fn anthropic_kind(model: &str, reasoning: bool) -> Option<AnthropicKind> {
    let m = model.to_lowercase();
    let a = |levels, always_on, off| Some(AnthropicKind::Adaptive { levels, always_on, off });
    if m.starts_with("claude-fable-") || m.starts_with("claude-mythos-") {
        return a(FIVE, true, Off::No);
    }
    if m.starts_with("claude-opus-5-5") || m.starts_with("claude-haiku-5-5") {
        return a(FIVE, true, Off::No);
    }
    if m == "claude-opus-5" || m.starts_with("claude-opus-5-2") {
        return a(FIVE, true, Off::No);
    }
    if m.starts_with("claude-sonnet-5-5") {
        return a(FIVE, true, Off::BetweenTools);
    }
    if m == "claude-sonnet-5" || m.starts_with("claude-sonnet-5-2") {
        return a(FIVE, true, Off::Disabled);
    }
    if m.starts_with("claude-opus-4-8") || m.starts_with("claude-opus-4-7") {
        return a(FIVE, false, Off::Disabled);
    }
    if m.starts_with("claude-opus-4-6") || m.starts_with("claude-sonnet-4-6") {
        return a(FOUR_SIX, false, Off::Disabled);
    }
    (m.starts_with("claude-") && reasoning).then_some(AnthropicKind::Budget)
}

fn budget_for(level: &str) -> Option<u32> {
    match level {
        "low" => Some(2_048),
        "medium" => Some(8_192),
        "high" => Some(16_384),
        "max" => Some(32_768),
        _ => None,
    }
}

/// OpenAI's levels per model family. The API checks them per model, and Chat Completions and Responses differ
/// for some families (`max` exists only on Responses). Probed live on 2026-11-13 by sending a level the model
/// rejects; the 400 error lists the accepted ones ("Chat" = `/v1/chat/completions`, "Resp" = `/v1/responses`):
///
/// | Models                                                         | Chat                       | Resp                    |
/// |----------------------------------------------------------------|----------------------------|-------------------------|
/// | gpt-5, gpt-5-mini, gpt-5-nano                                  | minimal, low, medium, high | same                    |
/// | gpt-5-pro                                                      | (Responses only)           | high                    |
/// | gpt-5.1                                                        | none, low, medium, high    | same                    |
/// | gpt-5.2, gpt-5.3-codex, gpt-5.4(-mini, -nano), gpt-5.5         | none, low .. xhigh         | same (codex: Resp only) |
/// | gpt-5.2-pro, gpt-5.4-pro, gpt-5.5-pro                          | (Responses only)           | medium, high, xhigh     |
/// | gpt-5.6-luna/-sol/-terra, gpt-6-luna, gpt-6-sol, gpt-6.1-terra | none, low .. xhigh         | none, low .. xhigh, max |
/// | gpt-6-astra, gpt-6.1-sol                                       | low .. xhigh               | low .. xhigh, max       |
/// | o3                                                             | low, medium, high, xhigh   | low, medium, high       |
///
/// o3's `xhigh` (Chat only) is not offered, so it shows the same levels on both. Models not in the table: a newer
/// gpt-5.x or gpt-6.x and later get the newest family's levels (none .. xhigh, plus `max` on Responses from
/// gpt-5.6 on), a newer `-pro` model medium .. xhigh, and anything else (other o-series models, Azure deployment
/// names, unknown ids) low, medium, high, which every OpenAI reasoning model probed accepts except the `-pro` ones.
fn openai_levels(protocol: Protocol, model: &str) -> &'static [&'static str] {
    const LMH: &[&str] = &["low", "medium", "high"];
    let m = model.to_lowercase();
    let Some(rest) = m.strip_prefix("gpt-") else { return LMH };
    // "gpt-5-mini" -> (5, 0), "gpt-5.4-nano" -> (5, 4).
    let num = |s: &str| s.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u32>().ok();
    let Some(major) = num(rest) else { return LMH };
    let minor = rest.trim_start_matches(|c: char| c.is_ascii_digit()).strip_prefix('.').and_then(num).unwrap_or(0);
    let version = (major, minor);
    if version < (5, 0) {
        return LMH;
    }
    if m.split('-').any(|p| p == "pro") {
        return if version == (5, 0) { &["high"] } else { &["medium", "high", "xhigh"] };
    }
    match version {
        (5, 0) => &["minimal", "low", "medium", "high"],
        (5, 1) => &["none", "low", "medium", "high"],
        v if v < (5, 6) => &["none", "low", "medium", "high", "xhigh"],
        _ => {
            // These two cannot switch reasoning off.
            let off = !(m.starts_with("gpt-6-astra") || m.starts_with("gpt-6.1-sol"));
            match (off, matches!(protocol, Protocol::OpenAiResponses)) {
                (true, true) => &["none", "low", "medium", "high", "xhigh", "max"],
                (true, false) => &["none", "low", "medium", "high", "xhigh"],
                (false, true) => &["low", "medium", "high", "xhigh", "max"],
                (false, false) => &["low", "medium", "high", "xhigh"],
            }
        }
    }
}

/// The levels a model offers, in the order a picker shows them. Empty when it does not reason (or when moco
/// does not know how to ask this provider).
pub fn variants(protocol: Protocol, provider_id: &str, model: &str, reasoning: bool) -> Vec<String> {
    let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match protocol {
        Protocol::Anthropic => match anthropic_kind(model, reasoning) {
            Some(AnthropicKind::Adaptive { levels, off, .. }) => {
                let mut v = if off == Off::No { vec![] } else { own(&["none"]) };
                v.extend(own(levels));
                v
            }
            Some(AnthropicKind::Budget) => own(&["none", "low", "medium", "high", "max"]),
            None => vec![],
        },
        // Only OpenAI's own reasoning models are known to take `reasoning_effort`; other servers speaking the
        // protocol can get custom variants from config.
        Protocol::OpenAiChat if provider_id == "openai" && reasoning => own(openai_levels(protocol, model)),
        Protocol::OpenAiResponses if reasoning => own(openai_levels(protocol, model)),
        Protocol::Gemini if reasoning => {
            if model.contains("gemini-3") { own(&["low", "high"]) } else { own(&["low", "medium", "high", "max"]) }
        }
        _ => vec![],
    }
}

/// Merge `overlay` into `base`: objects merge key by key, `null` removes a key, anything else replaces.
pub fn merge_json(base: &mut Value, overlay: &Value) {
    match (base.as_object_mut(), overlay.as_object()) {
        (Some(b), Some(o)) => {
            for (k, v) in o {
                if v.is_null() {
                    b.remove(k);
                } else if let Some(slot) = b.get_mut(k) {
                    merge_json(slot, v);
                } else {
                    b.insert(k.clone(), v.clone());
                }
            }
        }
        _ => *base = overlay.clone(),
    }
}

fn finish(body: &mut Value, req: &Request) {
    if let Some(o) = &req.overlay.body {
        merge_json(body, o);
    }
}

pub fn shape_anthropic(body: &mut Value, req: &Request) {
    let effort = req.effort.as_deref();
    match anthropic_kind(&req.model, true) {
        Some(AnthropicKind::Adaptive { levels, always_on, off }) => match effort {
            Some("none") if off == Off::Disabled => body["thinking"] = json!({"type": "disabled"}),
            Some("none") if off == Off::BetweenTools => body["thinking"] = json!({"type": "between_tools"}),
            Some(e) if levels.contains(&e) => {
                body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
                body["output_config"] = json!({"effort": e});
            }
            // Thinking happens anyway on these models; ask for readable summaries so it can be shown.
            _ if always_on => body["thinking"] = json!({"type": "adaptive", "display": "summarized"}),
            _ => {}
        },
        Some(AnthropicKind::Budget) => {
            if let Some(budget) = effort.and_then(budget_for) {
                body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
                // The budget must stay below max_tokens.
                let need = budget + 4_096;
                if body["max_tokens"].as_u64().unwrap_or(0) < need as u64 {
                    body["max_tokens"] = json!(need);
                }
            }
        }
        None => {}
    }
    finish(body, req);
}

pub fn shape_openai_chat(body: &mut Value, req: &Request) {
    if let Some(e) = req.effort.as_deref() {
        body["reasoning_effort"] = json!(e);
    }
    finish(body, req);
}

pub fn shape_openai_responses(body: &mut Value, req: &Request) {
    match req.effort.as_deref() {
        // The API also accepts a summary with `none`, but there is nothing to summarize.
        Some("none") => body["reasoning"] = json!({"effort": "none"}),
        Some(e) => body["reasoning"] = json!({"effort": e, "summary": "auto"}),
        None => {}
    }
    finish(body, req);
}

pub fn shape_gemini(body: &mut Value, req: &Request) {
    if let Some(e) = req.effort.as_deref() {
        let cfg = if req.model.contains("gemini-3") {
            json!({"thinkingLevel": e, "includeThoughts": true})
        } else {
            let budget = match e {
                "low" => 1_024,
                "medium" => 8_192,
                "high" => 24_576,
                _ => -1, // dynamic
            };
            json!({"thinkingBudget": budget, "includeThoughts": true})
        };
        body["generationConfig"]["thinkingConfig"] = cfg;
    }
    finish(body, req);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(model: &str, effort: Option<&str>) -> Request {
        Request { model: model.into(), effort: effort.map(String::from), max_tokens: 8_000, ..Default::default() }
    }

    fn names(v: Vec<String>) -> String {
        v.join(",")
    }

    #[test]
    fn anthropic_models_offer_their_own_levels() {
        let a = |m: &str, r: bool| names(variants(Protocol::Anthropic, "anthropic", m, r));
        assert_eq!(a("claude-opus-5-5", true), "low,medium,high,xhigh,max", "thinking cannot be switched off there");
        assert_eq!(a("claude-sonnet-5-5", true), "none,low,medium,high,xhigh,max");
        assert_eq!(a("claude-fable-5-1", true), "low,medium,high,xhigh,max");
        assert_eq!(a("claude-opus-4-6", true), "none,low,medium,high,max", "no xhigh before 4.7");
        assert_eq!(a("claude-opus-4-8", false), "none,low,medium,high,xhigh,max", "known families ignore a stale catalog flag");
        assert_eq!(a("claude-haiku-4-5", true), "none,low,medium,high,max");
        assert_eq!(a("claude-3-5-haiku-latest", false), "", "a model that does not reason has no levels");
    }

    #[test]
    fn other_protocols_and_unknown_servers() {
        assert_eq!(names(variants(Protocol::OpenAiChat, "openrouter", "x", true)), "", "only OpenAI itself is known to take reasoning_effort");
        assert_eq!(names(variants(Protocol::OpenAiChat, "openai", "gpt-4o", false)), "");
        assert_eq!(names(variants(Protocol::OpenAiResponses, "azure", "o4-mini", true)), "low,medium,high");
        assert_eq!(names(variants(Protocol::Gemini, "google", "gemini-2.5-pro", true)), "low,medium,high,max");
        assert_eq!(names(variants(Protocol::Gemini, "google", "gemini-3-pro", true)), "low,high");
    }

    #[test]
    fn openai_models_offer_their_own_levels() {
        let both = |m: &str| {
            let chat = names(variants(Protocol::OpenAiChat, "openai", m, true));
            let resp = names(variants(Protocol::OpenAiResponses, "openai", m, true));
            format!("{chat} | {resp}")
        };
        let mmh = "minimal,low,medium,high";
        assert_eq!(both("gpt-5"), format!("{mmh} | {mmh}"));
        assert_eq!(both("gpt-5-mini"), format!("{mmh} | {mmh}"));
        assert_eq!(both("gpt-5-nano-2025-08-07"), format!("{mmh} | {mmh}"), "dated ids are the same family");
        assert_eq!(both("gpt-5-pro"), "high | high");
        assert_eq!(both("gpt-5.1"), "none,low,medium,high | none,low,medium,high");
        let x = "none,low,medium,high,xhigh";
        for m in ["gpt-5.2", "gpt-5.3-codex", "gpt-5.4", "gpt-5.4-mini", "gpt-5.4-nano", "gpt-5.5-2026-04-23"] {
            assert_eq!(both(m), format!("{x} | {x}"), "{m}");
        }
        for m in ["gpt-5.2-pro", "gpt-5.4-pro", "gpt-5.5-pro"] {
            assert_eq!(both(m), "medium,high,xhigh | medium,high,xhigh", "{m}");
        }
        for m in ["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-6-luna", "gpt-6-sol", "gpt-6.1-terra"] {
            assert_eq!(both(m), format!("{x} | {x},max"), "{m}: max only on Responses");
        }
        for m in ["gpt-6-astra", "gpt-6.1-sol"] {
            assert_eq!(both(m), "low,medium,high,xhigh | low,medium,high,xhigh,max", "{m}: reasoning cannot be switched off");
        }
        assert_eq!(both("o3"), "low,medium,high | low,medium,high");
        // Not probed: newer families get the newest family's levels, everything else the safe three.
        assert_eq!(both("gpt-6.2-terra"), format!("{x} | {x},max"));
        assert_eq!(both("gpt-7"), format!("{x} | {x},max"));
        assert_eq!(both("GPT-5.7-Mini"), format!("{x} | {x},max"), "case does not matter");
        assert_eq!(both("gpt-6.2-pro"), "medium,high,xhigh | medium,high,xhigh");
        assert_eq!(both("o1"), "low,medium,high | low,medium,high");
        assert_eq!(both("gpt-oss-120b"), "low,medium,high | low,medium,high");
        assert_eq!(both("my-azure-deployment"), "low,medium,high | low,medium,high");
        assert_eq!(names(variants(Protocol::OpenAiChat, "openai", "gpt-6-luna", false)), "", "the catalog's reasoning flag still gates");
    }

    #[test]
    fn anthropic_bodies() {
        let shaped = |m: &str, e: Option<&str>| {
            let mut b = json!({"model": m, "max_tokens": 8_000});
            shape_anthropic(&mut b, &req(m, e));
            b
        };
        let adaptive = json!({"type": "adaptive", "display": "summarized"});
        let b = shaped("claude-opus-5-5", Some("high"));
        assert_eq!((&b["thinking"], &b["output_config"]), (&adaptive, &json!({"effort": "high"})));
        let b = shaped("claude-opus-5-5", None);
        assert_eq!((&b["thinking"], b.get("output_config")), (&adaptive, None), "always-on models get readable summaries, no effort");
        assert_eq!(shaped("claude-opus-4-8", None).get("thinking"), None, "4.8 runs without thinking unless asked");
        assert_eq!(shaped("claude-opus-4-8", Some("none"))["thinking"], json!({"type": "disabled"}));
        assert_eq!(shaped("claude-sonnet-5-5", Some("none"))["thinking"], json!({"type": "between_tools"}));
        assert_eq!(shaped("claude-opus-5-5", Some("none"))["thinking"], adaptive, "cannot be turned off: ignored");
        assert_eq!(shaped("claude-opus-4-6", Some("xhigh")).get("thinking"), None, "unknown level for the model: ignored");
        let b = shaped("claude-haiku-4-5", Some("high"));
        assert_eq!((&b["thinking"], &b["max_tokens"]), (&json!({"type": "enabled", "budget_tokens": 16_384}), &json!(20_480)), "max_tokens is raised above the budget");
        assert_eq!(shaped("claude-haiku-4-5", Some("low"))["max_tokens"], 8_000);
        assert_eq!(shaped("claude-haiku-4-5", Some("none")).get("thinking"), None);
    }

    #[test]
    fn other_bodies_and_overlays() {
        let mut b = json!({"model": "o3"});
        shape_openai_chat(&mut b, &req("o3", Some("low")));
        assert_eq!(b["reasoning_effort"], "low");
        let mut b = json!({"model": "o3"});
        shape_openai_responses(&mut b, &req("o3", Some("high")));
        assert_eq!(b["reasoning"], json!({"effort": "high", "summary": "auto"}));
        let mut b = json!({"model": "gpt-6-luna"});
        shape_openai_responses(&mut b, &req("gpt-6-luna", Some("none")));
        assert_eq!(b["reasoning"], json!({"effort": "none"}), "no summary when reasoning is off");
        let mut b = json!({"model": "gpt-6-luna"});
        shape_openai_responses(&mut b, &req("gpt-6-luna", None));
        assert_eq!(b.get("reasoning"), None, "no level: the API's default");
        let mut b = json!({"model": "gpt-6-luna"});
        shape_openai_chat(&mut b, &req("gpt-6-luna", Some("none")));
        assert_eq!(b["reasoning_effort"], "none", "the only way to use tools with these models on Chat (archive#185)");
        let mut b = json!({"generationConfig": {"maxOutputTokens": 5}});
        shape_gemini(&mut b, &req("gemini-2.5-pro", Some("medium")));
        assert_eq!(b["generationConfig"], json!({"maxOutputTokens": 5, "thinkingConfig": {"thinkingBudget": 8192, "includeThoughts": true}}));
        let mut b = json!({"generationConfig": {}});
        shape_gemini(&mut b, &req("gemini-3-pro", Some("high")));
        assert_eq!(b["generationConfig"]["thinkingConfig"], json!({"thinkingLevel": "high", "includeThoughts": true}));
        // Overlays merge last: objects deeply, null removes, the rest replaces.
        let mut r = req("o3", None);
        r.overlay.body = Some(json!({"temperature": 0.2, "stream_options": {"include_usage": null}, "extra": {"a": 1}}));
        let mut b = json!({"model": "o3", "stream_options": {"include_usage": true, "x": 1}, "extra": {"b": 2}});
        shape_openai_chat(&mut b, &r);
        assert_eq!(b, json!({"model": "o3", "temperature": 0.2, "stream_options": {"x": 1}, "extra": {"a": 1, "b": 2}}));
    }
}
