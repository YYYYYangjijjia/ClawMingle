//! Operator-owned snapshots of observed chat history and canonical group memory.
//! No model, QQ, authentication profile, downloaded media or workspace is copied.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::{Arc, RwLock, RwLockReadGuard, TryLockError},
    time::{Duration, Instant},
};

use anyhow::{Result, bail, ensure};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

use super::{SocialConfig, history, store};
use crate::model::social::GroupState;

const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_STATE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TASK_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
const MAX_FILES: usize = 4096;
const NOFOLLOW_NONBLOCK: i32 = 0o400000 | 0o4000;
static STATE_BARRIER: RwLock<()> = RwLock::new(());

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackupConfig {
    pub auto_enabled: bool,
    pub directory: Option<PathBuf>,
    pub daily_hour_utc: u32,
    pub retention_days: u32,
    pub max_total_bytes: u64,
    pub reserve_free_bytes: u64,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            auto_enabled: false,
            directory: None,
            daily_hour_utc: 20, // 04:00 China Standard Time, no separate timer.
            retention_days: 7,
            max_total_bytes: 1024 * 1024 * 1024,
            reserve_free_bytes: 15 * 1024 * 1024 * 1024,
        }
    }
}

impl BackupConfig {
    pub(crate) fn validate(&self, data: &Path) -> Result<()> {
        ensure!(self.daily_hour_utc < 24, "backup_hour_invalid");
        ensure!(
            (1..=3650).contains(&self.retention_days),
            "backup_retention_invalid"
        );
        ensure!(
            (16 * 1024 * 1024..=32 * 1024 * 1024 * 1024).contains(&self.max_total_bytes),
            "backup_quota_invalid"
        );
        if let Some(directory) = &self.directory {
            normalized(directory)?;
            // The default backup subtree is the only permitted live-data descendant.
            ensure!(
                !data.starts_with(directory)
                    && (!directory.starts_with(data) || directory == &data.join("backups")),
                "backup_directory_overlaps_live_data"
            );
        }
        Ok(())
    }

    fn directory(&self, data: &Path) -> PathBuf {
        self.directory
            .clone()
            .unwrap_or_else(|| data.join("backups"))
    }
}

/// Shared lock spans the JSON write and its archive commit. Never hold across await.
pub(crate) fn state_write_guard() -> Result<RwLockReadGuard<'static, ()>> {
    STATE_BARRIER
        .read()
        .map_err(|_| anyhow::anyhow!("backup_state_barrier_failed"))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    file: String,
    bytes: u64,
    sha1: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    created_at_ms: i64,
    completed_at_ms: i64,
    files: Vec<Entry>,
    coverage: Value,
}

#[derive(Deserialize)]
struct StoredState {
    group_id: String,
    state: GroupState,
}

fn normalized(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path
                .components()
                .all(|part| !matches!(part, Component::CurDir | Component::ParentDir)),
        "backup_normalized_absolute_path_required"
    );
    Ok(())
}

fn current_uid() -> Result<u32> {
    fs::metadata("/proc/self")
        .map(|m| m.uid())
        .map_err(|_| anyhow::anyhow!("backup_owner_unavailable"))
}

fn directory(path: &Path, create: bool) -> Result<()> {
    normalized(path)?;
    if create && !path.exists() {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| anyhow::anyhow!("backup_directory_create_failed"))?;
    }
    let mut part = PathBuf::new();
    for item in path.components() {
        part.push(item);
        ensure!(
            !fs::symlink_metadata(&part)
                .map_err(|_| anyhow::anyhow!("backup_directory_unavailable"))?
                .file_type()
                .is_symlink(),
            "backup_symlink_rejected"
        );
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|_| anyhow::anyhow!("backup_directory_unavailable"))?;
    ensure!(
        metadata.is_dir() && metadata.uid() == current_uid()? && metadata.mode() & 0o7777 == 0o700,
        "backup_private_directory_required"
    );
    Ok(())
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink(),
                "backup_symlink_rejected"
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => bail!("backup_source_unavailable"),
    }
}

fn regular(path: &Path, maximum: u64) -> Result<File> {
    directory(
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("backup_path_invalid"))?,
        false,
    )?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW_NONBLOCK)
        .open(path)
        .map_err(|_| anyhow::anyhow!("backup_file_unavailable"))?;
    let metadata = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("backup_file_unavailable"))?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == current_uid()?
            && metadata.nlink() == 1
            && metadata.mode() & 0o7777 == 0o600
            && metadata.len() <= maximum,
        "backup_private_bounded_file_required"
    );
    Ok(file)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    directory(
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("backup_path_invalid"))?,
        false,
    )?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(NOFOLLOW_NONBLOCK)
        .open(path)
        .map_err(|_| anyhow::anyhow!("backup_file_create_failed"))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| anyhow::anyhow!("backup_file_write_failed"))
}

fn digest(path: &Path, maximum: u64) -> Result<(u64, String)> {
    let file = regular(path, maximum)?;
    let mut reader = file.take(maximum + 1);
    let mut hash = Sha1::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut bytes = 0u64;
    loop {
        let size = reader
            .read(&mut buffer)
            .map_err(|_| anyhow::anyhow!("backup_file_read_failed"))?;
        if size == 0 {
            break;
        }
        bytes += size as u64;
        ensure!(bytes <= maximum, "backup_file_capacity_reached");
        hash.update(&buffer[..size]);
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}

fn copy(source: &Path, destination: &Path, maximum: u64) -> Result<()> {
    let source = regular(source, maximum)?;
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(NOFOLLOW_NONBLOCK)
        .open(destination)
        .map_err(|_| anyhow::anyhow!("backup_file_create_failed"))?;
    let bytes = std::io::copy(&mut source.take(maximum + 1), &mut target)
        .map_err(|_| anyhow::anyhow!("backup_file_copy_failed"))?;
    ensure!(bytes <= maximum, "backup_file_capacity_reached");
    target
        .sync_all()
        .map_err(|_| anyhow::anyhow!("backup_file_write_failed"))
}

fn children(path: &Path) -> Result<Vec<PathBuf>> {
    if !exists(path)? {
        return Ok(Vec::new());
    }
    directory(path, false)?;
    let mut values = fs::read_dir(path)
        .map_err(|_| anyhow::anyhow!("backup_directory_read_failed"))?
        .map(|entry| {
            entry
                .map(|e| e.path())
                .map_err(|_| anyhow::anyhow!("backup_directory_read_failed"))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(values.len() <= MAX_FILES, "backup_file_capacity_reached");
    values.sort();
    Ok(values)
}

fn allowed_relative(value: &str) -> bool {
    let parts = value.split('/').collect::<Vec<_>>();
    let id = |value: &str| {
        !value.is_empty()
            && value.len() <= 256
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    matches!(
        parts.as_slice(),
        ["history", "archive.sqlite"]
            | ["private", "history", "archive.sqlite"]
            | ["persona", "IDENTITY.md"]
            | ["persona", "SOUL.md"]
    ) || match parts.as_slice() {
        ["groups", file] | ["private", "groups", file] => {
            file.strip_suffix(".json").is_some_and(id)
        }
        ["memory", "groups", group, "store.json"]
        | ["private", "memory", "groups", group, "store.json"] => {
            group.len() == 41
                && group.starts_with('G')
                && group[1..].bytes().all(|b| b.is_ascii_hexdigit())
        }
        ["tasks", file] => file.strip_suffix(".json").is_some_and(|name| {
            name.len() == 42
                && (name.starts_with("g_") || name.starts_with("p_"))
                && name[2..].bytes().all(|b| b.is_ascii_hexdigit())
        }),
        ["group-documents", scope, file] => {
            scope.len() == 40
                && scope.bytes().all(|b| b.is_ascii_hexdigit())
                && file
                    .strip_suffix(".json")
                    .is_some_and(|id| id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        }
        _ => false,
    }
}

fn ensure_parent(root: &Path, relative: &str) -> Result<()> {
    ensure!(allowed_relative(relative), "backup_manifest_path_invalid");
    let parent = Path::new(relative)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("backup_manifest_path_invalid"))?;
    let mut path = root.to_owned();
    for part in parent.components() {
        path.push(part);
        directory(&path, true)?;
    }
    Ok(())
}

fn record(root: &Path, relative: &str, entries: &mut Vec<Entry>) -> Result<()> {
    let (bytes, sha1) = digest(&root.join(relative), MAX_FILE_BYTES)?;
    entries.push(Entry {
        file: relative.into(),
        bytes,
        sha1,
    });
    ensure!(entries.len() <= MAX_FILES, "backup_file_capacity_reached");
    Ok(())
}

fn archive_summary(path: &Path) -> Result<Value> {
    let connection = history::snapshot_connection(path)?;
    let integrity: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| anyhow::anyhow!("backup_archive_invalid"))?;
    ensure!(integrity == "ok", "backup_archive_invalid");
    let mut statement = connection
        .prepare("SELECT scope,started_at_ms,last_next_seq FROM conversations ORDER BY scope")
        .map_err(|_| anyhow::anyhow!("backup_archive_invalid"))?;
    let scopes = statement.query_map([], |row| Ok(json!({"scope":row.get::<_,String>(0)?,"started_at_ms":row.get::<_,i64>(1)?,"last_next_seq":row.get::<_,i64>(2)?}))).map_err(|_| anyhow::anyhow!("backup_archive_invalid"))?.collect::<rusqlite::Result<Vec<_>>>().map_err(|_| anyhow::anyhow!("backup_archive_invalid"))?;
    let incoming: i64 = connection
        .query_row("SELECT COUNT(*) FROM incoming", [], |row| row.get(0))
        .map_err(|_| anyhow::anyhow!("backup_archive_invalid"))?;
    let requests: i64 = connection
        .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
        .map_err(|_| anyhow::anyhow!("backup_archive_invalid"))?;
    Ok(json!({"scopes":scopes,"incoming":incoming,"requests":requests}))
}

fn check_state(path: &Path, connection: &Connection) -> Result<Value> {
    let stored: StoredState = serde_json::from_reader(regular(path, MAX_STATE_BYTES)?)
        .map_err(|_| anyhow::anyhow!("backup_state_invalid"))?;
    ensure!(
        path.file_stem().and_then(|s| s.to_str()) == Some(stored.group_id.as_str())
            && stored.state.schema_version == 1,
        "backup_state_identity_invalid"
    );
    let next: Option<i64> = connection
        .query_row(
            "SELECT last_next_seq FROM conversations WHERE scope=?1",
            [&stored.group_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| anyhow::anyhow!("backup_archive_invalid"))?;
    let state_next = i64::try_from(stored.state.next_seq)
        .map_err(|_| anyhow::anyhow!("backup_state_invalid"))?;
    ensure!(next == Some(state_next), "backup_history_state_mismatch");
    for message in &stored.state.messages {
        let seq =
            i64::try_from(message.seq).map_err(|_| anyhow::anyhow!("backup_state_invalid"))?;
        let archived: Option<String> = connection
            .query_row(
                "SELECT json FROM incoming WHERE scope=?1 AND seq=?2",
                rusqlite::params![&stored.group_id, seq],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| anyhow::anyhow!("backup_archive_invalid"))?;
        ensure!(
            archived == Some(serde_json::to_string(message)?),
            "backup_history_state_mismatch"
        );
    }
    for request in stored.state.requests.values() {
        let id = i64::try_from(request.id).map_err(|_| anyhow::anyhow!("backup_state_invalid"))?;
        let archived: Option<String> = connection
            .query_row(
                "SELECT json FROM requests WHERE scope=?1 AND id=?2",
                rusqlite::params![&stored.group_id, id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| anyhow::anyhow!("backup_archive_invalid"))?;
        ensure!(
            archived == Some(serde_json::to_string(request)?),
            "backup_history_state_mismatch"
        );
    }
    Ok(
        json!({"scope":stored.group_id,"next_seq":stored.state.next_seq,"decided_through_seq":stored.state.decided_through_seq}),
    )
}

fn inventory(data: &Path) -> Result<Vec<(PathBuf, String, u64)>> {
    directory(data, false)?;
    let mut files = Vec::new();
    if exists(&data.join("persona"))? {
        directory(&data.join("persona"), false)?;
        for relative in [super::persona::IDENTITY_FILE, super::persona::SOUL_FILE] {
            let path = data.join(relative);
            if exists(&path)? {
                let bytes = regular(&path, 32000)?.metadata()?.len();
                files.push((path, relative.into(), bytes));
            }
        }
    }
    for prefix in ["", "private/"] {
        let root = data.join(prefix);
        if !exists(&root)? {
            continue;
        }
        directory(&root, false)?;
        let db = root.join("history/archive.sqlite");
        if exists(&db)? {
            let file = regular(&db, MAX_FILE_BYTES)?;
            // A committed WAL can contain pages not yet present in the main DB.
            let wal = PathBuf::from(format!("{}-wal", db.display()));
            let wal_bytes = if exists(&wal)? {
                regular(&wal, 192 * 1024 * 1024)?.metadata()?.len()
            } else {
                0
            };
            files.push((
                db,
                format!("{prefix}history/archive.sqlite"),
                file.metadata()?
                    .len()
                    .saturating_add(wal_bytes)
                    .min(MAX_FILE_BYTES)
                    + 4096,
            ));
        }
        for path in children(&root.join("groups"))? {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                bail!("backup_source_name_invalid")
            };
            let relative = format!("{prefix}groups/{name}");
            ensure!(allowed_relative(&relative), "backup_source_name_invalid");
            let bytes = regular(&path, MAX_STATE_BYTES)?.metadata()?.len();
            files.push((path, relative, bytes));
        }
    }
    for prefix in ["", "private/"] {
        for path in children(&data.join(format!("{prefix}memory/groups")))? {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                bail!("backup_source_name_invalid")
            };
            let relative = format!("{prefix}memory/groups/{name}/store.json");
            ensure!(allowed_relative(&relative), "backup_source_name_invalid");
            directory(&path, false)?;
            let source = path.join("store.json");
            let _ = regular(&source, crate::memory::group::MAX_STORE_BYTES as u64)?;
            // These stores can change independently of the chat-state barrier.
            files.push((
                source,
                relative,
                crate::memory::group::MAX_STORE_BYTES as u64,
            ));
        }
    }
    for path in children(&data.join("tasks"))? {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("backup_source_name_invalid"))?;
        if name.ends_with(".lock") || (name.contains(".json.") && name.ends_with(".tmp")) {
            continue;
        }
        let relative = format!("tasks/{name}");
        ensure!(allowed_relative(&relative), "backup_source_name_invalid");
        let _ = regular(&path, MAX_TASK_BYTES)?;
        files.push((path, relative, MAX_TASK_BYTES));
    }
    for scope in children(&data.join("group-documents"))? {
        directory(&scope, false)?;
        for source in children(&scope)? {
            let file = source
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow::anyhow!("backup_source_name_invalid"))?;
            if file == "operations.lock" || file.ends_with(".tmp") {
                continue;
            }
            let name = scope
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow::anyhow!("backup_source_name_invalid"))?;
            let relative = format!("group-documents/{name}/{file}");
            ensure!(allowed_relative(&relative), "backup_source_name_invalid");
            let size = regular(&source, 512 * 1024)?.metadata()?.len();
            files.push((source, relative, size));
        }
    }
    ensure!(files.len() <= MAX_FILES, "backup_file_capacity_reached");
    Ok(files)
}

fn root(config: &SocialConfig) -> Result<PathBuf> {
    config.backup.validate(&config.data_dir)?;
    directory(&config.data_dir, false)?;
    let path = config.backup.directory(&config.data_dir);
    directory(&path, true)?;
    Ok(path)
}

fn snapshot_name(name: &str) -> Result<()> {
    ensure!(
        name.starts_with('b')
            && name.len() <= 64
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "backup_name_invalid"
    );
    Ok(())
}

fn lock(root: &Path) -> Result<File> {
    let path = root.join("backup.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(NOFOLLOW_NONBLOCK)
        .open(&path)
        .map_err(|_| anyhow::anyhow!("backup_lock_unavailable"))?;
    let _ = regular(&path, 0)?;
    file.try_lock_exclusive()
        .map_err(|_| anyhow::anyhow!("backup_already_running"))?;
    Ok(file)
}

fn load_manifest(path: &Path) -> Result<Manifest> {
    directory(path, false)?;
    let manifest: Manifest =
        serde_json::from_reader(regular(&path.join("manifest.json"), MAX_MANIFEST_BYTES)?)
            .map_err(|_| anyhow::anyhow!("backup_manifest_invalid"))?;
    ensure!(
        manifest.schema == 1
            && manifest.created_at_ms >= 0
            && manifest.completed_at_ms >= manifest.created_at_ms
            && manifest.files.len() <= MAX_FILES,
        "backup_manifest_invalid"
    );
    let mut seen = BTreeSet::new();
    for entry in &manifest.files {
        ensure!(
            allowed_relative(&entry.file)
                && seen.insert(&entry.file)
                && entry.bytes <= MAX_FILE_BYTES
                && entry.sha1.len() == 40
                && entry.sha1.bytes().all(|b| b.is_ascii_hexdigit()),
            "backup_manifest_invalid"
        );
    }
    Ok(manifest)
}

fn completed(root: &Path) -> Result<Vec<(PathBuf, Manifest)>> {
    let mut entries = Vec::new();
    for path in children(root)? {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if matches!(name, "backup.lock" | "status.json") {
            continue;
        }
        if name.starts_with('b') {
            snapshot_name(name)?;
            entries.push((path.clone(), load_manifest(&path)?));
        } else {
            bail!("backup_unfinished_or_unrecognized_entry");
        }
    }
    entries.sort_by_key(|(_, manifest)| manifest.created_at_ms);
    Ok(entries)
}

fn contents(
    root: &Path,
    path: &Path,
    files: &mut BTreeSet<String>,
    dirs: &mut Vec<PathBuf>,
) -> Result<()> {
    ensure!(
        path.strip_prefix(root)
            .map_err(|_| anyhow::anyhow!("backup_manifest_path_invalid"))?
            .components()
            .count()
            <= 4,
        "backup_unexpected_directory"
    );
    for entry in children(path)? {
        let metadata =
            fs::symlink_metadata(&entry).map_err(|_| anyhow::anyhow!("backup_file_unavailable"))?;
        if metadata.is_dir() {
            directory(&entry, false)?;
            contents(root, &entry, files, dirs)?;
            dirs.push(entry);
        } else {
            let _ = regular(&entry, MAX_FILE_BYTES)?;
            let relative = entry
                .strip_prefix(root)
                .map_err(|_| anyhow::anyhow!("backup_manifest_path_invalid"))?
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("backup_manifest_path_invalid"))?
                .to_owned();
            ensure!(files.len() < MAX_FILES + 1, "backup_file_capacity_reached");
            files.insert(relative);
        }
    }
    Ok(())
}

fn verify(path: &Path) -> Result<Manifest> {
    let manifest = load_manifest(path)?;
    let mut expected = manifest
        .files
        .iter()
        .map(|entry| entry.file.clone())
        .collect::<BTreeSet<_>>();
    expected.insert("manifest.json".into());
    let mut found = BTreeSet::new();
    let mut directories = Vec::new();
    contents(path, path, &mut found, &mut directories)?;
    ensure!(found == expected, "backup_unexpected_files");
    let mut expected_directories = BTreeSet::new();
    for entry in &manifest.files {
        let mut parent = Path::new(&entry.file).parent();
        while let Some(value) = parent.filter(|value| !value.as_os_str().is_empty()) {
            expected_directories.insert(path.join(value));
            parent = value.parent();
        }
    }
    ensure!(
        directories.into_iter().collect::<BTreeSet<_>>() == expected_directories,
        "backup_unexpected_directory"
    );
    for entry in &manifest.files {
        let (bytes, hash) = digest(&path.join(&entry.file), MAX_FILE_BYTES)?;
        ensure!(
            bytes == entry.bytes && hash == entry.sha1,
            "backup_checksum_mismatch"
        );
    }
    for prefix in ["", "private/"] {
        let db = path.join(format!("{prefix}history/archive.sqlite"));
        let states = manifest
            .files
            .iter()
            .filter(|entry| entry.file.starts_with(&format!("{prefix}groups/")));
        if db.exists() {
            let _ = archive_summary(&db)?;
            let connection = history::snapshot_connection(&db)?;
            for entry in states {
                check_state(&path.join(&entry.file), &connection)?;
            }
        } else {
            ensure!(states.count() == 0, "backup_history_missing");
        }
    }
    Ok(manifest)
}

fn remove_snapshot(path: &Path) -> Result<()> {
    let manifest = verify(path)?;
    let mut files = BTreeSet::new();
    let mut dirs = Vec::new();
    contents(path, path, &mut files, &mut dirs)?;
    // Only verified, application-owned snapshot entries are removed; no recursive deletion.
    for entry in manifest.files {
        fs::remove_file(path.join(entry.file))
            .map_err(|_| anyhow::anyhow!("backup_retention_failed"))?;
    }
    fs::remove_file(path.join("manifest.json"))
        .map_err(|_| anyhow::anyhow!("backup_retention_failed"))?;
    for dir in dirs {
        fs::remove_dir(dir).map_err(|_| anyhow::anyhow!("backup_retention_failed"))?;
    }
    fs::remove_dir(path).map_err(|_| anyhow::anyhow!("backup_retention_failed"))
}

fn reserve(config: &SocialConfig, path: &Path, bytes: u64) -> Result<()> {
    let available =
        fs2::available_space(path).map_err(|_| anyhow::anyhow!("backup_space_check_failed"))?;
    let total = fs2::total_space(path).map_err(|_| anyhow::anyhow!("backup_space_check_failed"))?;
    let minimum = config
        .backup
        .reserve_free_bytes
        .max(15 * 1024 * 1024 * 1024)
        .max(total / 5);
    ensure!(
        available.saturating_sub(bytes) >= minimum,
        "backup_insufficient_free_space"
    );
    Ok(())
}

fn prune(config: &SocialConfig, root: &Path, incoming: u64, now: i64) -> Result<()> {
    let mut snapshots = completed(root)?;
    // Never discard older valid copies while merely assuming the newest is intact.
    if let Some((path, _)) = snapshots.last() {
        verify(path)?;
    }
    let mut total: u64 = snapshots
        .iter()
        .map(|(_, manifest)| {
            manifest.files.iter().map(|entry| entry.bytes).sum::<u64>() + MAX_MANIFEST_BYTES
        })
        .sum();
    let oldest = now.saturating_sub(i64::from(config.backup.retention_days) * 86_400_000);
    while snapshots.len() > 1 {
        let expired = snapshots[0].1.created_at_ms < oldest;
        if !expired && total.saturating_add(incoming) <= config.backup.max_total_bytes {
            break;
        }
        let (path, manifest) = snapshots.remove(0);
        remove_snapshot(&path)?;
        total = total.saturating_sub(
            manifest.files.iter().map(|entry| entry.bytes).sum::<u64>() + MAX_MANIFEST_BYTES,
        );
    }
    ensure!(
        total.saturating_add(incoming) <= config.backup.max_total_bytes,
        "backup_quota_reached"
    );
    Ok(())
}

fn create(config: &SocialConfig) -> Result<Value> {
    create_with_space_check(config, reserve)
}

fn create_with_space_check(
    config: &SocialConfig,
    check_space: fn(&SocialConfig, &Path, u64) -> Result<()>,
) -> Result<Value> {
    let root = root(config)?;
    let _lock = lock(&root)?;
    let created = chrono::Utc::now().timestamp_millis();
    // Freeze successful JSON+archive saves, not asynchronous model/QQ operations.
    let deadline = Instant::now() + Duration::from_secs(5);
    let _barrier = loop {
        match STATE_BARRIER.try_write() {
            Ok(guard) => break guard,
            Err(TryLockError::Poisoned(_)) => bail!("backup_state_barrier_failed"),
            Err(TryLockError::WouldBlock) => {
                ensure!(Instant::now() < deadline, "backup_state_barrier_busy");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    };
    let inventory = inventory(&config.data_dir)?;
    let estimate = inventory
        .iter()
        .map(|(_, _, bytes)| *bytes)
        .sum::<u64>()
        .saturating_add(MAX_MANIFEST_BYTES);
    check_space(config, &root, estimate)?;
    prune(config, &root, estimate, created)?;
    let temp = tempfile::Builder::new()
        .prefix(".incomplete-")
        .tempdir_in(&root)
        .map_err(|_| anyhow::anyhow!("backup_directory_create_failed"))?;
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
        .map_err(|_| anyhow::anyhow!("backup_directory_create_failed"))?;
    let mut entries = Vec::new();
    for (source, relative, _) in &inventory {
        ensure_parent(temp.path(), relative)?;
        let target = temp.path().join(relative);
        if relative.ends_with("archive.sqlite") {
            history::snapshot_archive(source, &target)?;
            regular(&target, MAX_FILE_BYTES)?
                .sync_all()
                .map_err(|_| anyhow::anyhow!("backup_file_write_failed"))?;
        } else if relative.starts_with("memory/") || relative.starts_with("private/memory/") {
            let lock_path = source
                .parent()
                .ok_or_else(|| anyhow::anyhow!("backup_source_unavailable"))?
                .join("store.lock");
            let memory_lock = regular(&lock_path, crate::memory::group::MAX_STORE_BYTES as u64)?;
            memory_lock
                .try_lock_shared()
                .map_err(|_| anyhow::anyhow!("backup_memory_busy"))?;
            copy(
                source,
                &target,
                crate::memory::group::MAX_STORE_BYTES as u64,
            )?;
            let value: Value = serde_json::from_reader(regular(
                &target,
                crate::memory::group::MAX_STORE_BYTES as u64,
            )?)
            .map_err(|_| anyhow::anyhow!("backup_memory_invalid"))?;
            let expected = source
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix('G'))
                .ok_or_else(|| anyhow::anyhow!("backup_memory_invalid"))?;
            ensure!(
                value["schema"] == 1 && value["group_hash"].as_str() == Some(expected),
                "backup_memory_invalid"
            );
        } else if relative.starts_with("group-documents/") {
            let directory = source
                .parent()
                .ok_or_else(|| anyhow::anyhow!("backup_source_unavailable"))?;
            let document_lock = regular(&directory.join("operations.lock"), 0)?;
            document_lock
                .try_lock_shared()
                .map_err(|_| anyhow::anyhow!("backup_documents_busy"))?;
            copy(source, &target, 512 * 1024)?;
        } else if relative.starts_with("tasks/") {
            let task_lock = regular(&source.with_extension("lock"), 0)?;
            task_lock
                .try_lock_shared()
                .map_err(|_| anyhow::anyhow!("backup_tasks_busy"))?;
            copy(source, &target, MAX_TASK_BYTES)?;
            let value: Value = serde_json::from_reader(regular(&target, MAX_TASK_BYTES)?)
                .map_err(|_| anyhow::anyhow!("backup_tasks_invalid"))?;
            let private = value["private"]
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("backup_tasks_invalid"))?;
            let identity = value["target"]
                .as_str()
                .filter(|identity| {
                    !identity.is_empty()
                        && identity
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                })
                .ok_or_else(|| anyhow::anyhow!("backup_tasks_invalid"))?;
            let expected = format!(
                "{}_{:x}.json",
                if private { "p" } else { "g" },
                Sha1::digest(identity.as_bytes())
            );
            ensure!(
                value["schema"] == 1
                    && source.file_name().and_then(|name| name.to_str()) == Some(expected.as_str())
                    && value["jobs"].is_object()
                    && value["runs"].is_object(),
                "backup_tasks_identity_invalid"
            );
        } else {
            copy(source, &target, MAX_STATE_BYTES)?;
        }
        record(temp.path(), relative, &mut entries)?;
    }
    let mut coverage = json!({"basis":"observed_by_this_installation_only","before_archive_import":"unknown_or_previously_pruned","chat_consistency":"latest_durable_json_and_archive_under_state_save_barrier","memory_consistency":"independent_canonical_store_lock_per_group","tasks_consistency":"independent_task_file_lock_per_conversation","documents_consistency":"operation_journals_only_remote_content_not_backed_up","persona":"shared_IDENTITY_and_SOUL_files_when_present","inflight_network_operations":"not_acknowledged_by_backup","excluded":["credentials","auth","configuration_except_persona_files","codex_profiles_and_rollouts","media_pixels","workspaces","gateway_state","derived_memory_mirrors"]});
    for (kind, prefix) in [("group", ""), ("private", "private/")] {
        let db = temp.path().join(format!("{prefix}history/archive.sqlite"));
        if db.exists() {
            coverage[kind] = archive_summary(&db)?;
        }
    }
    let manifest = Manifest {
        schema: 1,
        created_at_ms: created,
        completed_at_ms: chrono::Utc::now().timestamp_millis(),
        files: entries,
        coverage,
    };
    let raw = serde_json::to_vec(&manifest)?;
    ensure!(
        raw.len() as u64 <= MAX_MANIFEST_BYTES,
        "backup_manifest_capacity_reached"
    );
    write_new(&temp.path().join("manifest.json"), &raw)?;
    verify(temp.path())?;
    File::open(temp.path())
        .and_then(|file| file.sync_all())
        .map_err(|_| anyhow::anyhow!("backup_directory_sync_failed"))?;
    let name = format!("b{}-{:016x}", created, rand::random::<u64>());
    let destination = root.join(&name);
    ensure!(
        !destination
            .try_exists()
            .map_err(|_| anyhow::anyhow!("backup_destination_unavailable"))?,
        "backup_destination_exists"
    );
    fs::rename(temp.path(), &destination).map_err(|_| anyhow::anyhow!("backup_publish_failed"))?;
    File::open(&root)
        .and_then(|file| file.sync_all())
        .map_err(|_| anyhow::anyhow!("backup_directory_sync_failed"))?;
    Ok(
        json!({"backup":name,"files":manifest.files.len(),"bytes":manifest.files.iter().map(|entry|entry.bytes).sum::<u64>(),"created_at_ms":created,"verified":true}),
    )
}

/// Manual snapshot takes the same operator lock as the bot. No live data is restored.
pub fn create_backup(config: &SocialConfig) -> Result<Value> {
    let _operator = store::acquire_lock(&config.data_dir)?;
    create_observed(config)
}

fn category(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(ToString::to_string)
        .find(|value| {
            value.len() <= 96
                && value
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
        .unwrap_or_else(|| "backup_failed".into())
}

fn create_observed(config: &SocialConfig) -> Result<Value> {
    let result = create(config);
    let status = match &result {
        Ok(value) => {
            json!({"ok":true,"at_ms":chrono::Utc::now().timestamp_millis(),"result":value})
        }
        Err(error) => {
            json!({"ok":false,"at_ms":chrono::Utc::now().timestamp_millis(),"failure_code":category(error)})
        }
    };
    if let Ok(root) = root(config) {
        let path = root.join("status.json");
        let write = || -> Result<()> {
            if exists(&path)? {
                let _ = regular(&path, 65_536)?;
            }
            crate::util::fs::atomic_write(&path, &serde_json::to_string(&status)?)
                .map_err(|_| anyhow::anyhow!("backup_status_write_failed"))
        };
        if write().is_err() {
            eprintln!("聊天历史备份状态记录失败（backup_status_write_failed）。");
        }
    }
    result
}

pub fn list_backups(config: &SocialConfig) -> Result<Value> {
    let root = root(config)?;
    let _lock = lock(&root)?;
    let status = if exists(&root.join("status.json"))? {
        Some(
            serde_json::from_reader::<_, Value>(regular(&root.join("status.json"), 65_536)?)
                .map_err(|_| anyhow::anyhow!("backup_status_invalid"))?,
        )
    } else {
        None
    };
    Ok(
        json!({"backups":completed(&root)?.iter().map(|(path,manifest)|json!({"name":path.file_name().and_then(|name|name.to_str()),"created_at_ms":manifest.created_at_ms,"files":manifest.files.len(),"bytes":manifest.files.iter().map(|entry|entry.bytes).sum::<u64>()})).collect::<Vec<_>>(),"automatic_enabled":config.backup.auto_enabled,"schedule_timezone":"UTC","daily_hour":config.backup.daily_hour_utc,"last_attempt":status}),
    )
}

pub fn verify_backup(config: &SocialConfig, name: &str) -> Result<Value> {
    snapshot_name(name)?;
    let root = root(config)?;
    let _lock = lock(&root)?;
    let manifest = verify(&root.join(name))?;
    Ok(json!({"backup":name,"verified":true,"coverage":manifest.coverage}))
}

/// Operator-only offline pagination. Conversation indexes are sorted archive scopes.
pub fn query_backup(
    config: &SocialConfig,
    name: &str,
    kind: &str,
    index: usize,
    after: u64,
) -> Result<Value> {
    snapshot_name(name)?;
    ensure!(
        matches!(kind, "group" | "private"),
        "backup_scope_kind_invalid"
    );
    let root = root(config)?;
    let _lock = lock(&root)?;
    let path = root.join(name);
    verify(&path)?;
    let db = path.join(if kind == "private" {
        "private/history/archive.sqlite"
    } else {
        "history/archive.sqlite"
    });
    let connection = history::snapshot_connection(&db)?;
    let scope: String = connection
        .query_row(
            "SELECT scope FROM conversations ORDER BY scope LIMIT 1 OFFSET ?1",
            [i64::try_from(index)
                .map_err(|_| anyhow::anyhow!("backup_conversation_index_invalid"))?],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| anyhow::anyhow!("backup_archive_invalid"))?
        .ok_or_else(|| anyhow::anyhow!("backup_conversation_index_invalid"))?;
    let reader = history::snapshot_reader(&db, &scope)?;
    let page = reader.list(after, 50, None, None, None)?;
    let items = page.items.iter().map(|(cursor,item)|match item {
        history::Item::Incoming(record)=>json!({"cursor":cursor,"incoming":record}),
        history::Item::Bot {request_id,bubble_index,bubble}=>json!({"cursor":cursor,"request_id":request_id,"bubble_index":bubble_index,"bot":bubble}),
    }).collect::<Vec<_>>();
    Ok(
        json!({"items":items,"next_cursor":page.next_cursor,"has_more":page.has_more,"coverage":reader.coverage()?,"offline":true}),
    )
}

/// Export a verified data tree into a NEW directory. Never overwrite active state.
pub fn export_backup(config: &SocialConfig, name: &str, destination: &Path) -> Result<Value> {
    export_with_space_check(config, name, destination, reserve)
}

fn export_with_space_check(
    config: &SocialConfig,
    name: &str,
    destination: &Path,
    check_space: fn(&SocialConfig, &Path, u64) -> Result<()>,
) -> Result<Value> {
    snapshot_name(name)?;
    normalized(destination)?;
    ensure!(
        !destination.starts_with(&config.data_dir) && !config.data_dir.starts_with(destination),
        "backup_export_overlaps_live_data"
    );
    let root = root(config)?;
    ensure!(
        !destination.starts_with(&root) && !root.starts_with(destination),
        "backup_export_overlaps_backups"
    );
    let _lock = lock(&root)?;
    let source = root.join(name);
    let manifest = verify(&source)?;
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("backup_export_parent_required"))?;
    directory(parent, false)?;
    check_space(
        config,
        parent,
        manifest.files.iter().map(|entry| entry.bytes).sum::<u64>() + MAX_MANIFEST_BYTES,
    )?;
    // create_dir is the exclusive claim; no existing directory is replaced.
    fs::DirBuilder::new()
        .mode(0o700)
        .create(destination)
        .map_err(|_| anyhow::anyhow!("backup_export_destination_exists_or_unavailable"))?;
    for entry in &manifest.files {
        ensure_parent(destination, &entry.file)?;
        copy(
            &source.join(&entry.file),
            &destination.join(&entry.file),
            MAX_FILE_BYTES,
        )?;
    }
    copy(
        &source.join("manifest.json"),
        &destination.join("manifest.json"),
        MAX_MANIFEST_BYTES,
    )?;
    verify(destination)?;
    Ok(
        json!({"exported":true,"verified":true,"active_data_overwritten":false,"authentication_included":false}),
    )
}

pub(crate) struct Maintenance(tokio::task::JoinHandle<()>);
impl Drop for Maintenance {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) fn start_maintenance(config: Arc<SocialConfig>) -> Option<Maintenance> {
    if !config.backup.auto_enabled {
        return None;
    }
    Some(Maintenance(tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        let mut attempted_day = None;
        loop {
            interval.tick().await;
            let now = chrono::Utc::now().timestamp();
            let day = now.div_euclid(86_400);
            if now.rem_euclid(86_400) < i64::from(config.backup.daily_hour_utc) * 3600
                || attempted_day == Some(day)
            {
                continue;
            }
            attempted_day = Some(day);
            let config = config.clone();
            let result = tokio::task::spawn_blocking(move || {
                let path = root(&config)?;
                if completed(&path)?
                    .iter()
                    .any(|(_, manifest)| manifest.created_at_ms.div_euclid(86_400_000) == day)
                {
                    return Ok(json!({"already_backed_up_today":true}));
                }
                create_observed(&config)
            })
            .await;
            match result {
                Ok(Ok(_)) => println!("聊天历史每日备份完成（或今日已有备份），未调用模型或 QQ。"),
                Ok(Err(error)) => eprintln!(
                    "聊天历史每日备份失败（{}）；本日不自动重试。",
                    category(&error)
                ),
                Err(_) => {
                    eprintln!("聊天历史每日备份失败（backup_worker_failed）；本日不自动重试。")
                }
            }
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::social::{Action, ChatMessage, Decision, DecisionStatus, MessageRecord};
    use std::os::unix::fs::symlink;

    fn space_available(_: &SocialConfig, _: &Path, _: u64) -> Result<()> {
        Ok(())
    }

    fn fixture() -> (tempfile::TempDir, SocialConfig, GroupState) {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let data = directory.path().join("data");
        super::directory(&data, true).unwrap();
        let config = SocialConfig {
            data_dir: data,
            allowed_groups: vec!["group-one".into()],
            ..Default::default()
        };
        let state = GroupState {
            next_seq: 3,
            messages: vec![
                MessageRecord {
                    seq: 1,
                    message: ChatMessage {
                        id: "platform-one".into(),
                        author_id: "human-one".into(),
                        text: "original pending text".into(),
                        timestamp_ms: 1000,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                MessageRecord {
                    seq: 2,
                    status: DecisionStatus::Decided,
                    decision: Some(Decision {
                        action: Action::Silent,
                        source: "jev".into(),
                        ..Default::default()
                    }),
                    message: ChatMessage {
                        id: "platform-two".into(),
                        author_id: "human-two".into(),
                        text: "silent text".into(),
                        timestamp_ms: 2000,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        StoreFixture::save(&config.data_dir, "group-one", &state);
        (directory, config, state)
    }

    struct StoreFixture;
    impl StoreFixture {
        fn save(data: &Path, scope: &str, state: &GroupState) {
            store::Store::new(data, scope).unwrap().save(state).unwrap();
        }
    }

    fn make(config: &SocialConfig) -> String {
        create_with_space_check(config, space_available).unwrap()["backup"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn snapshot_exports_only_the_two_shared_persona_files() {
        let (_directory, config, _state) = fixture();
        let persona = config.data_dir.join("persona");
        directory(&persona, true).unwrap();
        write_new(&persona.join("IDENTITY.md"), b"Identity fixture").unwrap();
        write_new(&persona.join("SOUL.md"), b"Soul fixture").unwrap();
        write_new(&persona.join("other.txt"), b"not included").unwrap();
        let name = make(&config);
        let snapshot = root(&config).unwrap().join(&name);
        let manifest = verify(&snapshot).unwrap();
        for (relative, expected) in [
            ("persona/IDENTITY.md", "Identity fixture"),
            ("persona/SOUL.md", "Soul fixture"),
        ] {
            assert!(manifest.files.iter().any(|entry| entry.file == relative));
            assert_eq!(
                std::fs::read_to_string(snapshot.join(relative)).unwrap(),
                expected
            );
        }
        assert!(!allowed_relative("persona/other.txt"));
        assert!(!allowed_relative("persona/../auth.json"));
    }

    #[test]
    fn snapshot_preserves_pruned_originals_latest_states_and_private_scopes_without_auth() {
        let (_directory, config, mut state) = fixture();
        state.messages.remove(0);
        StoreFixture::save(&config.data_dir, "group-one", &state);
        let private = config.data_dir.join("private");
        directory(&private, true).unwrap();
        let mut private_state = state.clone();
        private_state.messages[0].message.text = "private-only text".into();
        StoreFixture::save(&private, "human-private", &private_state);
        let runtime = config.data_dir.join("runtime");
        directory(&runtime, true).unwrap();
        write_new(
            &runtime.join("auth.json"),
            b"excluded authentication fixture",
        )
        .unwrap();
        let name = make(&config);
        let manifest = verify(&root(&config).unwrap().join(&name)).unwrap();
        assert!(
            !manifest
                .files
                .iter()
                .any(|file| file.file.contains("auth") || file.file.contains("runtime"))
        );
        let group = query_backup(&config, &name, "group", 0, 0).unwrap();
        assert_eq!(group["items"].as_array().unwrap().len(), 2);
        assert_eq!(
            group["items"][0]["incoming"]["message"]["text"],
            "original pending text"
        );
        assert_eq!(
            group["items"][1]["incoming"]["decision"]["action"],
            serde_json::to_value(Action::Silent).unwrap()
        );
        let private = query_backup(&config, &name, "private", 0, 0).unwrap();
        assert_eq!(
            private["items"][0]["incoming"]["message"]["text"],
            "private-only text"
        );
        assert_eq!(private["coverage"]["automatic_ttl"], false);
        state.messages[0].message.text = "updated after snapshot".into();
        StoreFixture::save(&config.data_dir, "group-one", &state);
        assert_eq!(
            query_backup(&config, &name, "group", 0, 0).unwrap()["items"][1]["incoming"]["message"]
                ["text"],
            "silent text"
        );
    }

    #[test]
    fn inconsistent_canonical_state_is_rejected_and_no_snapshot_is_published() {
        let (_directory, config, mut state) = fixture();
        state.messages[0].message.text = "not committed to archive".into();
        let raw = json!({"group_id":"group-one","state":state});
        crate::util::fs::atomic_write(
            &config.data_dir.join("groups/group-one.json"),
            &raw.to_string(),
        )
        .unwrap();
        assert_eq!(
            create_with_space_check(&config, space_available)
                .unwrap_err()
                .to_string(),
            "backup_history_state_mismatch"
        );
        assert!(completed(&root(&config).unwrap()).unwrap().is_empty());
    }

    #[test]
    fn checksum_and_unexpected_paths_are_rejected_and_exports_never_replace_data() {
        let (directory, config, mut state) = fixture();
        state.group_profile = Some(crate::model::social::GroupProfile {
            fields: crate::model::social::GroupProfileFields {
                purpose: "工作群".into(),
                ..Default::default()
            },
            revision: 3,
            source_seq: 2,
            ..Default::default()
        });
        StoreFixture::save(&config.data_dir, "group-one", &state);
        let name = make(&config);
        let export = directory.path().join("restored");
        export_with_space_check(&config, &name, &export, space_available).unwrap();
        assert_eq!(
            store::Store::new(&export, "group-one")
                .unwrap()
                .load()
                .unwrap()
                .next_seq,
            3
        );
        let restored = store::Store::new(&export, "group-one")
            .unwrap()
            .load()
            .unwrap();
        let profile = restored.group_profile.unwrap();
        assert_eq!(profile.revision, 3);
        assert_eq!(profile.fields.purpose, "工作群");
        assert!(export_with_space_check(&config, &name, &export, space_available).is_err());
        assert!(
            export_with_space_check(&config, &name, &config.data_dir, space_available).is_err()
        );
        let snapshot = root(&config).unwrap().join(&name);
        crate::util::fs::atomic_write(&snapshot.join("groups/group-one.json"), "{}").unwrap();
        assert_eq!(
            verify(&snapshot).err().unwrap().to_string(),
            "backup_checksum_mismatch"
        );
        assert!(verify_backup(&config, "../runtime").is_err());
    }

    #[test]
    fn source_symlinks_hardlinks_and_unfinished_backups_fail_closed() {
        let (directory, config, _state) = fixture();
        let state = config.data_dir.join("groups/group-one.json");
        fs::hard_link(&state, directory.path().join("state-hardlink")).unwrap();
        assert_eq!(
            create_with_space_check(&config, space_available)
                .unwrap_err()
                .to_string(),
            "backup_private_bounded_file_required"
        );
        fs::remove_file(directory.path().join("state-hardlink")).unwrap();
        symlink(directory.path(), config.data_dir.join("private")).unwrap();
        assert_eq!(
            create_with_space_check(&config, space_available)
                .unwrap_err()
                .to_string(),
            "backup_symlink_rejected"
        );
        fs::remove_file(config.data_dir.join("private")).unwrap();
        super::directory(&root(&config).unwrap().join(".incomplete-leftover"), true).unwrap();
        assert_eq!(
            create_with_space_check(&config, space_available)
                .unwrap_err()
                .to_string(),
            "backup_unfinished_or_unrecognized_entry"
        );
    }

    #[test]
    fn quota_rotation_never_deletes_the_newest_valid_snapshot_and_space_failure_is_explicit() {
        let (_directory, mut config, _state) = fixture();
        let oldest = make(&config);
        let newest = make(&config);
        let root = root(&config).unwrap();
        let mut old = load_manifest(&root.join(&oldest)).unwrap();
        old.created_at_ms = 1;
        crate::util::fs::atomic_write(
            &root.join(&oldest).join("manifest.json"),
            &serde_json::to_string(&old).unwrap(),
        )
        .unwrap();
        config.backup.max_total_bytes = 1;
        assert!(prune(&config, &root, 1, chrono::Utc::now().timestamp_millis()).is_err());
        assert!(!root.join(&oldest).exists());
        assert!(root.join(&newest).exists());
        config.backup.max_total_bytes = BackupConfig::default().max_total_bytes;
        let no_space = |_: &SocialConfig, _: &Path, _: u64| -> Result<()> {
            bail!("backup_insufficient_free_space")
        };
        assert_eq!(
            create_with_space_check(&config, no_space)
                .unwrap_err()
                .to_string(),
            "backup_insufficient_free_space"
        );
        verify(&root.join(&newest)).unwrap();
    }

    #[test]
    fn state_save_waits_for_snapshot_barrier_and_manual_backup_respects_operator_lock() {
        let (_directory, config, state) = fixture();
        let writer = STATE_BARRIER.write().unwrap();
        let data = config.data_dir.clone();
        let (started, receiver) = std::sync::mpsc::channel();
        let (done, completed) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            started.send(()).unwrap();
            StoreFixture::save(&data, "group-one", &state);
            done.send(()).unwrap();
        });
        receiver.recv().unwrap();
        assert!(completed.recv_timeout(Duration::from_millis(30)).is_err());
        drop(writer);
        thread.join().unwrap();
        completed.recv().unwrap();
        let _operator = store::acquire_lock(&config.data_dir).unwrap();
        assert_eq!(
            create_backup(&config).unwrap_err().to_string(),
            "social_runtime_already_running"
        );
    }

    #[test]
    fn group_document_operation_journal_is_backed_up_but_credentials_are_not() {
        let (_directory, config, _state) = fixture();
        let scope = "a".repeat(40);
        let name = format!("{}.json", "b".repeat(40));
        directory(&config.data_dir.join("group-documents"), true).unwrap();
        let path = config.data_dir.join("group-documents").join(&scope);
        directory(&path, true).unwrap();
        crate::util::fs::atomic_write(&path.join("operations.lock"), "").unwrap();
        crate::util::fs::atomic_write(&path.join(&name), r#"{"result":{"status":"unknown"}}"#)
            .unwrap();
        let backup = make(&config);
        let manifest = load_manifest(&root(&config).unwrap().join(backup)).unwrap();
        assert!(
            manifest
                .files
                .iter()
                .any(|file| file.file == format!("group-documents/{scope}/{name}"))
        );
        assert!(
            !manifest
                .files
                .iter()
                .any(|file| file.file.ends_with(".lock") || file.file.ends_with(".token"))
        );
    }

    #[test]
    fn memory_canonical_and_conversation_task_state_are_included_without_derived_mirrors() {
        let (_directory, config, _state) = fixture();
        crate::memory::group::GroupMemoryStore::new(&config.data_dir, "group-one").unwrap();
        crate::memory::group::GroupMemoryStore::new(
            config.data_dir.join("private"),
            "human-private",
        )
        .unwrap();
        let tasks = config.data_dir.join("tasks");
        directory(&tasks, true).unwrap();
        let file = tasks.join(format!("p_{:x}.json", Sha1::digest(b"human-private")));
        write_new(&file,&serde_json::to_vec(&json!({"schema":1,"private":true,"target":"human-private","push_enabled":false,"next_job":1,"next_run":1,"jobs":{},"runs":{}})).unwrap()).unwrap();
        write_new(&file.with_extension("lock"), b"").unwrap();
        let name = make(&config);
        let manifest = verify(&root(&config).unwrap().join(&name)).unwrap();
        assert!(
            manifest
                .files
                .iter()
                .any(|file| file.file.starts_with("memory/groups/")
                    && file.file.ends_with("/store.json"))
        );
        assert!(
            manifest
                .files
                .iter()
                .any(|file| file.file.starts_with("tasks/p_"))
        );
        assert!(
            manifest
                .files
                .iter()
                .any(|file| file.file.starts_with("private/memory/groups/")
                    && file.file.ends_with("/store.json"))
        );
        assert!(
            !manifest
                .files
                .iter()
                .any(|file| file.file.contains("documents") || file.file.ends_with(".lock"))
        );
        let tampered =
            json!({"schema":1,"private":false,"target":"different-scope","jobs":{},"runs":{}});
        crate::util::fs::atomic_write(&file, &tampered.to_string()).unwrap();
        assert_eq!(
            create_with_space_check(&config, space_available)
                .unwrap_err()
                .to_string(),
            "backup_tasks_identity_invalid"
        );
    }

    #[tokio::test]
    async fn automatic_snapshots_are_disabled_by_default() {
        let (_directory, config, _state) = fixture();
        assert!(start_maintenance(Arc::new(config)).is_none());
    }
}
