//! Context compaction (docs/03-agent-loop.md "Compaction").
//!
//! A `compaction` message stores `{summary, keepFrom, trigger}`: everything before message `keepFrom` is
//! replaced by the summary; messages from `keepFrom` on stay verbatim. A later compaction summarizes the
//! previous summary plus what followed, so there is always at most one checkpoint.
use crate::store::Message;
use serde_json::json;

/// How much room to keep free below the model's context limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Buffer {
    Tokens(u64),
    Percent(f64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub auto: bool,
    pub buffer: Buffer,
    /// Tokens of recent conversation kept verbatim.
    pub keep: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { auto: true, buffer: Buffer::Percent(15.0), keep: 20_000 }
    }
}

/// Never leave less than this free, whatever the buffer says (room for the next answer).
pub const MIN_FREE: u64 = 16_000;

impl Settings {
    pub fn from_config(c: &crate::config::CompactionConfig) -> Settings {
        let d = Settings::default();
        let buffer = match &c.buffer {
            Some(serde_json::Value::Number(n)) => n.as_u64().map(Buffer::Tokens).unwrap_or(d.buffer),
            Some(serde_json::Value::String(s)) => s.trim().trim_end_matches('%').parse::<f64>().ok().map(Buffer::Percent).unwrap_or(d.buffer),
            _ => d.buffer,
        };
        Settings { auto: c.auto.unwrap_or(d.auto), buffer, keep: c.keep.unwrap_or(d.keep) }
    }

    /// Prompt size above which a compaction should run, for a model with this context limit.
    /// With `MIN_FREE` larger than the whole window (tiny models) the threshold bottoms out at half the limit.
    pub fn threshold(&self, limit: u64) -> u64 {
        let free = match self.buffer {
            Buffer::Tokens(t) => t,
            Buffer::Percent(p) => (limit as f64 * p / 100.0) as u64,
        };
        let free = free.max(MIN_FREE.min(limit / 2));
        limit.saturating_sub(free).max(limit / 2)
    }
}

pub const SUMMARY_PREFIX: &str = "The conversation so far was compacted into the checkpoint below. Continue the work from it.\n\n";

/// Rough token count of a piece of text.
pub fn estimate_text(t: &str) -> u64 {
    (t.len() as u64).div_ceil(4)
}

/// Rough size of a whole request: system prompt, tool definitions and messages.
pub fn estimate_request(req: &crate::provider::Request) -> u64 {
    use crate::provider::Block;
    let tools: usize = req.tools.iter().map(|t| t.name.len() + t.description.len() + t.schema.to_string().len()).sum();
    let msgs: usize = req
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .map(|b| match b {
            Block::Text(t) => t.len(),
            Block::Image { .. } => 4 * crate::media::tokens("image/png", 0) as usize,
            Block::Document { media_type, data, .. } => 4 * crate::media::tokens(media_type, data.len() as u64 * 3 / 4) as usize,
            Block::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
            Block::ToolResult { content, .. } => content.len(),
        })
        .sum();
    ((req.system.len() + tools + msgs) as u64).div_ceil(4)
}

/// Rough token count (about 4 characters per token).
pub fn estimate(m: &Message) -> u64 {
    (m.data.to_string().len() as u64).div_ceil(4) + 4 + media_tokens(&m.data)
}

/// Tokens for the attachments referenced anywhere in a message (the references themselves are tiny).
fn media_tokens(v: &serde_json::Value) -> u64 {
    match v {
        serde_json::Value::Object(o) => {
            let own = if o.contains_key("blob") { crate::media::tokens(o.get("mediaType").and_then(|m| m.as_str()).unwrap_or(""), o.get("size").and_then(|s| s.as_u64()).unwrap_or(0)) } else { 0 };
            own + o.values().map(media_tokens).sum::<u64>()
        }
        serde_json::Value::Array(a) => a.iter().map(media_tokens).sum(),
        _ => 0,
    }
}

/// History as the model sees it: after the latest compaction, `[summary, messages from keepFrom on]`.
pub fn effective(messages: &[Message]) -> Vec<Message> {
    let Some(i) = messages.iter().rposition(|m| m.kind == "compaction") else { return messages.to_vec() };
    let c = &messages[i];
    let keep_from = c.data["keepFrom"].as_str().unwrap_or("");
    let summary = Message {
        id: c.id.clone(),
        session_id: c.session_id.clone(),
        kind: "synthetic".into(),
        data: json!({ "text": format!("{SUMMARY_PREFIX}{}", c.data["summary"].as_str().unwrap_or("")), "summary": true }),
    };
    std::iter::once(summary).chain(messages.iter().filter(|m| m.kind != "compaction" && m.id.as_str() >= keep_from).cloned()).collect()
}

pub fn has_checkpoint(eff: &[Message]) -> bool {
    eff.first().is_some_and(|m| m.data["summary"] == true)
}

/// Where the verbatim tail starts in `eff` (an index), or `None` if there is nothing worth compacting.
/// The tail is the longest suffix within `keep` tokens, and always contains the last message.
pub fn choose_cut(eff: &[Message], keep: u64) -> Option<usize> {
    let (mut acc, mut k) = (0u64, eff.len());
    for i in (0..eff.len()).rev() {
        let t = estimate(&eff[i]);
        if k < eff.len() && acc + t > keep {
            break;
        }
        acc += t;
        k = i;
    }
    let first_real = usize::from(has_checkpoint(eff));
    (k > first_real).then_some(k)
}

/// Current prompt size: the provider-reported total of the latest step after the checkpoint plus an estimate
/// of what came since; with no report yet, an estimate of the whole request.
pub fn context_size(eff: &[Message], request_estimate: u64) -> u64 {
    let Some(i) = eff.iter().rposition(|m| m.kind == "assistant" && m.data["usage"].is_object()) else { return request_estimate };
    let u = &eff[i].data["usage"];
    let reported: u64 = ["input", "output", "cacheRead", "cacheWrite"].iter().map(|k| u[*k].as_u64().unwrap_or(0)).sum();
    reported + eff[i + 1..].iter().map(estimate).sum::<u64>()
}

pub const SYSTEM: &str = "You are a summarization assistant inside a coding agent. You write checkpoints that let another agent resume \
the work after earlier conversation is removed. You never call tools and never continue the task yourself.";

pub const TEMPLATE: &str = "## Objective
One or two brief sentences.
## Requirements
Constraints, preferences and scope stated by the user, or (none).
## Decisions
Decisions already made and why, or (none).
## Work State
### Completed
### Active
### Blocked
## Next Move
1. Ordered next actions, or (none).
## Relevant Files
At most 15, most important first: `path`: reason
## Important Context
Facts the next agent cannot recover on its own, or (none).";

pub const RULES: &str = "- Use terse, single-line bullets and prefer short references.
- Preserve exact paths, symbols, commands, error strings, URLs and identifiers.
- Carry forward only unanswered user requests, with their exact wording.
- Preserve workflow state (uncommitted, committed, pushed, under review, merged).
- Do not summarize assistant setup (AGENTS.md contents, session ID); those are supplied again.
- Do not mention the summary process. Do not continue the task or call tools.
- Return only the structured summary, using the section headings of the template.
- Sections may be omitted when not applicable.";

pub const NUDGE: &str = "The previous response did not fill in the required summary template. Do not call tools. Return the summary as text using the exact section headings from the template.";

/// Final user message of the summarization request.
pub fn prompt(update: bool) -> String {
    let lead = if update {
        "Update the existing checkpoint (the first message in the conversation above) with everything that happened since, into one \
consolidated summary. Newer history always takes precedence over older history; drop details that are no longer relevant."
    } else {
        "You MUST summarize the conversation above into a structured summary that will be given to another agent to resume the work."
    };
    format!("{lead}\n\nFill in this template:\n\n{TEMPLATE}\n\nRules:\n{RULES}")
}

/// Did the reply follow the template? (At least three of its main headings.)
pub fn follows_template(reply: &str) -> bool {
    ["## Objective", "## Requirements", "## Decisions", "## Work State", "## Next Move", "## Relevant Files", "## Important Context"]
        .iter()
        .filter(|h| reply.contains(**h))
        .count()
        >= 3
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CompactionConfig;

    fn msg(id: &str, kind: &str, data: serde_json::Value) -> Message {
        Message { id: id.into(), session_id: "s".into(), kind: kind.into(), data }
    }
    fn big(id: &str, kind: &str, tokens: usize) -> Message {
        msg(id, kind, json!({ "text": "x".repeat(tokens * 4) }))
    }

    #[test]
    fn threshold_buffer_and_floor() {
        let s = Settings::default();
        assert_eq!(s.threshold(200_000), 170_000, "15% free");
        assert_eq!(s.threshold(1_000_000), 850_000);
        assert_eq!(s.threshold(64_000), 64_000 - 16_000, "at least 16k free");
        assert_eq!(s.threshold(8_000), 4_000, "tiny windows never go below half");
        let t = Settings { buffer: Buffer::Tokens(30_000), ..s };
        assert_eq!(t.threshold(200_000), 170_000);
        assert_eq!(Settings { buffer: Buffer::Tokens(1), ..s }.threshold(200_000), 184_000, "floor of 16k still applies");
    }

    #[test]
    fn settings_from_config_values() {
        let c: CompactionConfig = serde_json::from_value(json!({"auto": false, "buffer": "20%", "keep": 5000})).unwrap();
        assert_eq!(Settings::from_config(&c), Settings { auto: false, buffer: Buffer::Percent(20.0), keep: 5000 });
        let c: CompactionConfig = serde_json::from_value(json!({"buffer": 30000})).unwrap();
        assert_eq!(Settings::from_config(&c).buffer, Buffer::Tokens(30_000));
        assert_eq!(Settings::from_config(&CompactionConfig::default()), Settings::default());
    }

    #[test]
    fn effective_history_replaces_the_past_with_the_checkpoint() {
        let m = vec![
            msg("m1", "user", json!({"text":"old"})),
            msg("m2", "assistant", json!({"parts":[]})),
            msg("m3", "user", json!({"text":"kept"})),
            msg("m4", "compaction", json!({"summary":"SUM","keepFrom":"m3"})),
            msg("m5", "assistant", json!({"parts":[]})),
        ];
        let e = effective(&m);
        let ids: Vec<_> = e.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, ["m4", "m3", "m5"]);
        assert!(e[0].data["text"].as_str().unwrap().ends_with("SUM") && has_checkpoint(&e));
        assert_eq!(effective(&m[..3]).len(), 3, "no compaction: unchanged");
        // A second compaction supersedes the first.
        let mut m2 = m.clone();
        m2.push(msg("m6", "compaction", json!({"summary":"SUM2","keepFrom":"m5"})));
        assert_eq!(effective(&m2).iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["m6", "m5"]);
    }

    #[test]
    fn cut_keeps_the_longest_suffix_within_budget() {
        let m: Vec<Message> = (1..=6).map(|i| big(&format!("m{i}"), if i % 2 == 1 { "user" } else { "assistant" }, 100)).collect();
        assert_eq!(choose_cut(&m, 250), Some(4), "two messages of ~104 fit in 250");
        assert_eq!(choose_cut(&m, 10), Some(5), "the last message is always kept");
        assert_eq!(choose_cut(&m, 100_000), None, "everything fits: nothing to compact");
    }

    #[test]
    fn cut_with_a_checkpoint_needs_new_material() {
        let mut e = vec![msg("c", "synthetic", json!({"text":"x","summary":true}))];
        e.push(big("m1", "assistant", 100));
        assert_eq!(choose_cut(&e, 10), None, "only the checkpoint precedes the tail");
        e.push(big("m2", "user", 100));
        assert_eq!(choose_cut(&e, 10), Some(2), "m1 can now be folded into the checkpoint");
    }

    #[test]
    fn attachments_count_by_size_not_by_their_reference_json() {
        let with = msg("m1", "user", json!({"text":"x","mentions":{"files":[{"kind":"media","mediaType":"image/png","blob":"h","size":4_000_000,"name":"a.png"}]}}));
        let without = msg("m2", "user", json!({"text":"x","mentions":{"files":[{"kind":"text","content":"y"}]}}));
        assert!(estimate(&with) >= estimate(&without) + 1_400 && estimate(&with) < estimate(&without) + 1_700, "{} vs {}", estimate(&with), estimate(&without));
        let pdf = msg("m3", "assistant", json!({"parts":[{"type":"tool","state":{"media":[{"mediaType":"application/pdf","blob":"h","size":1_200_000}]}}]}));
        assert!(estimate(&pdf) >= 100_000, "a big PDF is expensive: {}", estimate(&pdf));
    }

    #[test]
    fn context_size_prefers_reported_usage() {
        let with_usage = msg("a", "assistant", json!({"parts":[],"usage":{"input":1000,"output":200,"cacheRead":300,"cacheWrite":0}}));
        let after = big("u", "user", 50);
        assert_eq!(context_size(&[with_usage.clone(), after.clone()], 9999), 1500 + estimate(&after));
        assert_eq!(context_size(&[after], 9999), 9999, "no report yet: use the request estimate");
    }

    #[test]
    fn template_detection_and_prompts() {
        assert!(follows_template("## Objective\nx\n## Next Move\n1. y\n## Relevant Files\n- a"));
        assert!(!follows_template("Sure! Here is a summary of what we did."));
        assert!(prompt(false).starts_with("You MUST summarize") && prompt(true).starts_with("Update the existing checkpoint"));
        assert!(prompt(false).contains("## Relevant Files") && prompt(false).contains("Preserve exact paths"));
    }
}
