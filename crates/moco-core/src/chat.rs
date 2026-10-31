//! Stored messages -> provider messages.
//!
//! Stored shapes (`message.data`): user/synthetic `{text}`; assistant
//! `{parts:[{type:"text",text} | {type:"tool",id,name,input,state:{status,output|error}}], finish?, usage?, error?}`.
use crate::provider::{Block, ChatMessage, Role};
use crate::store::Message;
use serde_json::{json, Value};

pub const INTERRUPTED: &str = "Tool execution interrupted";

pub fn tool_part(id: &str, name: &str, input: &Value) -> Value {
    json!({"type":"tool","id":id,"name":name,"input":input,"state":{"status":"running"}})
}

fn push_user(out: &mut Vec<ChatMessage>, block: Block) {
    // Consecutive user content merges into one message (the API wants alternating roles).
    if let Some(last) = out.last_mut().filter(|m| m.role == Role::User) {
        last.content.push(block);
    } else {
        out.push(ChatMessage { role: Role::User, content: vec![block] });
    }
}

/// Fetches an attachment's bytes by blob hash.
pub type Loader<'a> = &'a dyn Fn(&str) -> Option<Vec<u8>>;

/// A stored media reference `{mediaType, blob, name}` as a content block (the attachment's bytes are loaded now).
pub fn media_block(r: &Value, load: Loader) -> Block {
    use base64::Engine as _;
    let name = r["name"].as_str().unwrap_or("attachment");
    let Some(bytes) = r["blob"].as_str().and_then(load) else { return Block::Text(format!("[attachment {name} is no longer available]")) };
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    let media_type = r["mediaType"].as_str().unwrap_or("application/octet-stream").to_string();
    if media_type == "application/pdf" { Block::Document { media_type, data, name: name.to_string() } } else { Block::Image { media_type, data } }
}

/// Conversion without attachment data (tests, and callers that never have any).
pub fn to_chat(messages: &[Message]) -> Vec<ChatMessage> {
    to_chat_with(messages, &|_| None)
}

pub fn to_chat_with(messages: &[Message], load: Loader) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = vec![];
    for m in messages {
        match m.kind.as_str() {
            "user" | "synthetic" => {
                if let Some(t) = m.data["text"].as_str().filter(|t| !t.is_empty()) {
                    push_user(&mut out, Block::Text(t.into()));
                }
                // Files, skills and agent hints the user attached with @ (resolved when the prompt was sent).
                for extra in crate::mentions::render_blocks(&m.data["mentions"], load) {
                    push_user(&mut out, extra);
                }
            }
            "assistant" => {
                let mut blocks = vec![];
                let mut results = vec![];
                let mut media = vec![];
                for p in m.data["parts"].as_array().into_iter().flatten() {
                    match p["type"].as_str() {
                        Some("text") => {
                            if let Some(t) = p["text"].as_str().filter(|t| !t.is_empty()) {
                                blocks.push(Block::Text(t.into()));
                            }
                        }
                        Some("tool") => {
                            let id = p["id"].as_str().unwrap_or("").to_string();
                            blocks.push(Block::ToolUse { id: id.clone(), name: p["name"].as_str().unwrap_or("").into(), input: p["input"].clone() });
                            let st = &p["state"];
                            // Images and PDFs a tool returned follow the results in the same user message.
                            for r in st["media"].as_array().into_iter().flatten() {
                                media.push(media_block(r, load));
                            }
                            let (content, is_error) = match st["status"].as_str() {
                                Some("completed") => (st["output"].as_str().unwrap_or("").to_string(), false),
                                Some("error") => (st["error"].as_str().unwrap_or("error").to_string(), true),
                                // Never finished (crash or interrupt): the API still needs a result.
                                _ => (INTERRUPTED.to_string(), true),
                            };
                            results.push(Block::ToolResult { id, content, is_error });
                        }
                        _ => {}
                    }
                }
                if blocks.is_empty() {
                    continue;
                }
                out.push(ChatMessage { role: Role::Assistant, content: blocks });
                for r in results.into_iter().chain(media) {
                    push_user(&mut out, r);
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(kind: &str, data: Value) -> Message {
        Message { id: "m".into(), session_id: "s".into(), kind: kind.into(), data }
    }

    #[test]
    fn tool_round_trip_and_merging() {
        let m = vec![
            msg("user", json!({"text":"fix it"})),
            msg("synthetic", json!({"text":"note"})),
            msg("assistant", json!({"parts":[
                {"type":"text","text":"ok"},
                {"type":"tool","id":"t1","name":"read","input":{"path":"a"},"state":{"status":"completed","output":"1: x"}},
                {"type":"tool","id":"t2","name":"shell","input":{"command":"x"},"state":{"status":"error","error":"boom"}},
                {"type":"tool","id":"t3","name":"glob","input":{},"state":{"status":"running"}}]})),
            msg("assistant", json!({"parts":[{"type":"text","text":"done"}]})),
        ];
        let c = to_chat(&m);
        assert_eq!(c.len(), 4);
        assert_eq!(c[0].content, [Block::Text("fix it".into()), Block::Text("note".into())], "user + synthetic merge");
        assert_eq!(c[1].role, Role::Assistant);
        assert_eq!(
            c[2].content,
            [
                Block::ToolResult { id: "t1".into(), content: "1: x".into(), is_error: false },
                Block::ToolResult { id: "t2".into(), content: "boom".into(), is_error: true },
                Block::ToolResult { id: "t3".into(), content: INTERRUPTED.into(), is_error: true },
            ]
        );
        assert_eq!(c[3].content, [Block::Text("done".into())]);
    }

    #[test]
    fn mentions_follow_the_typed_text_in_the_same_message() {
        let m = vec![msg("user", json!({"text":"fix @a.txt","mentions":{"files":[{"path":"a.txt","kind":"file","content":"hello","truncated":false}],"skills":[],"agents":[]}}))];
        let c = to_chat(&m);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].content, [Block::Text("fix @a.txt".into()), Block::Text("<file path=\"a.txt\">\nhello\n</file>".into())]);
        // A mention with no typed text still produces a message.
        let only = to_chat(&[msg("user", json!({"text":"","mentions":{"files":[{"path":"b","kind":"file","content":"x"}]}}))]);
        assert_eq!(only[0].content.len(), 1);
    }

    #[test]
    fn media_from_tools_and_mentions_become_blocks_after_the_results() {
        let png = vec![1u8, 2, 3];
        let load = |h: &str| if h == "known" { Some(png.clone()) } else { None };
        let m = vec![
            msg("user", json!({"text":"see","mentions":{"files":[{"path":"a.pdf","kind":"media","mediaType":"application/pdf","blob":"known","size":3,"name":"a.pdf"}]}})),
            msg("assistant", json!({"parts":[
                {"type":"tool","id":"t1","name":"read","input":{},"state":{"status":"completed","output":"image file x","media":[{"mediaType":"image/png","blob":"known","size":3,"name":"x.png"}]}},
                {"type":"tool","id":"t2","name":"read","input":{},"state":{"status":"completed","output":"gone","media":[{"mediaType":"image/png","blob":"lost","size":3,"name":"y.png"}]}}]})),
        ];
        let c = to_chat_with(&m, &load);
        assert_eq!(c[0].content[0], Block::Text("see".into()));
        assert!(matches!(&c[0].content[1], Block::Text(t) if t.contains("a.pdf")), "{:?}", c[0].content[1]);
        assert!(matches!(&c[0].content[2], Block::Document { media_type, data, name } if media_type == "application/pdf" && data == "AQID" && name == "a.pdf"));
        let results = &c[2].content;
        assert!(matches!(&results[0], Block::ToolResult { id, .. } if id == "t1") && matches!(&results[1], Block::ToolResult { id, .. } if id == "t2"));
        assert!(matches!(&results[2], Block::Image { media_type, data } if media_type == "image/png" && data == "AQID"), "images come after every result");
        assert!(matches!(&results[3], Block::Text(t) if t == "[attachment y.png is no longer available]"), "a missing blob degrades to a note, not an error");
        assert_eq!(to_chat(&m)[0].content.len(), 3, "without a loader the blocks are placeholders");
    }

    #[test]
    fn empty_assistant_messages_are_dropped() {
        let c = to_chat(&[msg("user", json!({"text":"hi"})), msg("assistant", json!({"parts":[]}))]);
        assert_eq!(c.len(), 1);
    }
}
