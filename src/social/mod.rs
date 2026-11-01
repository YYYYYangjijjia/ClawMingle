//! Text-only social pilot. This entry never imports the operator's Codex profile.

mod backup;
mod batching;
mod behavior;
mod capabilities;
mod capacity;
mod command_menu;
mod commands;
mod config;
mod context_input;
mod control;
mod dispatch;
pub(crate) mod documents;
mod input_budget;
pub use documents::admin_operation as documents_admin_operation;
pub use documents::schema as document_tool_schema;
mod history;
mod image_delivery;
mod image_inputs;
mod images;
mod integration_control;
mod interaction;
mod jev;
mod media;
mod memory;
mod memory_agent;
mod memory_context;
mod memory_control;
mod memory_nightly;
mod mentions;
mod message_context;
mod models;
mod native_faces;
mod native_runtime;
mod persona;
mod persona_control;
pub use persona_control::schema as identity_tool_schema;
mod group_profile;
pub use group_profile::{check_saved as check_group_profiles, schema as group_profile_tool_schema};
mod qq_info;
mod quotes;
mod reply;
mod reply_reference;
mod runtime;
mod skill_control;
mod skill_install;
mod skill_package;
mod state;
mod steer;
mod sticker_agent;
mod stickers;
mod store;
mod task_feedback;
mod task_identity;
mod task_status;
mod tasks;
mod tool_catalog;
mod tool_control;
mod traffic;
mod voice;

pub use backup::{
    BackupConfig, create_backup, export_backup, list_backups, query_backup, verify_backup,
};
pub use behavior::behavior_settings;
pub use commands::catalog as command_catalog;
pub use config::SocialConfig;
pub use memory::{maintain_memories, memory_operation, memory_view};
pub use models::{AvailableModels, capability_catalog, check_integrations, load_available_models};
pub use native_runtime::resolve_codex_binary;
pub use runtime::run;
pub use stickers::{StickerSettings, sticker_operation};
pub use store::{resolve_delivery, retry_failed, show_status};

#[cfg(test)]
mod tests;

pub use sticker_agent::{
    schema as sticker_tool_schema, tool_action_allowed as sticker_tool_action_allowed,
};
