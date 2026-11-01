//! Group-scoped, explicitly confirmed memory, independent of personal memory.
//!
//! `store.json` is the sole canonical transaction boundary. `authority.json`
//! and `documents/{changes.json,CHANGES.md}` are disposable read-only mirrors;
//! edits to them are never imported. No model, scheduler, or QQ API is called.
//! This store is synchronous: async callers should use a blocking worker.

use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fmt,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

mod ledger;
pub use ledger::*;

pub const MAX_STORE_BYTES: usize = 1024 * 1024;
pub const MAX_TEXT_CHARS: usize = 2000;
pub const MAX_TEXT_BYTES: usize = 8 * 1024;
pub const MAX_PENDING_CHANGES: usize = 128;
pub const MAX_FACTS: usize = 256;
pub const MAX_FACT_REVISIONS: usize = 16;
pub const MAX_INJECTION_BYTES: usize = 8 * 1024;
pub const MAX_FACTS_PER_SUBJECT: usize = 10;
pub const MAX_INJECTION_FACTS: usize = 30;

// The deployment target is Linux. O_NONBLOCK also prevents an attacker-created
// FIFO from blocking before fstat rejects it. No new libc dependency is needed.
const O_NOFOLLOW: i32 = 0o400000;
const O_DIRECTORY: i32 = 0o200000;
const O_NONBLOCK: i32 = 0o4000;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

unsafe extern "C" {
    fn geteuid() -> u32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupMemoryError {
    InvalidInput,
    InvalidBudget,
    NotFound,
    NotOwner,
    StaleRevision,
    NotPending,
    AlreadyApplied,
    ConflictingReplacement,
    PendingLimit,
    FactLimit,
    RevisionLimit,
    StoreLimit,
    UnsafePath,
    CorruptStore,
    Io,
    /// The canonical commit succeeded. Reopen/retry to rebuild the mirrors.
    MirrorsOutOfDate,
}

impl fmt::Display for GroupMemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "invalid group memory input",
            Self::InvalidBudget => "invalid group memory injection budget",
            Self::NotFound => "group memory record not found",
            Self::NotOwner => "only the subject may change this record",
            Self::StaleRevision => "group memory revision is stale",
            Self::NotPending => "group memory change is not pending",
            Self::AlreadyApplied => "group memory change is already applied",
            Self::ConflictingReplacement => "a replacement is already pending for this fact",
            Self::PendingLimit => "group memory pending change limit reached",
            Self::FactLimit => "group memory fact limit reached",
            Self::RevisionLimit => "group memory fact revision limit reached",
            Self::StoreLimit => "group memory file size limit reached",
            Self::UnsafePath => "unsafe group memory filesystem entry",
            Self::CorruptStore => "invalid canonical group memory store",
            Self::Io => "group memory storage operation failed",
            Self::MirrorsOutOfDate => "group memory committed; derived mirrors need rebuilding",
        })
    }
}

impl std::error::Error for GroupMemoryError {}

type Result<T> = std::result::Result<T, GroupMemoryError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Pending,
    Confirmed,
    Applied,
    Withdrawn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// A digest, never the platform's raw message ID.
    pub message_hash: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub change_id: u64,
    /// Stable within this group; raw platform member IDs are never returned.
    pub subject: String,
    pub text: String,
    pub replace_fact_id: Option<u64>,
    pub source: Source,
    /// Proposal-content version. Status transitions do not change this value.
    pub revision: u64,
    pub status: ChangeStatus,
    pub confirmed_at: Option<i64>,
    pub applied_at: Option<i64>,
    pub applied_fact_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactRevision {
    pub revision: u64,
    pub text: String,
    pub source: Source,
    pub confirmed_at: i64,
    pub applied_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub fact_id: u64,
    pub subject: String,
    pub text: String,
    pub source: Source,
    pub confirmed_at: i64,
    /// Current live version; history includes every version, including this one.
    pub revision: u64,
    pub history: Vec<FactRevision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub authority_revision: u64,
    pub subjects: usize,
    pub facts: usize,
    pub pending: usize,
    pub confirmed: usize,
    pub applied_changes: usize,
    pub withdrawn: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaintenanceReport {
    pub applied: usize,
    pub authority_revision: u64,
}

/// Administrator view. It deliberately has no mutation or confirmation method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryView {
    pub summary: Summary,
    pub entries: Vec<Fact>,
    pub changes: Vec<Change>,
    pub last_maintained_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injection {
    /// Complete JSON, or empty when even the envelope does not fit the budget.
    /// Callers must treat all enclosed text as untrusted self-reported data.
    pub json: String,
    pub facts: usize,
    pub truncated: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredChange {
    owner_id: String,
    source_message_id: String,
    replace_revision: Option<u64>,
    change: Change,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredVersion {
    source_message_id: String,
    value: FactRevision,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFact {
    owner_id: String,
    fact_id: u64,
    current_revision: u64,
    versions: Vec<StoredVersion>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Canonical {
    schema: u32,
    group_hash: String,
    authority_revision: u64,
    #[serde(default)]
    last_maintained_at: Option<i64>,
    next_change: u64,
    next_fact: u64,
    facts: Vec<StoredFact>,
    changes: Vec<StoredChange>,
    #[serde(default, skip_serializing_if = "ledger::Ledger::is_empty")]
    ledger: ledger::Ledger,
}

#[derive(Serialize)]
struct AuthorityMirror {
    authority_revision: u64,
    facts: Vec<Fact>,
}

#[derive(Serialize)]
struct ChangesMirror<'a> {
    authority_revision: u64,
    changes: Vec<&'a Change>,
}

#[derive(Serialize)]
struct InjectionFact<'a> {
    fact_id: u64,
    subject: String,
    text: &'a str,
    source: &'a Source,
    confirmed_at: i64,
    revision: u64,
}

#[derive(Serialize)]
struct InjectionEnvelope<'a> {
    kind: &'static str,
    basis: &'static str,
    authority_revision: u64,
    facts: &'a [InjectionFact<'a>],
}

pub struct GroupMemoryStore {
    data_dir: PathBuf,
    group_hash: String,
    uid: u32,
}

impl GroupMemoryStore {
    pub fn new(data_dir: impl AsRef<Path>, group_id: &str) -> Result<Self> {
        validate_id(group_id)?;
        let path = data_dir.as_ref();
        // Configuration expansion belongs to the caller. Resolving a raw `~/...`
        // against cwd silently creates a second memory store in the repository.
        if !path.is_absolute() {
            return Err(GroupMemoryError::UnsafePath);
        }
        let data_dir = path.to_owned();
        // SAFETY: geteuid has no arguments and simply returns the process UID.
        let uid = unsafe { geteuid() };
        let store = Self {
            data_dir,
            group_hash: digest(group_id.as_bytes()),
            uid,
        };
        store.transact(|_| Ok(()))?;
        Ok(store)
    }

    /// Map a trusted event's author to the same group-scoped alias used in
    /// memory facts. This does not register a member or expose its raw ID.
    pub fn subject_alias(&self, author_id: &str) -> Result<String> {
        validate_id(author_id)?;
        Ok(self.alias(author_id))
    }

    /// The source author must come from the trusted QQ event, never model text.
    /// Repeating an identical source message is idempotent, including after an
    /// interrupted mirror write. A reused source ID with different input fails.
    pub fn propose(
        &self,
        author_id: &str,
        text: &str,
        replace_fact_id: Option<u64>,
        source_message_id: &str,
        source_timestamp: i64,
    ) -> Result<Change> {
        validate_id(author_id)?;
        validate_id(source_message_id)?;
        validate_text(text)?;
        if source_timestamp < 0 {
            return Err(GroupMemoryError::InvalidInput);
        }
        self.transact(|state| {
            if let Some(existing) = state.changes.iter().find(|entry| {
                entry.owner_id == author_id && entry.source_message_id == source_message_id
            }) {
                if existing.change.text == text
                    && existing.change.replace_fact_id == replace_fact_id
                    && existing.change.source.timestamp == source_timestamp
                {
                    return Ok(existing.change.clone());
                }
                return Err(GroupMemoryError::InvalidInput);
            }
            if active_changes(state).count() >= MAX_PENDING_CHANGES {
                return Err(GroupMemoryError::PendingLimit);
            }
            let replace_revision = if let Some(fact_id) = replace_fact_id {
                let fact = state
                    .facts
                    .iter()
                    .find(|fact| fact.fact_id == fact_id)
                    .ok_or(GroupMemoryError::NotFound)?;
                if fact.owner_id != author_id {
                    return Err(GroupMemoryError::NotOwner);
                }
                if fact.versions.len() >= MAX_FACT_REVISIONS {
                    return Err(GroupMemoryError::RevisionLimit);
                }
                if active_changes(state).any(|entry| entry.change.replace_fact_id == Some(fact_id))
                {
                    return Err(GroupMemoryError::ConflictingReplacement);
                }
                Some(fact.current_revision)
            } else {
                let reserved = active_changes(state)
                    .filter(|entry| entry.change.replace_fact_id.is_none())
                    .count();
                if state.facts.len() + reserved >= MAX_FACTS {
                    return Err(GroupMemoryError::FactLimit);
                }
                None
            };
            let change = Change {
                change_id: next_id(&self.group_hash, "change", &mut state.next_change)?,
                subject: self.alias(author_id),
                text: text.to_owned(),
                replace_fact_id,
                source: source(source_message_id, source_timestamp),
                revision: 1,
                status: ChangeStatus::Pending,
                confirmed_at: None,
                applied_at: None,
                applied_fact_id: None,
            };
            state.changes.push(StoredChange {
                owner_id: author_id.to_owned(),
                source_message_id: source_message_id.to_owned(),
                replace_revision,
                change: change.clone(),
            });
            Ok(change)
        })
    }

    pub fn confirm(&self, author_id: &str, change_id: u64, revision: u64) -> Result<Change> {
        validate_id(author_id)?;
        self.transact(|state| {
            let index = owned_change(state, author_id, change_id, revision)?;
            let entry = &state.changes[index];
            match entry.change.status {
                ChangeStatus::Confirmed | ChangeStatus::Applied => {
                    return Ok(entry.change.clone());
                }
                ChangeStatus::Withdrawn => return Err(GroupMemoryError::NotPending),
                ChangeStatus::Pending => {}
            }
            validate_replacement(state, entry)?;
            let change = &mut state.changes[index].change;
            change.status = ChangeStatus::Confirmed;
            change.confirmed_at = Some(unix_now()?);
            Ok(change.clone())
        })
    }

    /// Both pending and confirmed-but-unapplied changes can be withdrawn.
    pub fn withdraw(&self, author_id: &str, change_id: u64, revision: u64) -> Result<Change> {
        validate_id(author_id)?;
        self.transact(|state| {
            let index = owned_change(state, author_id, change_id, revision)?;
            let change = &mut state.changes[index].change;
            if change.status == ChangeStatus::Applied {
                return Err(GroupMemoryError::AlreadyApplied);
            }
            change.status = ChangeStatus::Withdrawn;
            Ok(change.clone())
        })
    }

    /// Merge confirmed proposals in one canonical transaction. No pending
    /// proposal is inferred, auto-confirmed, or injected. History is never pruned.
    pub fn maintain(&self, now: i64) -> Result<MaintenanceReport> {
        if now < 0 {
            return Err(GroupMemoryError::InvalidInput);
        }
        self.transact(|state| {
            let indices: Vec<_> = state
                .changes
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.change.status == ChangeStatus::Confirmed)
                .map(|(index, _)| index)
                .collect();
            for &index in &indices {
                let entry = state.changes[index].clone();
                validate_replacement(state, &entry)?;
                let fact_id = if let Some(fact_id) = entry.change.replace_fact_id {
                    fact_id
                } else {
                    if state.facts.len() >= MAX_FACTS {
                        return Err(GroupMemoryError::FactLimit);
                    }
                    let fact_id = next_id(&self.group_hash, "fact", &mut state.next_fact)?;
                    state.facts.push(StoredFact {
                        owner_id: entry.owner_id.clone(),
                        fact_id,
                        current_revision: 0,
                        versions: Vec::new(),
                    });
                    fact_id
                };
                let fact = state
                    .facts
                    .iter_mut()
                    .find(|fact| fact.fact_id == fact_id)
                    .ok_or(GroupMemoryError::CorruptStore)?;
                if fact.versions.len() >= MAX_FACT_REVISIONS {
                    return Err(GroupMemoryError::RevisionLimit);
                }
                let revision = fact.current_revision + 1;
                fact.versions.push(StoredVersion {
                    source_message_id: entry.source_message_id,
                    value: FactRevision {
                        revision,
                        text: entry.change.text,
                        source: entry.change.source,
                        confirmed_at: entry
                            .change
                            .confirmed_at
                            .ok_or(GroupMemoryError::CorruptStore)?,
                        applied_at: now,
                    },
                });
                fact.current_revision = revision;
                state.authority_revision = state
                    .authority_revision
                    .checked_add(1)
                    .ok_or(GroupMemoryError::StoreLimit)?;
                let change = &mut state.changes[index].change;
                change.status = ChangeStatus::Applied;
                change.applied_at = Some(now);
                change.applied_fact_id = Some(fact_id);
            }
            state.last_maintained_at = Some(now);
            Ok(MaintenanceReport {
                applied: indices.len(),
                authority_revision: state.authority_revision,
            })
        })
    }

    pub fn list_pending(&self, author_id: &str) -> Result<Vec<Change>> {
        validate_id(author_id)?;
        self.transact(|state| {
            Ok(active_changes(state)
                .filter(|entry| entry.owner_id == author_id)
                .map(|entry| entry.change.clone())
                .collect())
        })
    }

    /// Return only the subject's applied facts, with retained revision history.
    pub fn confirmed(&self, author_id: &str) -> Result<Vec<Fact>> {
        validate_id(author_id)?;
        self.transact(|state| {
            state
                .facts
                .iter()
                .filter(|fact| fact.owner_id == author_id)
                .map(|fact| self.public_fact(fact))
                .collect()
        })
    }

    /// Suitable for group/admin display: no raw IDs, member text, or sources.
    pub fn summary(&self) -> Result<Summary> {
        self.transact(|state| Ok(summary_from(state)))
    }

    /// The caller must enforce its administrator session before showing this
    /// group-wide view. It contains aliases and self-reported text, never IDs.
    pub fn view(&self) -> Result<MemoryView> {
        self.transact(|state| {
            Ok(MemoryView {
                summary: summary_from(state),
                entries: state
                    .facts
                    .iter()
                    .map(|fact| self.public_fact(fact))
                    .collect::<Result<_>>()?,
                changes: state
                    .changes
                    .iter()
                    .map(|entry| entry.change.clone())
                    .collect(),
                last_maintained_at: state.last_maintained_at,
            })
        })
    }

    /// Current versions only, at most 10 facts per subject and 30 in total.
    /// Budget includes the JSON envelope and escaping, and never exceeds 8 KiB.
    pub fn inject_for(&self, subject_ids: &[&str], budget: usize) -> Result<Injection> {
        if budget > MAX_INJECTION_BYTES {
            return Err(GroupMemoryError::InvalidBudget);
        }
        let subjects: BTreeSet<_> = subject_ids.iter().copied().collect();
        for subject in &subjects {
            validate_id(subject)?;
        }
        self.transact(|state| {
            let serialize = |facts: &[InjectionFact<'_>]| {
                serde_json::to_string(&InjectionEnvelope {
                    kind: "untrusted_group_memory",
                    basis: "confirmed_self_report",
                    authority_revision: state.authority_revision,
                    facts,
                })
                .map_err(|_| GroupMemoryError::CorruptStore)
            };
            let mut selected = Vec::new();
            let mut json = serialize(&selected)?;
            let eligible = state
                .facts
                .iter()
                .filter(|fact| subjects.contains(fact.owner_id.as_str()))
                .count();
            if json.len() > budget {
                return Ok(Injection {
                    json: String::new(),
                    facts: 0,
                    truncated: eligible > 0,
                });
            }
            for subject in subjects {
                let mut per_subject = 0;
                for fact in state.facts.iter().filter(|fact| fact.owner_id == subject) {
                    if per_subject >= MAX_FACTS_PER_SUBJECT || selected.len() >= MAX_INJECTION_FACTS
                    {
                        break;
                    }
                    let version = fact.versions.last().ok_or(GroupMemoryError::CorruptStore)?;
                    selected.push(InjectionFact {
                        fact_id: fact.fact_id,
                        subject: self.alias(subject),
                        text: &version.value.text,
                        source: &version.value.source,
                        confirmed_at: version.value.confirmed_at,
                        revision: fact.current_revision,
                    });
                    let candidate = serialize(&selected)?;
                    if candidate.len() <= budget {
                        json = candidate;
                        per_subject += 1;
                    } else {
                        selected.pop();
                    }
                }
            }
            Ok(Injection {
                json,
                facts: selected.len(),
                truncated: selected.len() < eligible,
            })
        })
    }

    fn alias(&self, author_id: &str) -> String {
        format!(
            "M{}",
            digest(format!("{}\0{author_id}", self.group_hash).as_bytes())
        )
    }

    fn public_fact(&self, fact: &StoredFact) -> Result<Fact> {
        let live = fact.versions.last().ok_or(GroupMemoryError::CorruptStore)?;
        Ok(Fact {
            fact_id: fact.fact_id,
            subject: self.alias(&fact.owner_id),
            text: live.value.text.clone(),
            source: live.value.source.clone(),
            confirmed_at: live.value.confirmed_at,
            revision: fact.current_revision,
            history: fact
                .versions
                .iter()
                .map(|entry| entry.value.clone())
                .collect(),
        })
    }

    fn transact<T>(&self, operation: impl FnOnce(&mut Canonical) -> Result<T>) -> Result<T> {
        let data = open_data_dir(&self.data_dir, self.uid)?;
        let memory = private_dir(&data, OsStr::new("memory"), self.uid)?;
        let groups = private_dir(&memory, OsStr::new("groups"), self.uid)?;
        let directory = private_dir(
            &groups,
            OsStr::new(&format!("G{}", self.group_hash)),
            self.uid,
        )?;
        let documents = private_dir(&directory, OsStr::new("documents"), self.uid)?;
        let lock = open_lock(&directory, self.uid)?;
        FileExt::lock_exclusive(&lock).map_err(io_error)?;
        // The owned lock FD is closed on every return, releasing the lock. All
        // reads, the RMW, and every mirror write remain inside this lock scope.
        let raw = read_private(&directory, "store.json", self.uid)?;
        let mut state = if let Some(raw) = &raw {
            serde_json::from_slice(raw).map_err(|_| GroupMemoryError::CorruptStore)?
        } else {
            for (dir, name) in [
                (&directory, "authority.json"),
                (&documents, "changes.json"),
                (&documents, "CHANGES.md"),
            ] {
                if checked_file(dir, name, self.uid)?.is_some() {
                    return Err(GroupMemoryError::CorruptStore);
                }
            }
            ledger::reject_orphaned_mirrors(self, &directory, &documents)?;
            Canonical {
                schema: 1,
                group_hash: self.group_hash.clone(),
                authority_revision: 0,
                last_maintained_at: None,
                next_change: 1,
                next_fact: 1,
                facts: Vec::new(),
                changes: Vec::new(),
                ledger: ledger::Ledger::default(),
            }
        };
        self.validate_state(&state)?;
        // Repair interrupted or edited derived files before another operation.
        if raw.is_some() {
            self.write_mirrors(&directory, &documents, &state)?;
        }
        let result = operation(&mut state)?;
        self.validate_state(&state)?;
        let canonical = bounded_json(&state)?;
        let mirrors = self.mirrors(&state)?;
        let ledger_mirrors = ledger::prepare_derived(self, &state.ledger)?;
        if raw.as_deref() != Some(canonical.as_slice()) {
            atomic_private_write(&directory, "store.json", &canonical, self.uid)?;
        }
        write_mirrors(&directory, &documents, &mirrors, self.uid)
            .and_then(|()| ledger::write_prepared(self, &directory, &documents, &ledger_mirrors))
            .map_err(|_| GroupMemoryError::MirrorsOutOfDate)?;
        Ok(result)
    }

    fn mirrors(&self, state: &Canonical) -> Result<[Vec<u8>; 3]> {
        let authority = bounded_json(&AuthorityMirror {
            authority_revision: state.authority_revision,
            facts: state
                .facts
                .iter()
                .map(|fact| self.public_fact(fact))
                .collect::<Result<_>>()?,
        })?;
        let changes = bounded_json(&ChangesMirror {
            authority_revision: state.authority_revision,
            changes: state.changes.iter().map(|entry| &entry.change).collect(),
        })?;
        // JSON string escaping keeps member-supplied newlines/backticks inside
        // string literals; the generated document is data, never instructions.
        let mut markdown = b"# Group memory changes\n\nDerived view only. Pending proposals are not long-term memory.\n\n```json\n".to_vec();
        markdown.extend_from_slice(&changes);
        markdown.extend_from_slice(b"\n```\n");
        if markdown.len() > MAX_STORE_BYTES {
            return Err(GroupMemoryError::StoreLimit);
        }
        Ok([authority, changes, markdown])
    }

    fn write_mirrors(&self, dir: &File, documents: &File, state: &Canonical) -> Result<()> {
        write_mirrors(dir, documents, &self.mirrors(state)?, self.uid)?;
        ledger::write_derived(self, dir, documents, &state.ledger)
    }

    fn validate_state(&self, state: &Canonical) -> Result<()> {
        if state.schema != 1
            || state.group_hash != self.group_hash
            || state.next_change == 0
            || state.next_fact == 0
            || state.last_maintained_at.is_some_and(|time| time < 0)
        {
            return Err(GroupMemoryError::CorruptStore);
        }
        if state.facts.len() > MAX_FACTS || active_changes(state).count() > MAX_PENDING_CHANGES {
            return Err(GroupMemoryError::CorruptStore);
        }
        let mut fact_ids = BTreeSet::new();
        let mut revision_count = 0_u64;
        for fact in &state.facts {
            if !fact_ids.insert(fact.fact_id)
                || fact.fact_id == 0
                || validate_id(&fact.owner_id).is_err()
                || fact.versions.is_empty()
                || fact.versions.len() > MAX_FACT_REVISIONS
                || fact.current_revision != fact.versions.len() as u64
            {
                return Err(GroupMemoryError::CorruptStore);
            }
            for (index, version) in fact.versions.iter().enumerate() {
                if version.value.revision != index as u64 + 1
                    || validate_text(&version.value.text).is_err()
                    || !valid_source(&version.value.source, &version.source_message_id)
                    || version.value.confirmed_at < 0
                    || version.value.applied_at < 0
                {
                    return Err(GroupMemoryError::CorruptStore);
                }
            }
            revision_count += fact.current_revision;
        }
        if revision_count != state.authority_revision {
            return Err(GroupMemoryError::CorruptStore);
        }
        let mut change_ids = BTreeSet::new();
        let mut source_ids = BTreeSet::new();
        let mut replacements = BTreeSet::new();
        for entry in &state.changes {
            let change = &entry.change;
            if !change_ids.insert(change.change_id)
                || change.change_id == 0
                || !source_ids.insert((&entry.owner_id, &entry.source_message_id))
                || validate_id(&entry.owner_id).is_err()
                || change.subject != self.alias(&entry.owner_id)
                || validate_text(&change.text).is_err()
                || !valid_source(&change.source, &entry.source_message_id)
                || change.revision != 1
                || change.replace_fact_id.is_some() != entry.replace_revision.is_some()
            {
                return Err(GroupMemoryError::CorruptStore);
            }
            let active = matches!(
                change.status,
                ChangeStatus::Pending | ChangeStatus::Confirmed
            );
            if active
                && let Some(fact_id) = change.replace_fact_id
                && !replacements.insert(fact_id)
            {
                return Err(GroupMemoryError::CorruptStore);
            }
            match change.status {
                ChangeStatus::Pending => {
                    if change.confirmed_at.is_some()
                        || change.applied_at.is_some()
                        || change.applied_fact_id.is_some()
                    {
                        return Err(GroupMemoryError::CorruptStore);
                    }
                }
                ChangeStatus::Confirmed => {
                    if change.confirmed_at.is_none_or(|time| time < 0)
                        || change.applied_at.is_some()
                        || change.applied_fact_id.is_some()
                    {
                        return Err(GroupMemoryError::CorruptStore);
                    }
                }
                ChangeStatus::Applied => {
                    if change.confirmed_at.is_none_or(|time| time < 0)
                        || change.applied_at.is_none_or(|time| time < 0)
                        || !change.applied_fact_id.is_some_and(|id| {
                            state
                                .facts
                                .iter()
                                .any(|fact| fact.fact_id == id && fact.owner_id == entry.owner_id)
                        })
                    {
                        return Err(GroupMemoryError::CorruptStore);
                    }
                }
                ChangeStatus::Withdrawn => {
                    if change.applied_at.is_some() || change.applied_fact_id.is_some() {
                        return Err(GroupMemoryError::CorruptStore);
                    }
                }
            }
        }
        state.ledger.validate(self)
    }
}

fn summary_from(state: &Canonical) -> Summary {
    let subjects: BTreeSet<_> = state
        .facts
        .iter()
        .map(|fact| &fact.owner_id)
        .chain(state.changes.iter().map(|entry| &entry.owner_id))
        .collect();
    let count = |status| {
        state
            .changes
            .iter()
            .filter(|entry| entry.change.status == status)
            .count()
    };
    Summary {
        authority_revision: state.authority_revision,
        subjects: subjects.len(),
        facts: state.facts.len(),
        pending: count(ChangeStatus::Pending),
        confirmed: count(ChangeStatus::Confirmed),
        applied_changes: count(ChangeStatus::Applied),
        withdrawn: count(ChangeStatus::Withdrawn),
    }
}

fn active_changes(state: &Canonical) -> impl Iterator<Item = &StoredChange> {
    state.changes.iter().filter(|entry| {
        matches!(
            entry.change.status,
            ChangeStatus::Pending | ChangeStatus::Confirmed
        )
    })
}

fn owned_change(
    state: &Canonical,
    author_id: &str,
    change_id: u64,
    revision: u64,
) -> Result<usize> {
    let index = state
        .changes
        .iter()
        .position(|entry| entry.change.change_id == change_id)
        .ok_or(GroupMemoryError::NotFound)?;
    let entry = &state.changes[index];
    if entry.owner_id != author_id {
        return Err(GroupMemoryError::NotOwner);
    }
    if entry.change.revision != revision {
        return Err(GroupMemoryError::StaleRevision);
    }
    Ok(index)
}

fn validate_replacement(state: &Canonical, entry: &StoredChange) -> Result<()> {
    if let Some(fact_id) = entry.change.replace_fact_id {
        let fact = state
            .facts
            .iter()
            .find(|fact| fact.fact_id == fact_id)
            .ok_or(GroupMemoryError::NotFound)?;
        if fact.owner_id != entry.owner_id {
            return Err(GroupMemoryError::NotOwner);
        }
        if Some(fact.current_revision) != entry.replace_revision {
            return Err(GroupMemoryError::StaleRevision);
        }
        if fact.versions.len() >= MAX_FACT_REVISIONS {
            return Err(GroupMemoryError::RevisionLimit);
        }
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(GroupMemoryError::InvalidInput);
    }
    Ok(())
}

fn validate_text(text: &str) -> Result<()> {
    if text.trim().is_empty()
        || text.contains('\0')
        || text.len() > MAX_TEXT_BYTES
        || text.chars().count() > MAX_TEXT_CHARS
    {
        return Err(GroupMemoryError::InvalidInput);
    }
    Ok(())
}

fn source(message_id: &str, timestamp: i64) -> Source {
    Source {
        message_hash: digest(message_id.as_bytes()),
        timestamp,
    }
}

fn valid_source(source: &Source, raw_id: &str) -> bool {
    validate_id(raw_id).is_ok()
        && source.timestamp >= 0
        && source.message_hash == digest(raw_id.as_bytes())
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

fn next_id(group_hash: &str, kind: &str, counter: &mut u64) -> Result<u64> {
    let current = *counter;
    *counter = counter.checked_add(1).ok_or(GroupMemoryError::StoreLimit)?;
    let hash = Sha1::digest(format!("{group_hash}\0{kind}\0{current}").as_bytes());
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&hash[..8]);
    // Exact JSON integer transport even through JavaScript clients.
    let id = u64::from_be_bytes(bytes) >> 11;
    if id == 0 {
        return Err(GroupMemoryError::StoreLimit);
    }
    Ok(id)
}

fn unix_now() -> Result<i64> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| GroupMemoryError::Io)?
        .as_secs();
    i64::try_from(seconds).map_err(|_| GroupMemoryError::Io)
}

fn bounded_json(value: &impl Serialize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|_| GroupMemoryError::CorruptStore)?;
    if bytes.len() > MAX_STORE_BYTES {
        return Err(GroupMemoryError::StoreLimit);
    }
    Ok(bytes)
}

fn io_error(error: std::io::Error) -> GroupMemoryError {
    match error.raw_os_error() {
        Some(20 | 40) => GroupMemoryError::UnsafePath,
        _ => GroupMemoryError::Io,
    }
}

fn fd_path(directory: &File, name: impl AsRef<OsStr>) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name.as_ref())
}

fn open_directory(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_DIRECTORY)
        .open(path)
        .map_err(io_error)
}

fn open_data_dir(path: &Path, uid: u32) -> Result<File> {
    let mut directory = open_directory(Path::new("/"))?;
    let mut found = false;
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                let next = fd_path(&directory, name);
                match DirBuilder::new().mode(0o700).create(&next) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(io_error(error)),
                }
                directory = open_directory(&next)?;
                found = true;
            }
            _ => return Err(GroupMemoryError::UnsafePath),
        }
    }
    if !found || directory.metadata().map_err(io_error)?.uid() != uid {
        return Err(GroupMemoryError::UnsafePath);
    }
    Ok(directory)
}

fn private_dir(parent: &File, name: &OsStr, uid: u32) -> Result<File> {
    let path = fd_path(parent, name);
    match DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io_error(error)),
    }
    let directory = open_directory(&path)?;
    let metadata = directory.metadata().map_err(io_error)?;
    if metadata.uid() != uid || metadata.mode() & 0o7777 != 0o700 {
        return Err(GroupMemoryError::UnsafePath);
    }
    Ok(directory)
}

fn validate_file(file: &File, uid: u32) -> Result<()> {
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(GroupMemoryError::UnsafePath);
    }
    if metadata.len() > MAX_STORE_BYTES as u64 {
        return Err(GroupMemoryError::StoreLimit);
    }
    Ok(())
}

fn checked_file(directory: &File, name: &str, uid: u32) -> Result<Option<File>> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(fd_path(directory, name))
    {
        Ok(file) => {
            validate_file(&file, uid)?;
            Ok(Some(file))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}

fn open_lock(directory: &File, uid: u32) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(fd_path(directory, "store.lock"))
        .map_err(io_error)?;
    validate_file(&file, uid)?;
    Ok(file)
}

fn read_private(directory: &File, name: &str, uid: u32) -> Result<Option<Vec<u8>>> {
    let Some(file) = checked_file(directory, name, uid)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(MAX_STORE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > MAX_STORE_BYTES {
        return Err(GroupMemoryError::StoreLimit);
    }
    Ok(Some(bytes))
}

fn atomic_private_write(directory: &File, name: &str, bytes: &[u8], uid: u32) -> Result<()> {
    if bytes.len() > MAX_STORE_BYTES {
        return Err(GroupMemoryError::StoreLimit);
    }
    if read_private(directory, name, uid)?.as_deref() == Some(bytes) {
        return Ok(());
    }
    let temporary = format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    );
    let path = fd_path(directory, &temporary);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(&path)
        .map_err(io_error)?;
    let result = (|| {
        validate_file(&file, uid)?;
        file.write_all(bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        checked_file(directory, name, uid)?;
        fs::rename(&path, fd_path(directory, name)).map_err(io_error)?;
        directory.sync_all().map_err(io_error)
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

fn write_mirrors(
    directory: &File,
    documents: &File,
    mirrors: &[Vec<u8>; 3],
    uid: u32,
) -> Result<()> {
    atomic_private_write(directory, "authority.json", &mirrors[0], uid)?;
    atomic_private_write(documents, "changes.json", &mirrors[1], uid)?;
    atomic_private_write(documents, "CHANGES.md", &mirrors[2], uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

    fn setup() -> (TempDir, GroupMemoryStore) {
        let temporary = tempfile::tempdir().unwrap();
        let store = GroupMemoryStore::new(temporary.path(), "group-one").unwrap();
        (temporary, store)
    }

    fn directory(store: &GroupMemoryStore) -> PathBuf {
        store
            .data_dir
            .join("memory/groups")
            .join(format!("G{}", store.group_hash))
    }

    fn apply(store: &GroupMemoryStore, text: &str, message: &str, replace: Option<u64>) -> Fact {
        let change = store
            .propose("member-one", text, replace, message, 100)
            .unwrap();
        store
            .confirm("member-one", change.change_id, change.revision)
            .unwrap();
        store.maintain(200).unwrap();
        store
            .confirmed("member-one")
            .unwrap()
            .into_iter()
            .find(|fact| Some(fact.fact_id) == replace || fact.text == text)
            .unwrap()
    }

    fn seed_fact(store: &GroupMemoryStore, state: &mut Canonical, owner: &str, text: &str) {
        let fact_id = next_id(&store.group_hash, "fact", &mut state.next_fact).unwrap();
        let message = format!("seed-{fact_id}");
        state.facts.push(StoredFact {
            owner_id: owner.to_owned(),
            fact_id,
            current_revision: 1,
            versions: vec![StoredVersion {
                source_message_id: message.clone(),
                value: FactRevision {
                    revision: 1,
                    text: text.to_owned(),
                    source: source(&message, 100),
                    confirmed_at: 101,
                    applied_at: 102,
                },
            }],
        });
        state.authority_revision += 1;
    }

    fn seed_pending(store: &GroupMemoryStore, state: &mut Canonical, index: usize, text: &str) {
        let message = format!("proposal-{index}");
        state.changes.push(StoredChange {
            owner_id: "member-one".to_owned(),
            source_message_id: message.clone(),
            replace_revision: None,
            change: Change {
                change_id: next_id(&store.group_hash, "change", &mut state.next_change).unwrap(),
                subject: store.alias("member-one"),
                text: text.to_owned(),
                replace_fact_id: None,
                source: source(&message, 100),
                revision: 1,
                status: ChangeStatus::Pending,
                confirmed_at: None,
                applied_at: None,
                applied_fact_id: None,
            },
        });
    }

    #[test]
    fn exact_owner_and_revision_are_required_and_confirmation_is_idempotent() {
        let (_temporary, store) = setup();
        let change = store
            .propose("member-one", "喜欢茶", None, "message-one", 100)
            .unwrap();
        for author in ["member-two", "Member-one"] {
            assert_eq!(
                store.confirm(author, change.change_id, 1),
                Err(GroupMemoryError::NotOwner)
            );
            assert_eq!(
                store.withdraw(author, change.change_id, 1),
                Err(GroupMemoryError::NotOwner)
            );
        }
        assert_eq!(
            store.confirm("member-one", change.change_id, 2),
            Err(GroupMemoryError::StaleRevision)
        );
        let confirmed = store.confirm("member-one", change.change_id, 1).unwrap();
        assert_eq!(confirmed.status, ChangeStatus::Confirmed);
        assert_eq!(
            store.confirm("member-one", change.change_id, 1).unwrap(),
            confirmed
        );
        assert_eq!(
            store
                .propose("member-one", "喜欢茶", None, "message-one", 100)
                .unwrap(),
            confirmed
        );
        assert_eq!(
            store.propose("member-one", "改写", None, "message-one", 100),
            Err(GroupMemoryError::InvalidInput)
        );
    }

    #[test]
    fn candidates_never_inject_and_repeated_maintenance_adds_no_history() {
        let (_temporary, store) = setup();
        let change = store
            .propose(
                "private-member-id",
                "喜欢绿茶",
                None,
                "private-message-id",
                100,
            )
            .unwrap();
        assert_eq!(store.maintain(200).unwrap().applied, 0);
        assert_eq!(
            store
                .inject_for(&["private-member-id"], 8192)
                .unwrap()
                .facts,
            0
        );
        store
            .confirm("private-member-id", change.change_id, 1)
            .unwrap();
        assert!(store.confirmed("private-member-id").unwrap().is_empty());
        assert_eq!(
            store
                .inject_for(&["private-member-id"], 8192)
                .unwrap()
                .facts,
            0
        );
        assert_eq!(
            store.maintain(200).unwrap(),
            MaintenanceReport {
                applied: 1,
                authority_revision: 1
            }
        );
        assert_eq!(
            store.maintain(201).unwrap(),
            MaintenanceReport {
                applied: 0,
                authority_revision: 1
            }
        );
        let facts = store.confirmed("private-member-id").unwrap();
        assert_eq!(facts[0].history.len(), 1);
        let injection = store.inject_for(&["private-member-id"], 8192).unwrap();
        assert_eq!(injection.facts, 1);
        let value: serde_json::Value = serde_json::from_str(&injection.json).unwrap();
        assert_eq!(value["kind"], "untrusted_group_memory");
        assert_eq!(value["facts"][0]["text"], "喜欢绿茶");
        for public in [
            injection.json,
            serde_json::to_string(&facts).unwrap(),
            fs::read_to_string(directory(&store).join("authority.json")).unwrap(),
        ] {
            assert!(!public.contains("private-member-id"));
            assert!(!public.contains("private-message-id"));
        }
        assert!(store.confirmed("another-member").unwrap().is_empty());
        assert_eq!(
            store.withdraw("private-member-id", change.change_id, 1),
            Err(GroupMemoryError::AlreadyApplied)
        );
        assert_eq!(
            store
                .confirm("private-member-id", change.change_id, 1)
                .unwrap()
                .status,
            ChangeStatus::Applied
        );
    }

    #[test]
    fn replacements_require_ownership_and_keep_complete_history() {
        let (_temporary, store) = setup();
        let original = apply(&store, "喜欢红茶", "first", None);
        assert_eq!(
            store.propose("member-two", "替换", Some(original.fact_id), "second", 101),
            Err(GroupMemoryError::NotOwner)
        );
        let replacement = store
            .propose(
                "member-one",
                "喜欢绿茶",
                Some(original.fact_id),
                "third",
                102,
            )
            .unwrap();
        assert_eq!(
            store.propose("member-one", "冲突", Some(original.fact_id), "fourth", 103),
            Err(GroupMemoryError::ConflictingReplacement)
        );
        store
            .confirm("member-one", replacement.change_id, 1)
            .unwrap();
        store.maintain(300).unwrap();
        let facts = store.confirmed("member-one").unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].fact_id, original.fact_id);
        assert_eq!(facts[0].revision, 2);
        assert_eq!(facts[0].text, "喜欢绿茶");
        assert_eq!(
            facts[0]
                .history
                .iter()
                .map(|version| version.text.as_str())
                .collect::<Vec<_>>(),
            ["喜欢红茶", "喜欢绿茶"]
        );
        assert_eq!(store.summary().unwrap().authority_revision, 2);
        let injection = store.inject_for(&["member-one"], 8192).unwrap();
        assert!(injection.json.contains("喜欢绿茶"));
        assert!(!injection.json.contains("喜欢红茶"));
    }

    #[test]
    fn confirmed_unapplied_changes_can_be_withdrawn_and_never_merge() {
        let (_temporary, store) = setup();
        let change = store
            .propose("member-one", "草稿", None, "first", 100)
            .unwrap();
        store.confirm("member-one", change.change_id, 1).unwrap();
        let withdrawn = store.withdraw("member-one", change.change_id, 1).unwrap();
        assert_eq!(withdrawn.status, ChangeStatus::Withdrawn);
        assert_eq!(
            store.withdraw("member-one", change.change_id, 1).unwrap(),
            withdrawn
        );
        assert_eq!(
            store.confirm("member-one", change.change_id, 1),
            Err(GroupMemoryError::NotPending)
        );
        assert!(store.list_pending("member-one").unwrap().is_empty());
        assert_eq!(store.maintain(200).unwrap().applied, 0);
    }

    #[test]
    fn group_bound_aliases_ids_truth_and_canonical_validation_prevent_cross_group_use() {
        let (temporary, first) = setup();
        let second = GroupMemoryStore::new(temporary.path(), "group-two").unwrap();
        let change = first
            .propose("same-member", "仅第一群", None, "first", 100)
            .unwrap();
        assert_eq!(
            second.confirm("same-member", change.change_id, 1),
            Err(GroupMemoryError::NotFound)
        );
        first.confirm("same-member", change.change_id, 1).unwrap();
        first.maintain(200).unwrap();
        let fact = first.confirmed("same-member").unwrap().remove(0);
        assert_eq!(
            second.propose("same-member", "替换别群", Some(fact.fact_id), "second", 100),
            Err(GroupMemoryError::NotFound)
        );
        assert_ne!(first.alias("same-member"), second.alias("same-member"));
        assert_eq!(second.inject_for(&["same-member"], 8192).unwrap().facts, 0);
        let copied = fs::read(directory(&first).join("store.json")).unwrap();
        fs::write(directory(&second).join("store.json"), copied).unwrap();
        assert_eq!(second.summary(), Err(GroupMemoryError::CorruptStore));
    }

    #[test]
    fn canonical_truth_rebuilds_missing_stale_and_manually_edited_mirrors() {
        let (temporary, store) = setup();
        apply(&store, "正式本地事实", "first", None);
        let path = directory(&store);
        let canonical = fs::read(path.join("store.json")).unwrap();
        fs::write(path.join("authority.json"), b"[]").unwrap();
        fs::remove_file(path.join("documents/changes.json")).unwrap();
        fs::write(
            path.join("documents/CHANGES.md"),
            b"unconfirmed manual edits",
        )
        .unwrap();
        let reopened = GroupMemoryStore::new(temporary.path(), "group-one").unwrap();
        assert_eq!(
            reopened.confirmed("member-one").unwrap()[0].text,
            "正式本地事实"
        );
        assert_eq!(fs::read(path.join("store.json")).unwrap(), canonical);
        let authority: serde_json::Value =
            serde_json::from_slice(&fs::read(path.join("authority.json")).unwrap()).unwrap();
        let changes: serde_json::Value =
            serde_json::from_slice(&fs::read(path.join("documents/changes.json")).unwrap())
                .unwrap();
        assert_eq!(authority["facts"][0]["text"], "正式本地事实");
        assert_eq!(changes["changes"][0]["status"], "applied");
        let document = fs::read_to_string(path.join("documents/CHANGES.md")).unwrap();
        assert!(document.contains("正式本地事实"));
        assert!(!document.contains("manual edits"));
        assert!(!path.join("documents/profiles").exists());
        fs::remove_file(path.join("store.json")).unwrap();
        assert_eq!(reopened.summary(), Err(GroupMemoryError::CorruptStore));
    }

    #[test]
    fn files_and_directories_are_private_and_permission_weakening_is_rejected() {
        let (_temporary, store) = setup();
        let path = directory(&store);
        for relative in [
            "store.json",
            "store.lock",
            "authority.json",
            "documents/changes.json",
            "documents/CHANGES.md",
        ] {
            assert_eq!(
                fs::metadata(path.join(relative)).unwrap().mode() & 0o7777,
                0o600
            );
        }
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o700);
        assert_eq!(
            fs::metadata(path.join("documents")).unwrap().mode() & 0o7777,
            0o700
        );
        fs::set_permissions(path.join("store.json"), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(store.summary(), Err(GroupMemoryError::UnsafePath));
        fs::set_permissions(path.join("store.json"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(path.join("documents"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(store.summary(), Err(GroupMemoryError::UnsafePath));
    }

    #[test]
    fn symlinks_and_hardlinks_never_become_read_or_write_targets() {
        let (temporary, store) = setup();
        let path = directory(&store);
        let victim = temporary.path().join("victim");
        fs::write(&victim, b"untouched").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(path.join("authority.json")).unwrap();
        symlink(&victim, path.join("authority.json")).unwrap();
        assert_eq!(store.summary(), Err(GroupMemoryError::UnsafePath));
        assert_eq!(fs::read(&victim).unwrap(), b"untouched");
        fs::remove_file(path.join("authority.json")).unwrap();
        fs::hard_link(&victim, path.join("authority.json")).unwrap();
        assert_eq!(store.summary(), Err(GroupMemoryError::UnsafePath));
        let linked_data = temporary.path().join("linked-data");
        symlink(temporary.path(), &linked_data).unwrap();
        assert!(matches!(
            GroupMemoryStore::new(&linked_data, "third-group"),
            Err(GroupMemoryError::UnsafePath)
        ));
        fs::remove_file(path.join("authority.json")).unwrap();
        let group_symlink = temporary
            .path()
            .join("memory/groups")
            .join(format!("G{}", digest(b"linked-group")));
        symlink(temporary.path(), group_symlink).unwrap();
        assert!(matches!(
            GroupMemoryStore::new(temporary.path(), "linked-group"),
            Err(GroupMemoryError::UnsafePath)
        ));
    }

    #[test]
    fn pending_and_fact_capacity_reject_without_pruning() {
        let (_temporary, store) = setup();
        store
            .transact(|state| {
                for index in 0..MAX_PENDING_CHANGES {
                    seed_pending(&store, state, index, "候选");
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store.propose("member-one", "额外", None, "extra", 100),
            Err(GroupMemoryError::PendingLimit)
        );
        let first = store.list_pending("member-one").unwrap().remove(0);
        store
            .withdraw("member-one", first.change_id, first.revision)
            .unwrap();
        assert!(
            store
                .propose("member-one", "新候选", None, "extra", 100)
                .is_ok()
        );
        let (_other_temporary, full) = setup();
        full.transact(|state| {
            for _ in 0..MAX_FACTS {
                seed_fact(&full, state, "member-one", "已确认");
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            full.propose("member-one", "额外事实", None, "extra", 100),
            Err(GroupMemoryError::FactLimit)
        );
        assert_eq!(full.confirmed("member-one").unwrap().len(), MAX_FACTS);
    }

    #[test]
    fn revision_limit_retains_all_sixteen_and_rejects_the_next() {
        let (_temporary, store) = setup();
        let first = apply(&store, "version-1", "message-1", None);
        for revision in 2..=MAX_FACT_REVISIONS {
            apply(
                &store,
                &format!("version-{revision}"),
                &format!("message-{revision}"),
                Some(first.fact_id),
            );
        }
        assert_eq!(
            store.propose(
                "member-one",
                "第十七版",
                Some(first.fact_id),
                "message-17",
                100
            ),
            Err(GroupMemoryError::RevisionLimit)
        );
        let fact = store.confirmed("member-one").unwrap().remove(0);
        assert_eq!(fact.history.len(), MAX_FACT_REVISIONS);
        assert_eq!(fact.history[0].text, "version-1");
        assert_eq!(fact.text, "version-16");
        assert_eq!(store.maintain(400).unwrap().applied, 0);
    }

    #[test]
    fn byte_limit_rejects_the_whole_transaction_and_oversized_reads() {
        let (_temporary, store) = setup();
        let path = directory(&store).join("store.json");
        let before = fs::read(&path).unwrap();
        assert_eq!(
            store.transact(|state| {
                for index in 0..MAX_PENDING_CHANGES {
                    seed_pending(&store, state, index, &"\u{1}".repeat(2000));
                }
                Ok(())
            }),
            Err(GroupMemoryError::StoreLimit)
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(store.summary().unwrap().pending, 0);
        OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_len(MAX_STORE_BYTES as u64 + 1)
            .unwrap();
        assert_eq!(store.summary(), Err(GroupMemoryError::StoreLimit));
    }

    #[test]
    fn injection_is_valid_bounded_json_with_subject_and_fact_limits() {
        let (_temporary, store) = setup();
        store
            .transact(|state| {
                for owner in ["member-one", "member-two", "member-three", "member-four"] {
                    for _ in 0..12 {
                        seed_fact(&store, state, owner, "喜欢🦀\n\"不可信数据\"");
                    }
                }
                seed_fact(&store, state, "excluded", "不应注入");
                seed_pending(&store, state, 1, "候选不能注入");
                Ok(())
            })
            .unwrap();
        let subjects = [
            "member-one",
            "member-two",
            "member-three",
            "member-four",
            "member-one",
        ];
        for budget in [0, 100, 250, 1024, 8192] {
            let injection = store.inject_for(&subjects, budget).unwrap();
            assert!(injection.json.len() <= budget);
            assert!(injection.facts <= MAX_INJECTION_FACTS);
            assert!(injection.truncated);
            if !injection.json.is_empty() {
                let value: serde_json::Value = serde_json::from_str(&injection.json).unwrap();
                let facts = value["facts"].as_array().unwrap();
                assert_eq!(facts.len(), injection.facts);
                for owner in subjects {
                    assert!(
                        facts
                            .iter()
                            .filter(|fact| fact["subject"] == store.alias(owner))
                            .count()
                            <= MAX_FACTS_PER_SUBJECT
                    );
                }
                assert!(!injection.json.contains("不应注入"));
                assert!(!injection.json.contains("候选不能注入"));
            }
        }
        assert_eq!(
            store.inject_for(&subjects, 8193),
            Err(GroupMemoryError::InvalidBudget)
        );
    }

    #[test]
    fn input_validation_and_file_locking_preserve_all_concurrent_proposals() {
        let (temporary, store) = setup();
        for text in ["", " \n", "a\0b", &"x".repeat(2001)] {
            assert_eq!(
                store.propose("member-one", text, None, "invalid", 100),
                Err(GroupMemoryError::InvalidInput)
            );
        }
        assert!(
            store
                .propose("member-one", &"🦀".repeat(2000), None, "unicode", 100)
                .is_ok()
        );
        let mut threads = Vec::new();
        for index in 0..8 {
            let path = temporary.path().to_owned();
            threads.push(std::thread::spawn(move || {
                GroupMemoryStore::new(path, "group-one")
                    .unwrap()
                    .propose(
                        "member-one",
                        "并发候选",
                        None,
                        &format!("concurrent-{index}"),
                        100,
                    )
                    .unwrap()
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(store.summary().unwrap().pending, 9);
        let ids: BTreeSet<_> = store
            .list_pending("member-one")
            .unwrap()
            .iter()
            .map(|change| change.change_id)
            .collect();
        assert_eq!(ids.len(), 9);
        assert!(ids.into_iter().all(|id| id < (1_u64 << 53)));
    }

    #[test]
    fn administrator_view_has_aliases_only_and_records_maintenance_time() {
        let (_temporary, store) = setup();
        let change = store
            .propose("raw-member-one", "候选文本", None, "raw-message-one", 100)
            .unwrap();
        assert_eq!(store.view().unwrap().last_maintained_at, None);
        store
            .confirm("raw-member-one", change.change_id, 1)
            .unwrap();
        store.maintain(1234).unwrap();
        let view = store.view().unwrap();
        assert_eq!(view.summary.facts, 1);
        assert_eq!(view.entries.len(), 1);
        assert_eq!(
            store.subject_alias("raw-member-one").unwrap(),
            view.entries[0].subject
        );
        assert_eq!(
            store.subject_alias(" "),
            Err(GroupMemoryError::InvalidInput)
        );
        assert_eq!(view.changes[0].status, ChangeStatus::Applied);
        assert_eq!(view.last_maintained_at, Some(1234));
        let serialized = serde_json::to_string(&view).unwrap();
        assert!(!serialized.contains("raw-member-one"));
        assert!(!serialized.contains("raw-message-one"));
        store.maintain(1235).unwrap();
        assert_eq!(store.view().unwrap().last_maintained_at, Some(1235));
        assert_eq!(store.view().unwrap().entries[0].history.len(), 1);
    }
}
