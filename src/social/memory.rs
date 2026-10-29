use super::SocialConfig;
use crate::memory::group::GroupMemoryStore;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Group and private stores use the same policy but never share data.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemorySettings {
    pub enabled: bool,
    pub automatic_capture: bool,
    pub archive_hour: u32,
    pub archive_minute: u32,
    pub archive_every_days: u32,
    pub short_term_days: u32,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            automatic_capture: true,
            archive_hour: 4,
            archive_minute: 0,
            archive_every_days: 1,
            short_term_days: 14,
        }
    }
}

impl MemorySettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.archive_hour < 24
                && self.archive_minute < 60
                && (1..=30).contains(&self.archive_every_days)
                && (1..=90).contains(&self.short_term_days),
            "memory_settings_invalid"
        );
        Ok(())
    }
}

pub(super) fn private_targets(
    config: &SocialConfig,
    mut targets: Vec<String>,
) -> Result<Vec<String>> {
    if !config.private_enabled || !config.memory.enabled {
        return Ok(targets);
    }
    let directory = config.data_dir.join("private/groups");
    if !directory.exists() {
        return Ok(targets);
    }
    super::store::private_directory(&directory)?;
    let mut observed = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        ensure!(entry.file_type()?.is_file(), "memory_private_entry_invalid");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("memory_private_entry_invalid"))?;
        ensure!(
            !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "memory_private_entry_invalid"
        );
        if config.private_allowed_users.is_empty()
            || config.private_allowed_users.iter().any(|user| user == id)
        {
            observed.push(id.to_owned());
        }
    }
    observed.sort();
    for id in observed {
        if !targets.contains(&id) && targets.len() < config.max_private_sessions {
            let store = super::memory_agent::store(config, &id, true)?;
            let view = store.ledger_view()?;
            let pending = view.drafts.iter().any(|draft| {
                matches!(draft.status, crate::memory::group::DraftStatus::Buffered)
                    && draft.data.disposition == crate::memory::group::Disposition::Retain
                    && draft.data.certainty != crate::memory::group::Certainty::Uncertain
                    && matches!(
                        draft.data.operation,
                        crate::memory::group::MemoryOperation::Append { .. }
                    )
            });
            let unscanned = config.memory.automatic_capture
                && !super::history::History::new(&config.data_dir.join("private"), &id)?
                    .reader()?
                    .list(view.maintenance.history_cursor, 1, None, None, None)?
                    .items
                    .is_empty();
            if pending || unscanned {
                targets.push(id);
            }
        }
    }
    Ok(targets)
}

/// Operator-only JSON view. No group/member raw identifiers in this output.
pub fn memory_view(config: &SocialConfig, index: usize) -> Result<()> {
    let group = config
        .allowed_groups
        .get(index)
        .ok_or_else(|| anyhow::anyhow!("group_index_out_of_range"))?;
    let view = GroupMemoryStore::new(&config.data_dir, group)?.view()?;
    println!("{}", serde_json::to_string(&view)?);
    Ok(())
}

/// Local, deterministic maintenance; no model, QQ or external network calls.
pub fn maintain_memories(config: &SocialConfig, index: Option<usize>) -> Result<()> {
    if !config.memory.enabled {
        println!(
            "{}",
            serde_json::json!({"groups":0,"applied":0,"failed":0,"disabled":true})
        );
        return Ok(());
    }
    if let Some(index) = index {
        ensure!(
            index < config.allowed_groups.len(),
            "group_index_out_of_range"
        );
    }
    let mut applied = 0;
    let mut groups = 0;
    let mut failed = 0;
    for (position, group) in config.allowed_groups.iter().enumerate() {
        if index.is_some_and(|index| index != position) {
            continue;
        }
        let maintenance = || -> Result<usize> {
            let now = chrono::Utc::now().timestamp();
            let store = GroupMemoryStore::new(&config.data_dir, group)?;
            let count = store.maintain(now)?.applied;
            if config.memory.enabled && super::memory_nightly::due(config, &store, now)?.is_some() {
                store.archive_ready(now)?;
            }
            Ok(count)
        };
        match maintenance() {
            Ok(count) => applied += count,
            Err(_) => failed += 1,
        }
        groups += 1;
    }
    println!(
        "{}",
        serde_json::json!({"groups":groups,"applied":applied,"failed":failed})
    );
    ensure!(failed == 0, "group_memory_maintenance_failed");
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OperatorInput {
    action: String,
    target: String,
    #[serde(default)]
    private: bool,
    id: Option<u64>,
    revision: Option<u64>,
    content: Option<String>,
}

/// Authenticated operator CLI. Payload travels over stdin, never shell arguments
/// or logs. The web layer must authenticate before invoking it.
pub fn memory_operation(config: &SocialConfig) -> Result<()> {
    use std::io::Read;
    let mut raw = Vec::new();
    std::io::stdin().take(32769).read_to_end(&mut raw)?;
    ensure!(raw.len() <= 32768, "memory_operation_too_large");
    let input: OperatorInput = serde_json::from_slice(&raw)?;
    let output = operator(config, input)?;
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn operator(config: &SocialConfig, input: OperatorInput) -> Result<serde_json::Value> {
    ensure!(
        if input.private {
            config.private_enabled
                && (config.private_allowed_users.is_empty()
                    || config.private_allowed_users.contains(&input.target))
        } else {
            config.allowed_groups.contains(&input.target)
        },
        "memory_scope_not_allowed"
    );
    let store = super::memory_agent::store(config, &input.target, input.private)?;
    let now = chrono::Utc::now().timestamp();
    let version = || -> Result<(u64, u64)> {
        Ok((
            input
                .id
                .filter(|id| *id > 0)
                .ok_or_else(|| anyhow::anyhow!("memory_id_required"))?,
            input
                .revision
                .filter(|r| *r > 0)
                .ok_or_else(|| anyhow::anyhow!("memory_revision_required"))?,
        ))
    };
    match input.action.as_str() {
        "view" => {}
        "archive" => {
            store.maintain(now)?;
            store.archive_ready(now)?;
        }
        "edit" => {
            let (id, rev) = version()?;
            store.edit_content(id, rev, input.content.as_deref().unwrap_or(""), now)?;
        }
        "reject" => {
            let (id, rev) = version()?;
            store.reject_draft(id, rev, now)?;
        }
        "apply" => {
            let (id, rev) = version()?;
            store.apply_operator(id, rev, now)?;
        }
        "correct" => {
            let (id, rev) = version()?;
            store.propose_record_correction(
                id,
                rev,
                input.content.as_deref().unwrap_or(""),
                now,
            )?;
        }
        _ => anyhow::bail!("memory_action_invalid"),
    }
    Ok(
        serde_json::json!({"legacy":store.view()?,"ledger":store.ledger_view()?,"layout":store.memory_layout(),"settings":config.memory}),
    )
}
