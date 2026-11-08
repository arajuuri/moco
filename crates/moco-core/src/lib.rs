//! moco engine: UI-independent core (docs/00-overview.md).
#![allow(clippy::type_complexity)]
#![allow(unstable_name_collisions)] // compat.rs back-ports std methods for Rust 1.73 (macOS 10.7)
#![allow(clippy::format_collect, clippy::needless_borrow, clippy::search_is_some, clippy::single_char_pattern)] // clippy 1.73 lints that later clippy (main) dropped or made opt-in
pub mod agent;
pub mod auth;
pub mod blobs;
pub mod catalog;
pub mod classifier;
pub mod commands;
pub mod compat;
pub mod chat;
pub mod check;
pub mod compaction;
pub mod config;
pub mod engine;
pub mod export;
pub mod format;
pub mod id;
pub mod instructions;
pub mod log;
pub mod markdown;
pub mod mcp;
pub mod media;
pub mod mentions;
pub mod permission;
pub mod provider;
pub mod sandbox;
pub mod saved_permissions;
pub mod skills;
pub mod snapshot;
pub mod store;
pub mod suggest;
pub mod sysinfo;
pub mod title;
pub mod tools;
pub mod truncate;
pub mod version;
pub mod wildcard;
