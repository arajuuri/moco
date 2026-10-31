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

pub fn to_chat(messages: &[Message]) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = vec![];
    for m in messages {
        match m.kind.as_str() {
            "user" | "synthetic" => {
                if let Some(t) = m.data["text"].as_str().filter(|t| !t.is_empty()) {
                    push_user(&mut out, Block::Text(t.into()));
                }
            }
            "assistant" => {
                let mut blocks = vec![];
                let mut results = vec![];
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
                for r in results {
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
    fn empty_assistant_messages_are_dropped() {
        let c = to_chat(&[msg("user", json!({"text":"hi"})), msg("assistant", json!({"parts":[]}))]);
        assert_eq!(c.len(), 1);
    }
}
