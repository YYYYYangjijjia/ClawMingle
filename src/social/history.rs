//! Private, conversation-scoped history of messages observed by this installation.
//! The JSON state is persisted first; archiving failure must prevent gateway ACK.

use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::model::social::{
    Bubble, DeliveryStatus, GroupState, MessageRecord, Quote, RequestRecord,
};

const MAX_DB_BYTES: u64 = 128 * 1024 * 1024;
// Checkpoints can be pinned by a reader. Never let repeated writes grow WAL
// indefinitely; a transaction may temporarily exceed this physical preflight.
const MAX_PHYSICAL_BYTES: u64 = 192 * 1024 * 1024;
const APPLICATION_ID: i64 = 0x434d4849;
const SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const NOFOLLOW: i32 = 0o400000;

pub struct History {
    path: PathBuf,
    scope: String,
}

pub struct HistoryReader {
    connection: Connection,
    scope: String,
}

pub(super) struct ParticipantObservation {
    pub author_id: String,
    pub name: Option<String>,
    pub timestamp_ms: i64,
    pub seq: u64,
}

#[derive(Clone)]
pub enum Item {
    Incoming(Box<MessageRecord>),
    Bot {
        request_id: u64,
        bubble_index: usize,
        bubble: Box<Bubble>,
    },
}

pub struct Page {
    pub items: Vec<(u64, Item)>,
    pub next_cursor: Option<u64>,
    pub has_more: bool,
}

/// Snapshot helpers are operator-only; model tools still use a scoped History.
pub(crate) fn snapshot_connection(path: &Path) -> Result<Connection> {
    let connection = open(path, true)?;
    check_schema(&connection)?;
    Ok(connection)
}

pub(crate) fn snapshot_reader(path: &Path, scope: &str) -> Result<HistoryReader> {
    History {
        path: path.to_owned(),
        scope: scope.to_owned(),
    }
    .reader()
}

pub(crate) fn snapshot_archive(source: &Path, target: &Path) -> Result<()> {
    let source = snapshot_connection(source)?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(NOFOLLOW)
        .open(target)
        .map_err(|_| anyhow::anyhow!("backup_snapshot_create_failed"))?;
    let mut target = Connection::open_with_flags(
        target,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(database_error)?;
    target.busy_timeout(BUSY_TIMEOUT).map_err(database_error)?;
    {
        let backup = rusqlite::backup::Backup::new(&source, &mut target).map_err(database_error)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            use rusqlite::backup::StepResult;
            match backup.step(128).map_err(database_error)? {
                StepResult::Done => break,
                StepResult::More | StepResult::Busy | StepResult::Locked => {
                    ensure!(
                        std::time::Instant::now() < deadline,
                        "backup_snapshot_timeout"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                _ => bail!("backup_snapshot_failed"),
            }
        }
    }
    // A completed backup must be self-contained, without WAL/SHM sidecars.
    let mode: String = target
        .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
        .map_err(database_error)?;
    ensure!(mode == "delete", "backup_snapshot_journal_failed");
    let result: String = target
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(database_error)?;
    ensure!(result == "ok", "backup_snapshot_invalid");
    Ok(())
}

fn database_error(error: rusqlite::Error) -> anyhow::Error {
    match error {
        rusqlite::Error::SqliteFailure(code, _)
            if code.extended_code & 0xff == rusqlite::ffi::SQLITE_FULL =>
        {
            anyhow::anyhow!("history_archive_full")
        }
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.extended_code & 0xff,
                rusqlite::ffi::SQLITE_BUSY | rusqlite::ffi::SQLITE_LOCKED
            ) =>
        {
            anyhow::anyhow!("history_archive_busy")
        }
        _ => anyhow::anyhow!("history_archive_database_failed"),
    }
}

fn number(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| anyhow::anyhow!("history_integer_out_of_range"))
}

fn timestamp() -> Result<i64> {
    let value = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("history_clock_invalid"))?
        .as_millis();
    i64::try_from(value).map_err(|_| anyhow::anyhow!("history_clock_invalid"))
}

fn uid() -> Result<u32> {
    fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .map_err(|_| anyhow::anyhow!("history_owner_unavailable"))
}

fn no_symlinks(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "history_absolute_path_required");
    let mut current = PathBuf::new();
    for component in path.components() {
        ensure!(
            !matches!(component, Component::CurDir | Component::ParentDir),
            "history_normalized_path_required"
        );
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|_| anyhow::anyhow!("history_path_unavailable"))?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "history_symlink_rejected"
        );
    }
    Ok(())
}

fn private_directory(path: &Path, create: bool) -> Result<()> {
    if create {
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => bail!("history_directory_create_failed"),
        }
    }
    no_symlinks(path)?;
    let metadata =
        fs::symlink_metadata(path).map_err(|_| anyhow::anyhow!("history_directory_unavailable"))?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == uid()?
            && metadata.permissions().mode() & 0o7777 == 0o700,
        "history_private_directory_required"
    );
    Ok(())
}

fn private_database(path: &Path) -> Result<()> {
    no_symlinks(path)?;
    let metadata =
        fs::symlink_metadata(path).map_err(|_| anyhow::anyhow!("history_database_unavailable"))?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == uid()?
            && metadata.permissions().mode() & 0o7777 == 0o600,
        "history_private_database_required"
    );
    ensure!(metadata.len() <= MAX_DB_BYTES, "history_archive_full");
    Ok(())
}

fn physical_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        match fs::symlink_metadata(name) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file()
                        && !metadata.file_type().is_symlink()
                        && metadata.nlink() == 1
                        && metadata.uid() == uid()?
                        && metadata.permissions().mode() & 0o077 == 0,
                    "history_sidecar_invalid"
                );
                total = total
                    .checked_add(metadata.len())
                    .ok_or_else(|| anyhow::anyhow!("history_archive_full"))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => bail!("history_database_unavailable"),
        }
    }
    Ok(total)
}

fn open(path: &Path, readonly: bool) -> Result<Connection> {
    private_directory(
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("history_path_invalid"))?,
        false,
    )?;
    private_database(path)?;
    physical_size(path)?;
    let access = if readonly {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let connection = Connection::open_with_flags(
        path,
        access | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(database_error)?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(database_error)?;
    connection
        .pragma_update(None, "trusted_schema", false)
        .map_err(database_error)?;
    if readonly {
        connection
            .pragma_update(None, "query_only", true)
            .map_err(database_error)?;
    }
    Ok(connection)
}

fn check_schema(connection: &Connection) -> Result<()> {
    let application: i64 = connection
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(database_error)?;
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(database_error)?;
    ensure!(
        application == APPLICATION_ID && version == SCHEMA_VERSION,
        "history_schema_mismatch"
    );
    Ok(())
}

fn writer_settings(connection: &Connection) -> Result<()> {
    let page_size: i64 = connection
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .map_err(database_error)?;
    ensure!(page_size > 0, "history_database_invalid");
    let pages: i64 = connection
        .pragma_query_value(None, "max_page_count", |row| row.get(0))
        .map_err(database_error)?;
    if pages != MAX_DB_BYTES as i64 / page_size {
        connection
            .pragma_update(None, "max_page_count", MAX_DB_BYTES as i64 / page_size)
            .map_err(database_error)?;
    }
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(database_error)?;
    connection
        .pragma_update(None, "cache_size", -256)
        .map_err(database_error)?;
    connection
        .pragma_update(None, "wal_autocheckpoint", 256)
        .map_err(database_error)?;
    connection
        .pragma_update(None, "journal_size_limit", 4 * 1024 * 1024)
        .map_err(database_error)?;
    Ok(())
}

impl History {
    pub(super) fn conversation_id(&self) -> &str {
        &self.scope
    }

    pub fn new(data_dir: &Path, conversation_id: &str) -> Result<Self> {
        ensure!(
            !conversation_id.trim().is_empty()
                && conversation_id.len() <= 512
                && !conversation_id.chars().any(char::is_control),
            "history_conversation_invalid"
        );
        private_directory(data_dir, false)?;
        let directory = data_dir.join("history");
        private_directory(&directory, true)?;
        let path = directory.join("archive.sqlite");
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(NOFOLLOW)
            .open(&path)
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => bail!("history_database_create_failed"),
        }
        let mut connection = open(&path, false)?;
        writer_settings(&connection)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let application: i64 = transaction
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .map_err(database_error)?;
        if application == 0 {
            let tables: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                    [],
                    |row| row.get(0),
                )
                .map_err(database_error)?;
            ensure!(tables == 0, "history_schema_mismatch");
            transaction.execute_batch(SCHEMA).map_err(database_error)?;
            transaction
                .pragma_update(None, "application_id", APPLICATION_ID)
                .map_err(database_error)?;
            transaction
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(database_error)?;
        } else {
            check_schema(&transaction)?;
        }
        transaction.commit().map_err(database_error)?;
        let mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(database_error)?;
        if mode != "wal" {
            let mode: String = connection
                .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
                .map_err(database_error)?;
            ensure!(mode == "wal", "history_wal_unavailable");
        }
        Ok(Self {
            path,
            scope: conversation_id.to_owned(),
        })
    }

    /// Caller must durably save the canonical JSON before calling this method.
    /// No missing/pruned in-memory record is deleted from the archive.
    pub fn save(&self, state: &GroupState) -> Result<()> {
        let mut connection = open(&self.path, false)?;
        check_schema(&connection)?;
        writer_settings(&connection)?;
        // Passive never waits for an old reader to release its snapshot.
        connection
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))
            .map_err(database_error)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        ensure!(
            physical_size(&self.path)? < MAX_PHYSICAL_BYTES,
            "history_archive_full"
        );
        let next = number(state.next_seq)?;
        ensure!(next >= 1, "history_state_invalid");
        let now = timestamp()?;
        let first = state.messages.iter().map(|record| record.seq).min();
        transaction
            .execute(
                "INSERT INTO conversations(scope, started_at_ms, import_first_seq, import_next_seq, last_next_seq) VALUES(?1,?2,?3,?4,?4) ON CONFLICT(scope) DO NOTHING",
                params![&self.scope, now, first.map(number).transpose()?, next],
            )
            .map_err(database_error)?;
        let archived_next: i64 = transaction
            .query_row(
                "SELECT last_next_seq FROM conversations WHERE scope=?1",
                [&self.scope],
                |row| row.get(0),
            )
            .map_err(database_error)?;
        ensure!(next >= archived_next, "history_state_rewind_detected");
        let mut pending = Vec::new();
        for record in &state.messages {
            let seq = number(record.seq)?;
            ensure!(seq > 0 && seq < next, "history_state_invalid");
            ensure!(!record.message.id.is_empty(), "history_state_invalid");
            let old: Option<String> = transaction
                .query_row(
                    "SELECT message_id FROM incoming WHERE scope=?1 AND seq=?2",
                    params![&self.scope, seq],
                    |row| row.get(0),
                )
                .optional()
                .map_err(database_error)?;
            ensure!(
                old.as_deref().is_none_or(|id| id == record.message.id),
                "history_message_identity_changed"
            );
            let raw = serde_json::to_string(record)
                .map_err(|_| anyhow::anyhow!("history_record_invalid"))?;
            transaction.execute(
                "INSERT INTO incoming(scope,seq,message_id,reference_idx,timestamp_ms,search_text,json) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(scope,seq) DO UPDATE SET reference_idx=excluded.reference_idx,timestamp_ms=excluded.timestamp_ms,search_text=excluded.search_text,json=excluded.json WHERE incoming.json<>excluded.json",
                params![&self.scope,seq,&record.message.id,&record.message.reference_idx,record.message.timestamp_ms,searchable(record),raw],
            ).map_err(database_error)?;
            pending.push(Event {
                kind: 0,
                incoming: Some(seq),
                request: None,
                bubble: None,
                time: Some(record.message.timestamp_ms),
            });
        }
        for request in state.requests.values() {
            let id = number(request.id)?;
            ensure!(id > 0, "history_state_invalid");
            let raw = serde_json::to_string(request)
                .map_err(|_| anyhow::anyhow!("history_record_invalid"))?;
            transaction.execute(
                "INSERT INTO requests(scope,id,json) VALUES(?1,?2,?3) ON CONFLICT(scope,id) DO UPDATE SET json=excluded.json WHERE requests.json<>excluded.json",
                params![&self.scope,id,raw],
            ).map_err(database_error)?;
            for (index, bubble) in request.bubbles.iter().enumerate() {
                let index = i64::try_from(index)
                    .map_err(|_| anyhow::anyhow!("history_integer_out_of_range"))?;
                let sent = i64::from(bubble.status == DeliveryStatus::Sent);
                let raw = serde_json::to_string(bubble)
                    .map_err(|_| anyhow::anyhow!("history_record_invalid"))?;
                transaction.execute(
                    "INSERT INTO bubbles(scope,request_id,bubble_index,sent,platform_message_id,reference_idx,timestamp_ms,search_text,json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(scope,request_id,bubble_index) DO UPDATE SET sent=excluded.sent,platform_message_id=excluded.platform_message_id,reference_idx=excluded.reference_idx,timestamp_ms=excluded.timestamp_ms,search_text=excluded.search_text,json=excluded.json WHERE bubbles.json<>excluded.json",
                    params![&self.scope,id,index,sent,&bubble.platform_message_id,&bubble.reference_idx,bubble.sent_at_ms,&bubble.text,raw],
                ).map_err(database_error)?;
                if sent == 1 {
                    pending.push(Event {
                        kind: 1,
                        incoming: None,
                        request: Some(id),
                        bubble: Some(index),
                        time: bubble.sent_at_ms,
                    });
                }
            }
        }
        // Historical seed rows are ordered by known timestamps. An event's
        // cursor never moves after its first archival eligibility.
        pending.sort_by_key(|event| {
            (
                event.time.unwrap_or(i64::MAX),
                event.kind,
                event.incoming,
                event.request,
                event.bubble,
            )
        });
        for event in pending {
            transaction.execute(
                "INSERT INTO events(scope,kind,incoming_seq,request_id,bubble_index,timestamp_ms) SELECT ?1,?2,?3,?4,?5,?6 WHERE NOT EXISTS(SELECT 1 FROM events WHERE scope=?1 AND kind=?2 AND incoming_seq IS ?3 AND request_id IS ?4 AND bubble_index IS ?5)",
                params![&self.scope,event.kind,event.incoming,event.request,event.bubble,event.time],
            ).map_err(database_error)?;
            transaction.execute(
                "UPDATE events SET timestamp_ms=?6 WHERE scope=?1 AND kind=?2 AND incoming_seq IS ?3 AND request_id IS ?4 AND bubble_index IS ?5 AND timestamp_ms IS NOT ?6",
                params![&self.scope,event.kind,event.incoming,event.request,event.bubble,event.time],
            ).map_err(database_error)?;
        }
        transaction
            .execute(
                "UPDATE conversations SET last_next_seq=?2 WHERE scope=?1 AND last_next_seq<?2",
                params![&self.scope, next],
            )
            .map_err(database_error)?;
        transaction.commit().map_err(database_error)
    }

    pub fn reader(&self) -> Result<HistoryReader> {
        let connection = open(&self.path, true)?;
        check_schema(&connection)?;
        connection
            .execute_batch("BEGIN DEFERRED")
            .map_err(database_error)?;
        // BEGIN is lazy: this actual read pins the snapshot now, not at the
        // caller's first query. Never retain a reader across an await/turn.
        let _: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM conversations WHERE scope=?1",
                [&self.scope],
                |row| row.get(0),
            )
            .map_err(database_error)?;
        Ok(HistoryReader {
            connection,
            scope: self.scope.clone(),
        })
    }
}

struct Event {
    kind: i64,
    incoming: Option<i64>,
    request: Option<i64>,
    bubble: Option<i64>,
    time: Option<i64>,
}

fn searchable(record: &MessageRecord) -> String {
    fn append_quotes(quotes: &[Quote], text: &mut String, remaining: &mut usize, depth: usize) {
        if depth >= 4 {
            return;
        }
        for quote in quotes {
            if *remaining == 0 {
                return;
            }
            *remaining -= 1;
            text.push('\n');
            text.push_str(&quote.text);
            for voice in &quote.voices {
                if let Some(transcript) = &voice.transcript {
                    text.push('\n');
                    text.push_str(transcript);
                }
            }
            append_quotes(&quote.parts, text, remaining, depth + 1);
        }
    }
    let mut text = record.message.text.clone();
    for voice in &record.message.voices {
        if let Some(transcript) = &voice.transcript {
            text.push('\n');
            text.push_str(transcript);
        }
    }
    append_quotes(&record.message.quotes, &mut text, &mut 24, 0);
    text
}

fn incoming(raw: &str) -> Result<Item> {
    serde_json::from_str(raw)
        .map(Item::Incoming)
        .map_err(|_| anyhow::anyhow!("history_record_invalid"))
}

fn bot(request: i64, index: i64, raw: &str) -> Result<Item> {
    let bubble: Bubble =
        serde_json::from_str(raw).map_err(|_| anyhow::anyhow!("history_record_invalid"))?;
    ensure!(
        bubble.status == DeliveryStatus::Sent,
        "history_record_invalid"
    );
    Ok(Item::Bot {
        request_id: u64::try_from(request)
            .map_err(|_| anyhow::anyhow!("history_record_invalid"))?,
        bubble_index: usize::try_from(index)
            .map_err(|_| anyhow::anyhow!("history_record_invalid"))?,
        bubble: Box::new(bubble),
    })
}

impl HistoryReader {
    /// Bounded identity evidence from this conversation only, including older
    /// messages pruned from GroupState. Names are observations, never authority.
    pub(super) fn participants(&self, through_seq: u64) -> Result<Vec<ParticipantObservation>> {
        let mut statement = self.connection.prepare(
            "WITH observations AS (SELECT json_extract(json,'$.message.author_id') AS author_id, json_extract(json,'$.message.author_name') AS name, json_extract(json,'$.message.timestamp_ms') AS timestamp_ms, seq FROM incoming WHERE scope=?1 AND seq<=?2), ranked AS (SELECT *, ROW_NUMBER() OVER (PARTITION BY author_id,name ORDER BY timestamp_ms DESC,seq DESC) AS position FROM observations) SELECT author_id,name,timestamp_ms,seq FROM ranked WHERE position=1 ORDER BY timestamp_ms DESC,seq DESC LIMIT 257"
        ).map_err(database_error)?;
        let rows = statement
            .query_map(params![&self.scope, number(through_seq)?], |row| {
                Ok(ParticipantObservation {
                    author_id: row.get(0)?,
                    name: row.get(1)?,
                    timestamp_ms: row.get(2)?,
                    seq: row.get::<_, i64>(3)? as u64,
                })
            })
            .map_err(database_error)?;
        rows.map(|row| row.map_err(database_error)).collect()
    }

    /// Exact platform identity for gateway deduplication, never a quote alias
    /// or a model-visible M/B cursor. Survives rolling-window pruning.
    pub(super) fn received(&self, message_id: &str) -> Result<bool> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM incoming WHERE scope=?1 AND message_id=?2)",
                params![&self.scope, message_id],
                |row| row.get(0),
            )
            .map_err(database_error)
    }

    pub fn lookup(&self, id: &str) -> Result<Option<Item>> {
        ensure!(
            !id.trim().is_empty() && id.len() <= 4096 && !id.chars().any(char::is_control),
            "history_lookup_invalid"
        );
        let seq = id
            .strip_prefix('M')
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|seq| *seq > 0);
        let selected = id
            .strip_prefix('B')
            .and_then(|value| value.split_once('.'))
            .and_then(|(request, index)| {
                Some((request.parse::<i64>().ok()?, index.parse::<i64>().ok()?))
            })
            .filter(|(request, _)| *request > 0);
        let request = selected.map(|(request, _)| request);
        let index = selected.map(|(_, index)| index).filter(|index| *index >= 0);
        let mut matches = Vec::new();
        {
            let mut statement=self.connection.prepare(
                "SELECT json FROM incoming WHERE scope=?1 AND (seq=?2 OR message_id=?3 OR reference_idx=?3) LIMIT 2"
            ).map_err(database_error)?;
            let rows = statement
                .query_map(params![&self.scope, seq, id], |row| row.get::<_, String>(0))
                .map_err(database_error)?;
            for raw in rows {
                matches.push(incoming(&raw.map_err(database_error)?)?);
            }
        }
        {
            let mut statement=self.connection.prepare(
                "SELECT request_id,bubble_index,json FROM bubbles WHERE scope=?1 AND sent=1 AND ((request_id=?2 AND bubble_index=?3) OR platform_message_id=?4 OR reference_idx=?4) LIMIT 2"
            ).map_err(database_error)?;
            let rows = statement
                .query_map(params![&self.scope, request, index, id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(database_error)?;
            for row in rows {
                let (request, index, raw) = row.map_err(database_error)?;
                matches.push(bot(request, index, &raw)?);
            }
        }
        ensure!(matches.len() <= 1, "history_lookup_ambiguous");
        Ok(matches.pop())
    }

    pub fn request(&self, id: u64) -> Result<Option<RequestRecord>> {
        let raw: Option<String> = self
            .connection
            .query_row(
                "SELECT json FROM requests WHERE scope=?1 AND id=?2",
                params![&self.scope, number(id)?],
                |row| row.get(0),
            )
            .optional()
            .map_err(database_error)?;
        raw.map(|raw| {
            serde_json::from_str(&raw).map_err(|_| anyhow::anyhow!("history_record_invalid"))
        })
        .transpose()
    }

    pub fn list(
        &self,
        after: u64,
        limit: usize,
        since_ms: Option<i64>,
        until_ms: Option<i64>,
        contains: Option<&str>,
    ) -> Result<Page> {
        ensure!((1..=50).contains(&limit), "history_page_limit_invalid");
        ensure!(
            since_ms
                .zip(until_ms)
                .is_none_or(|(start, end)| start <= end),
            "history_time_range_invalid"
        );
        ensure!(
            contains.is_none_or(|query| query.len() <= 1024),
            "history_search_invalid"
        );
        let mut statement=self.connection.prepare(
            "SELECT e.event_seq,e.kind,e.request_id,e.bubble_index,CASE WHEN e.kind=0 THEN i.json ELSE b.json END FROM events e LEFT JOIN incoming i ON i.scope=e.scope AND i.seq=e.incoming_seq LEFT JOIN bubbles b ON b.scope=e.scope AND b.request_id=e.request_id AND b.bubble_index=e.bubble_index WHERE e.scope=?1 AND e.event_seq>?2 AND (e.kind=0 OR b.sent=1) AND (?3 IS NULL OR e.timestamp_ms>=?3) AND (?4 IS NULL OR e.timestamp_ms<=?4) AND (?5 IS NULL OR instr(CASE WHEN e.kind=0 THEN i.search_text ELSE b.search_text END,?5)>0) ORDER BY e.event_seq LIMIT ?6"
        ).map_err(database_error)?;
        let rows = statement
            .query_map(
                params![
                    &self.scope,
                    number(after)?,
                    since_ms,
                    until_ms,
                    contains,
                    (limit + 1) as i64
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .map_err(database_error)?;
        let mut items = Vec::new();
        for row in rows {
            let (event, kind, request, index, raw) = row.map_err(database_error)?;
            let item = if kind == 0 {
                incoming(&raw)?
            } else {
                bot(
                    request.ok_or_else(|| anyhow::anyhow!("history_record_invalid"))?,
                    index.ok_or_else(|| anyhow::anyhow!("history_record_invalid"))?,
                    &raw,
                )?
            };
            items.push((
                u64::try_from(event).map_err(|_| anyhow::anyhow!("history_record_invalid"))?,
                item,
            ));
        }
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_cursor = has_more.then(|| items.last().expect("positive page limit").0);
        Ok(Page {
            items,
            next_cursor,
            has_more,
        })
    }

    pub fn coverage(&self) -> Result<Value> {
        let imported:Option<(i64,Option<i64>,i64,i64)>=self.connection.query_row(
            "SELECT started_at_ms,import_first_seq,import_next_seq,last_next_seq FROM conversations WHERE scope=?1", [&self.scope], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))
        ).optional().map_err(database_error)?;
        let (count,first_time,last_time):(i64,Option<i64>,Option<i64>)=self.connection.query_row(
            "SELECT COUNT(*),MIN(e.timestamp_ms),MAX(e.timestamp_ms) FROM events e LEFT JOIN bubbles b ON b.scope=e.scope AND b.request_id=e.request_id AND b.bubble_index=e.bubble_index WHERE e.scope=?1 AND (e.kind=0 OR b.sent=1)", [&self.scope], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))
        ).map_err(database_error)?;
        let incoming_count: i64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM incoming WHERE scope=?1",
                [&self.scope],
                |row| row.get(0),
            )
            .map_err(database_error)?;
        let (started, first_import, next_import, last_next) = match imported {
            Some((started, first, next, last)) => (Some(started), first, Some(next), Some(last)),
            None => (None, None, None, None),
        };
        Ok(json!({
            "archive_started_at_ms":started,"first_available_message_time_ms":first_time,"last_available_message_time_ms":last_time,
            "message_count":count,"incoming_message_count":incoming_count,"sent_bot_message_count":count-incoming_count,
            "first_import_first_seq":first_import,"first_import_next_seq":next_import,"last_next_seq":last_next,
            "automatic_ttl":false,"scope":"current_conversation_only","coverage":"observed_by_clawmingle_only",
            "before_first_import":"unknown_previously_pruned_records_cannot_be_recovered",
            "first_import_is_complete_qq_history":false,"status_history":"latest_persisted_status_not_every_transition",
            "images":"metadata_only_pixels_have_separate_retention","cursor_order":"first_archival_eligibility_not_guaranteed_platform_send_order",
            "contains_case_sensitive":true,"database_main_file_limit_bytes":MAX_DB_BYTES
        }))
    }
}

const SCHEMA: &str = "
CREATE TABLE conversations(scope TEXT PRIMARY KEY,started_at_ms INTEGER NOT NULL,import_first_seq INTEGER,import_next_seq INTEGER NOT NULL,last_next_seq INTEGER NOT NULL);
CREATE TABLE incoming(scope TEXT NOT NULL,seq INTEGER NOT NULL,message_id TEXT NOT NULL,reference_idx TEXT,timestamp_ms INTEGER NOT NULL,search_text TEXT NOT NULL,json TEXT NOT NULL,PRIMARY KEY(scope,seq),UNIQUE(scope,message_id));
CREATE INDEX incoming_reference ON incoming(scope,reference_idx);
CREATE TABLE requests(scope TEXT NOT NULL,id INTEGER NOT NULL,json TEXT NOT NULL,PRIMARY KEY(scope,id));
CREATE TABLE bubbles(scope TEXT NOT NULL,request_id INTEGER NOT NULL,bubble_index INTEGER NOT NULL,sent INTEGER NOT NULL,platform_message_id TEXT,reference_idx TEXT,timestamp_ms INTEGER,search_text TEXT NOT NULL,json TEXT NOT NULL,PRIMARY KEY(scope,request_id,bubble_index));
CREATE INDEX bubbles_platform ON bubbles(scope,platform_message_id);
CREATE INDEX bubbles_reference ON bubbles(scope,reference_idx);
CREATE TABLE events(event_seq INTEGER PRIMARY KEY AUTOINCREMENT,scope TEXT NOT NULL,kind INTEGER NOT NULL,incoming_seq INTEGER,request_id INTEGER,bubble_index INTEGER,timestamp_ms INTEGER,UNIQUE(scope,incoming_seq),UNIQUE(scope,request_id,bubble_index));
CREATE INDEX events_scope_cursor ON events(scope,event_seq);
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::social::{ChatMessage, DecisionStatus, VoiceInput, VoiceStatus};

    fn setup() -> (tempfile::TempDir, History) {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let history = History::new(directory.path(), "conversation-a").unwrap();
        (directory, history)
    }

    fn state() -> GroupState {
        GroupState {
            next_seq: 3,
            messages: vec![
                MessageRecord {
                    seq: 1,
                    message: ChatMessage {
                        id: "platform-one".into(),
                        author_id: "member".into(),
                        reference_idx: Some("reference-one".into()),
                        timestamp_ms: 100,
                        text: "first observed text".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                MessageRecord {
                    seq: 2,
                    message: ChatMessage {
                        id: "platform-two".into(),
                        author_id: "member".into(),
                        timestamp_ms: 200,
                        voices: vec![VoiceInput {
                            status: VoiceStatus::Available,
                            transcript: Some("quoted%_keyword".into()),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                    status: DecisionStatus::Bypassed,
                    ..Default::default()
                },
            ],
            requests: std::collections::BTreeMap::from([(
                1,
                RequestRecord {
                    id: 1,
                    owner_id: "member".into(),
                    source_seqs: vec![1],
                    revision: 1,
                    bubbles: vec![
                        Bubble {
                            text: "sent response".into(),
                            status: DeliveryStatus::Sent,
                            platform_message_id: Some("platform-bot".into()),
                            reference_idx: Some("reference-bot".into()),
                            sent_at_ms: Some(150),
                            ..Default::default()
                        },
                        Bubble {
                            text: "unsent draft".into(),
                            status: DeliveryStatus::Pending,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
            )]),
            ..Default::default()
        }
    }

    #[test]
    fn persists_pruned_history_and_updates_status_without_moving_cursors() {
        let (directory, history) = setup();
        let mut state = state();
        history.save(&state).unwrap();
        let before = history
            .reader()
            .unwrap()
            .list(0, 50, None, None, None)
            .unwrap();
        assert_eq!(before.items.len(), 3);
        let cursors = before
            .items
            .iter()
            .map(|(cursor, _)| *cursor)
            .collect::<Vec<_>>();
        state.messages[0].status = DecisionStatus::Decided;
        history.save(&state).unwrap();
        state.messages.clear();
        state.requests.clear();
        history.save(&state).unwrap();
        let reopened = History::new(directory.path(), "conversation-a").unwrap();
        let reader = reopened.reader().unwrap();
        let Item::Incoming(message) = reader.lookup("M1").unwrap().unwrap() else {
            panic!("incoming expected")
        };
        assert_eq!(message.status, DecisionStatus::Decided);
        assert!(reader.request(1).unwrap().is_some());
        assert!(reader.received("platform-one").unwrap());
        assert!(!reader.received("M1").unwrap());
        assert!(!reader.received("reference-one").unwrap());
        assert!(!reader.received("unseen-platform-id").unwrap());
        assert_eq!(
            reader
                .list(0, 50, None, None, None)
                .unwrap()
                .items
                .iter()
                .map(|(cursor, _)| *cursor)
                .collect::<Vec<_>>(),
            cursors
        );
        assert_eq!(reader.coverage().unwrap()["automatic_ttl"], false);
    }

    #[test]
    fn lookup_is_exact_ambiguous_and_conversation_scoped_and_bot_must_be_sent() {
        let (directory, history) = setup();
        let mut state = state();
        history.save(&state).unwrap();
        let reader = history.reader().unwrap();
        for id in ["M1", "platform-one", "reference-one"] {
            assert!(matches!(
                reader.lookup(id).unwrap(),
                Some(Item::Incoming(_))
            ));
        }
        for id in ["B1.0", "platform-bot", "reference-bot"] {
            assert!(matches!(reader.lookup(id).unwrap(), Some(Item::Bot { .. })));
        }
        assert!(reader.lookup("B1.1").unwrap().is_none());
        assert!(reader.lookup("platform").unwrap().is_none());
        drop(reader);
        state.messages[1].message.reference_idx = Some("reference-one".into());
        history.save(&state).unwrap();
        assert_eq!(
            history
                .reader()
                .unwrap()
                .lookup("reference-one")
                .err()
                .unwrap()
                .to_string(),
            "history_lookup_ambiguous"
        );
        let other = History::new(directory.path(), "conversation-b").unwrap();
        let other = other.reader().unwrap();
        assert!(other.lookup("M1").unwrap().is_none());
        assert!(other.request(1).unwrap().is_none());
        assert!(
            other
                .list(0, 50, None, None, None)
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[test]
    fn pagination_filters_and_snapshot_are_consistent_and_readonly() {
        let (_directory, history) = setup();
        let mut state = state();
        history.save(&state).unwrap();
        let reader = history.reader().unwrap();
        assert!(reader.connection.is_readonly("main").unwrap());
        assert!(
            reader
                .connection
                .execute("DELETE FROM incoming", [])
                .is_err()
        );
        let first = reader.list(0, 1, None, None, None).unwrap();
        assert!(first.has_more);
        let next = reader
            .list(first.next_cursor.unwrap(), 50, None, None, None)
            .unwrap();
        assert_eq!(next.items.len(), 2);
        assert!(!next.has_more);
        assert_eq!(
            reader
                .list(0, 50, Some(150), Some(200), None)
                .unwrap()
                .items
                .len(),
            2
        );
        assert_eq!(
            reader
                .list(0, 50, None, None, Some("%_keyword"))
                .unwrap()
                .items
                .len(),
            1
        );
        assert!(
            reader
                .list(0, 50, None, None, Some("' OR 1=1 --"))
                .unwrap()
                .items
                .is_empty()
        );
        state.messages[0].status = DecisionStatus::Decided;
        history.save(&state).unwrap();
        let Item::Incoming(original) = reader.lookup("M1").unwrap().unwrap() else {
            panic!("incoming expected")
        };
        assert_eq!(original.status, DecisionStatus::Pending);
        let Item::Incoming(updated) = history.reader().unwrap().lookup("M1").unwrap().unwrap()
        else {
            panic!("incoming expected")
        };
        assert_eq!(updated.status, DecisionStatus::Decided);
    }

    #[test]
    fn missing_or_rolled_back_json_cannot_overwrite_the_archive() {
        let (_directory, history) = setup();
        history.save(&state()).unwrap();
        assert_eq!(
            history
                .save(&GroupState::default())
                .err()
                .unwrap()
                .to_string(),
            "history_state_rewind_detected"
        );
        let mut changed = state();
        changed.messages[0].message.id = "replacement-id".into();
        assert_eq!(
            history.save(&changed).err().unwrap().to_string(),
            "history_message_identity_changed"
        );
        assert!(
            history
                .reader()
                .unwrap()
                .lookup("platform-one")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn private_permissions_and_symlink_and_hardlink_boundaries() {
        use std::os::unix::fs::symlink;
        let (directory, history) = setup();
        assert_eq!(
            fs::metadata(&history.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(history.path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        fs::set_permissions(&history.path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(history.reader().is_err());
        fs::set_permissions(&history.path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = directory.path().join("db-hardlink");
        fs::hard_link(&history.path, &link).unwrap();
        assert!(history.reader().is_err());
        fs::remove_file(link).unwrap();
        let alias = directory.path().join("alias");
        symlink(directory.path(), &alias).unwrap();
        assert!(History::new(&alias, "conversation-a").is_err());
    }

    #[test]
    fn coverage_preserves_the_initial_historical_gap_and_limits_are_set() {
        let (_directory, history) = setup();
        let mut state = state();
        state.messages.remove(0);
        state.requests.clear();
        history.save(&state).unwrap();
        let reader = history.reader().unwrap();
        let coverage = reader.coverage().unwrap();
        assert_eq!(coverage["first_import_first_seq"], 2);
        assert_eq!(coverage["first_import_is_complete_qq_history"], false);
        assert!(reader.list(0, 51, None, None, None).is_err());
        assert!(reader.list(0, 1, Some(2), Some(1), None).is_err());
        drop(reader);
        let connection = open(&history.path, false).unwrap();
        writer_settings(&connection).unwrap();
        let pages: i64 = connection
            .pragma_query_value(None, "max_page_count", |row| row.get(0))
            .unwrap();
        let size: i64 = connection
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        assert!(pages * size <= MAX_DB_BYTES as i64);
    }

    #[test]
    fn identical_saves_do_not_allocate_new_timeline_cursors_and_full_has_a_fixed_code() {
        let (_directory, history) = setup();
        let state = state();
        history.save(&state).unwrap();
        let sequence = || {
            let connection = open(&history.path, true).unwrap();
            connection
                .query_row(
                    "SELECT seq FROM sqlite_sequence WHERE name='events'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
        };
        let before = sequence();
        history.save(&state).unwrap();
        assert_eq!(sequence(), before);
        let connection = open(&history.path, false).unwrap();
        let used: i64 = connection
            .pragma_query_value(None, "page_count", |row| row.get(0))
            .unwrap();
        connection
            .pragma_update(None, "max_page_count", used + 2)
            .unwrap();
        let error = connection
            .execute(
                "CREATE TABLE capacity_fixture AS SELECT zeroblob(1048576) AS content",
                [],
            )
            .unwrap_err();
        assert_eq!(database_error(error).to_string(), "history_archive_full");
    }
}
