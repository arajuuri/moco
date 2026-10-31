//! Model Context Protocol client (docs/08 "MCP"): servers started from config, their tools, prompts and resources.
pub mod client;
pub mod manager;
pub mod trust;

pub use manager::{Manager, PromptInfo, ResourceInfo, ServerInfo, ServerSpec, Status};
