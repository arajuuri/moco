//! Model Context Protocol client (docs/08 "MCP"): servers started from config, their tools, prompts and resources.
pub mod client;
pub mod http;
pub mod manager;
pub mod oauth;
pub mod trust;

pub use manager::{Manager, PromptInfo, ResourceInfo, ServerInfo, ServerSpec, Status};
