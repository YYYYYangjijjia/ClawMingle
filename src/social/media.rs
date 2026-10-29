//! Bounded output images, copied from one conversation's workspace before delivery.
//!
//! Model paths are resolved component by component beneath an opened directory;
//! symlinks, special files and hard links cannot publish host or other chat data.
//! Snapshots live outside the model workspace and are never modified in place.

use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime},
};

use super::store::private_directory;
use crate::model::social::{GroupState, OutputFile, OutputImage};
use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use fs2::FileExt;
use sha1::{Digest, Sha1};

pub const MAX_OUTPUT_IMAGES: usize = 2;
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_STORE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_STORE_FILES: usize = 512;
const ORPHAN_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const NOFOLLOW: i32 = 0o400000;
const NONBLOCK: i32 = 0o4000;
const DIRECTORY: i32 = 0o200000;
const INPUT_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

pub struct MediaStore {
    root: PathBuf,
    scope: PathBuf,
    input_retention: Duration,
}

impl MediaStore {
    pub fn snapshot_files(&self, workspace: &Path, paths: &[String]) -> Result<Vec<OutputFile>> {
        ensure!(paths.len() <= 2, "output_file_count_exceeded");
        let sources = paths
            .iter()
            .map(|path| {
                let name = Path::new(path)
                    .file_name()
                    .and_then(|p| p.to_str())
                    .ok_or_else(|| anyhow::anyhow!("output_file_name_invalid"))?;
                ensure!(
                    name.chars().count() <= 128 && !name.contains('\\'),
                    "output_file_name_invalid"
                );
                Ok((name.to_owned(), read_workspace_bytes(workspace, path)?))
            })
            .collect::<Result<Vec<_>>>()?;
        let _lock = self.lock()?;
        let (bytes, count) = self.usage()?;
        ensure!(
            count + sources.len() <= MAX_STORE_FILES
                && bytes + sources.iter().map(|(_, b)| b.len() as u64).sum::<u64>()
                    <= MAX_STORE_BYTES,
            "media_store_capacity_reached"
        );
        let mut output = Vec::new();
        for (name, bytes) in sources {
            ensure!(!bytes.is_empty(), "output_file_empty");
            let snapshot = format!(
                "{}.file",
                ulid::Ulid::new().to_string().to_ascii_lowercase()
            );
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(NOFOLLOW)
                .open(self.scope.join(&snapshot))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            output.push(OutputFile {
                file: snapshot,
                name,
                bytes: bytes.len() as u64,
            });
        }
        File::open(&self.scope)?.sync_all()?;
        Ok(output)
    }

    pub fn read_file(&self, asset: &OutputFile) -> Result<Vec<u8>> {
        ensure!(
            valid_asset_name(&asset.file) && asset.file.ends_with(".file"),
            "output_file_reference_invalid"
        );
        let directory = private_dir(&self.scope)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW | NONBLOCK)
            .open(fd_child(&directory, &asset.file))?;
        private_regular(&file, MAX_IMAGE_BYTES as u64)?;
        let bytes = read_bounded(file)?;
        ensure!(
            bytes.len() as u64 == asset.bytes,
            "output_file_snapshot_invalid"
        );
        Ok(bytes)
    }
    pub fn new(data_dir: &Path, scope: &str) -> Result<Self> {
        ensure!(valid_scope(scope), "media_scope_invalid");
        let root = data_dir.join("media");
        private_directory(&root)?;
        let directory = root.join(scope);
        private_directory(&directory)?;
        Ok(Self {
            root,
            scope: directory,
            input_retention: INPUT_RETENTION,
        })
    }

    pub fn with_input_retention(mut self, retention: Duration) -> Self {
        self.input_retention = retention.min(INPUT_RETENTION);
        self
    }

    pub fn stage_inputs(
        &self,
        workspace: &Path,
        data_urls: &[String],
    ) -> Result<Vec<serde_json::Value>> {
        let mut manifest = stage_inputs_for_retention(workspace, data_urls, self.input_retention)?;
        let path = self.scope.join("input-cache.json");
        if data_urls.is_empty() && path.exists() {
            let bounds: Vec<serde_json::Value> =
                serde_json::from_str(&super::config::private_read_limit(&path, 8192)?)?;
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_millis() as u64;
            let root = private_dir(workspace)?;
            let directory = private_dir(&fd_child(&root, ".clawmingle-images"))?;
            let mut retained = Vec::new();
            for mut item in manifest {
                if let Some(bound) = bounds.iter().find(|bound| bound["path"] == item["path"]) {
                    let expires = bound["expires_at_ms"]
                        .as_u64()
                        .ok_or_else(|| anyhow::anyhow!("image_cache_binding_invalid"))?;
                    if expires <= now {
                        let name = item["path"]
                            .as_str()
                            .and_then(|path| path.rsplit('/').next())
                            .filter(|name| input_name(name).is_some())
                            .expect("validated cache manifest");
                        std::fs::remove_file(fd_child(&directory, name))?;
                        continue;
                    }
                    let name = item["path"]
                        .as_str()
                        .and_then(|path| path.rsplit('/').next())
                        .filter(|name| input_name(name).is_some())
                        .expect("validated cache manifest");
                    let file = OpenOptions::new()
                        .read(true)
                        .custom_flags(NOFOLLOW | NONBLOCK)
                        .open(fd_child(&directory, name))?;
                    private_regular(&file, MAX_IMAGE_BYTES as u64)?;
                    let bytes = read_bounded(file)?;
                    ensure!(
                        bound["digest"].as_str() == Some(&format!("{:x}", Sha1::digest(&bytes))),
                        "image_cache_binding_changed"
                    );
                    item["expires_in_seconds"] = serde_json::json!(
                        item["expires_in_seconds"]
                            .as_u64()
                            .expect("cache TTL")
                            .min((expires - now) / 1000)
                    );
                }
                retained.push(item);
            }
            manifest = retained;
        }
        Ok(manifest)
    }

    /// The source binding lives outside the model-writable workspace. A hash
    /// also detects edits to cached pixels; the current input:N alone is never
    /// sufficient evidence of which historical message supplied an image.
    pub fn stage_bound_inputs(
        &self,
        workspace: &Path,
        data_urls: &[String],
        keys: &[String],
    ) -> Result<Vec<serde_json::Value>> {
        ensure!(
            data_urls.len() == keys.len(),
            "image_input_binding_mismatch"
        );
        let path = self.scope.join("input-cache.json");
        let previous = if path.exists() {
            serde_json::from_str::<Vec<serde_json::Value>>(&super::config::private_read_limit(
                &path, 8192,
            )?)?
        } else {
            Vec::new()
        };
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_millis() as u64;
        let mut manifest = self.stage_inputs(workspace, data_urls)?;
        if data_urls.is_empty() {
            return Ok(manifest);
        }
        let entries = manifest.iter_mut().zip(keys).zip(data_urls).map(|((item,key),data)| {
            let (_,encoded) = data.split_once(',').expect("validated image");
            let bytes = STANDARD.decode(encoded).expect("validated image");
            let key = format!("{:x}",Sha1::digest(key.as_bytes()));
            let expires = previous.iter()
                .find(|item|item["key"].as_str()==Some(&key))
                .and_then(|item|item["expires_at_ms"].as_u64())
                .filter(|expires|*expires>now)
                .unwrap_or(now+self.input_retention.as_millis() as u64);
            item["expires_in_seconds"] = serde_json::json!((expires-now)/1000);
            serde_json::json!({"key":key,"path":item["path"],"mime":item["mime"],"digest":format!("{:x}",Sha1::digest(&bytes)),"expires_at_ms":expires})
        }).collect::<Vec<_>>();
        let _lock = self.lock()?;
        let previous_bytes = if path.exists() {
            super::config::private_read_limit(&path, 8192)?.len() as u64
        } else {
            0
        };
        let serialized = serde_json::to_string(&entries)?;
        let (bytes, files) = self.usage()?;
        ensure!(
            bytes - previous_bytes + serialized.len() as u64 <= MAX_STORE_BYTES
                && files + usize::from(previous_bytes == 0) <= MAX_STORE_FILES,
            "media_store_capacity_reached"
        );
        crate::util::fs::atomic_write(&path, &serialized)?;
        Ok(manifest)
    }

    pub fn cached_input(&self, workspace: &Path, key: &str) -> Result<Option<String>> {
        let path = self.scope.join("input-cache.json");
        if !path.exists() {
            return Ok(None);
        }
        let raw = super::config::private_read_limit(&path, 8192)?;
        let entries: Vec<serde_json::Value> = serde_json::from_str(&raw)
            .map_err(|_| anyhow::anyhow!("image_cache_binding_invalid"))?;
        ensure!(
            entries.len() <= super::images::MAX_IMAGES,
            "image_cache_binding_invalid"
        );
        let key = format!("{:x}", Sha1::digest(key.as_bytes()));
        let Some(item) = entries
            .iter()
            .find(|item| item["key"].as_str() == Some(&key))
        else {
            return Ok(None);
        };
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_millis() as u64;
        if item["expires_at_ms"]
            .as_u64()
            .is_none_or(|expires| expires <= now)
        {
            return Ok(None);
        }
        let name = item["path"]
            .as_str()
            .and_then(|p| p.strip_prefix("/workspace/.clawmingle-images/"))
            .filter(|name| input_name(name).is_some())
            .ok_or_else(|| anyhow::anyhow!("image_cache_binding_invalid"))?;
        let workspace = private_dir(workspace)?;
        let location = fd_child(&workspace, ".clawmingle-images");
        if !location.exists() {
            return Ok(None);
        }
        let directory = private_dir(&location)?;
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW | NONBLOCK)
            .open(fd_child(&directory, name))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        private_regular(&file, MAX_IMAGE_BYTES as u64)?;
        let age = SystemTime::now().duration_since(file.metadata()?.modified()?);
        if age.is_err() || age.is_ok_and(|age| age >= self.input_retention) {
            return Ok(None);
        }
        let bytes = read_bounded(file)?;
        ensure!(
            item["digest"].as_str() == Some(&format!("{:x}", Sha1::digest(&bytes))),
            "image_cache_binding_changed"
        );
        let mime = super::images::sniff_mime(&bytes)
            .ok_or_else(|| anyhow::anyhow!("image_cache_invalid"))?;
        ensure!(item["mime"].as_str() == Some(mime), "image_cache_invalid");
        Ok(Some(format!(
            "data:{mime};base64,{}",
            STANDARD.encode(bytes)
        )))
    }

    fn lock(&self) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(NOFOLLOW | NONBLOCK)
            .open(self.root.join("store.lock"))?;
        private_regular(&file, MAX_STORE_BYTES)?;
        file.lock_exclusive()?;
        Ok(file)
    }

    /// Validate every source before creating any snapshot. A full store fails
    /// clearly; it never deletes workspaces, user files or delivery evidence.
    pub fn snapshot(&self, workspace: &Path, paths: &[String]) -> Result<Vec<OutputImage>> {
        ensure!(
            paths.len() <= MAX_OUTPUT_IMAGES,
            "output_image_count_exceeded"
        );
        self.write_snapshots(self.read_sources(workspace, paths)?)
    }

    /// Host-validated catalogue asset, copied into this chat's immutable outbox.
    /// Deleting the library entry cannot invalidate a queued or unknown send.
    pub fn sticker_snapshot(&self, bytes: &[u8], mime: &str) -> Result<OutputImage> {
        ensure!(
            bytes.len() <= MAX_IMAGE_BYTES && image_mime(bytes) == Some(mime),
            "sticker_snapshot_invalid"
        );
        Ok(self
            .write_snapshots(vec![(mime, bytes.to_vec())])?
            .remove(0))
    }

    /// Reference bytes for an isolated image worker, without creating an outbox asset.
    pub fn reference_images(&self, workspace: &Path, paths: &[String]) -> Result<Vec<Vec<u8>>> {
        ensure!(
            paths.len() <= super::images::MAX_IMAGES,
            "image_reference_count_exceeded"
        );
        Ok(self
            .read_sources(workspace, paths)?
            .into_iter()
            .map(|(_, bytes)| bytes)
            .collect())
    }

    fn read_sources(
        &self,
        workspace: &Path,
        paths: &[String],
    ) -> Result<Vec<(&'static str, Vec<u8>)>> {
        let inputs = if paths.iter().any(|path| path.starts_with("input:")) {
            self.stage_inputs(workspace, &[])?
        } else {
            Vec::new()
        };
        let resolved = paths
            .iter()
            .map(|path| {
                if path.starts_with("input:") {
                    ensure!(
                        matches!(path.as_str(), "input:1" | "input:2" | "input:3"),
                        "input_image_reference_invalid"
                    );
                    inputs
                        .iter()
                        .find(|item| item["image_ref"].as_str() == Some(path.as_str()))
                        .and_then(|item| item["path"].as_str())
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow::anyhow!("input_image_unavailable"))
                } else {
                    Ok(path.clone())
                }
            })
            .collect::<Result<Vec<_>>>()?;
        resolved
            .iter()
            .map(|path| read_workspace_image(workspace, path))
            .collect::<Result<Vec<_>>>()
    }

    /// Native image-generation bytes are supplied by the authenticated runtime,
    /// never collected from its `savedPath` or arbitrary model-supplied paths.
    pub fn generated(&self, images: &[crate::codex::GeneratedImage]) -> Result<Vec<OutputImage>> {
        ensure!(
            images.len() <= MAX_OUTPUT_IMAGES,
            "output_image_count_exceeded"
        );
        let mut sources = Vec::new();
        for image in images {
            ensure!(
                image.png.len() <= crate::codex::MAX_GENERATED_IMAGE_BYTES
                    && image_mime(&image.png) == Some("image/png"),
                "generated_image_invalid"
            );
            sources.push(super::image_delivery::prepare_native_png(&image.png)?);
        }
        self.write_snapshots(sources)
    }

    fn write_snapshots(&self, sources: Vec<(&str, Vec<u8>)>) -> Result<Vec<OutputImage>> {
        if sources.is_empty() {
            return Ok(Vec::new());
        }
        let _lock = self.lock()?;
        let (bytes, files) = self.usage()?;
        let incoming: u64 = sources.iter().map(|(_, data)| data.len() as u64).sum();
        ensure!(
            bytes + incoming <= MAX_STORE_BYTES && files + sources.len() <= MAX_STORE_FILES,
            "media_store_capacity_reached"
        );
        let mut assets = Vec::with_capacity(sources.len());
        for (mime, data) in sources {
            let extension = match mime {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/gif" => "gif",
                "image/webp" => "webp",
                _ => anyhow::bail!("output_image_format_unsupported"),
            };
            let name = format!(
                "{}.{}",
                ulid::Ulid::new().to_string().to_ascii_lowercase(),
                extension
            );
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(NOFOLLOW)
                .open(self.scope.join(&name))?;
            file.write_all(&data)?;
            file.sync_all()?;
            assets.push(OutputImage {
                file: name,
                mime: mime.into(),
                bytes: data.len() as u64,
                ..Default::default()
            });
        }
        File::open(&self.scope)?.sync_all()?;
        Ok(assets)
    }

    pub fn read(&self, asset: &OutputImage) -> Result<Vec<u8>> {
        ensure!(
            valid_asset_name(&asset.file),
            "output_image_reference_invalid"
        );
        let scope = private_dir(&self.scope)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW | NONBLOCK)
            .open(fd_child(&scope, &asset.file))?;
        private_regular(&file, MAX_IMAGE_BYTES as u64)?;
        let data = read_bounded(file)?;
        ensure!(
            data.len() as u64 == asset.bytes && image_mime(&data) == Some(asset.mime.as_str()),
            "output_image_snapshot_invalid"
        );
        Ok(data)
    }

    /// Only aged, unreferenced application snapshots in this exact chat scope
    /// are eligible. Unknown delivery records continue retaining their assets.
    pub fn cleanup(&self, state: &GroupState) -> Result<()> {
        let _lock = self.lock()?;
        let referenced = state
            .requests
            .values()
            .flat_map(|request| &request.bubbles)
            .filter_map(|bubble| bubble.image.as_ref().map(|asset| asset.file.as_str()))
            .chain(
                state
                    .requests
                    .values()
                    .flat_map(|r| &r.bubbles)
                    .filter_map(|b| b.file.as_ref().map(|f| f.file.as_str())),
            )
            .chain(
                state
                    .messages
                    .iter()
                    .flat_map(|record| super::quotes::local_images(&record.message.quotes))
                    .map(|asset| asset.file.as_str()),
            )
            .collect::<HashSet<_>>();
        let directory = private_dir(&self.scope)?;
        for entry in std::fs::read_dir(&self.scope)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !valid_asset_name(&name) || referenced.contains(name.as_str()) {
                continue;
            }
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(NOFOLLOW | NONBLOCK)
                .open(fd_child(&directory, &name))?;
            private_regular(&file, MAX_IMAGE_BYTES as u64)?;
            if SystemTime::now()
                .duration_since(file.metadata()?.modified()?)
                .is_ok_and(|age| age >= ORPHAN_RETENTION)
            {
                std::fs::remove_file(fd_child(&directory, &name))?;
            }
        }
        Ok(())
    }

    fn usage(&self) -> Result<(u64, usize)> {
        let mut bytes = 0;
        let mut files = 0;
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_name() == "store.lock" {
                continue;
            }
            let name = entry.file_name();
            ensure!(
                name.to_str().is_some_and(valid_scope),
                "media_store_entry_invalid"
            );
            private_directory(&entry.path())?;
            for asset in std::fs::read_dir(entry.path())? {
                let metadata = asset?.path().symlink_metadata()?;
                ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "media_store_entry_invalid"
                );
                bytes += metadata.len();
                files += 1;
            }
        }
        Ok((bytes, files))
    }
}

fn valid_scope(value: &str) -> bool {
    value.is_ascii()
        && value.len() == 42
        && matches!(&value[..2], "g_" | "p_")
        && value[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_asset_name(value: &str) -> bool {
    let Some((id, extension)) = value.rsplit_once('.') else {
        return false;
    };
    id.len() == 26
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && matches!(extension, "png" | "jpg" | "gif" | "webp" | "file")
}

fn fd_child(directory: &File, name: impl AsRef<Path>) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name)
}

fn private_dir(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | DIRECTORY)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == std::fs::metadata("/proc/self")?.uid()
            && metadata.permissions().mode() & 0o777 == 0o700,
        "media_private_directory_required"
    );
    Ok(file)
}

fn private_regular(file: &File, maximum: u64) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == std::fs::metadata("/proc/self")?.uid()
            && metadata.permissions().mode() & 0o777 == 0o600
            && metadata.len() <= maximum,
        "media_private_file_required"
    );
    Ok(())
}

fn read_bounded(file: File) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_IMAGE_BYTES, "output_image_too_large");
    Ok(bytes)
}

pub fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn read_workspace_image(workspace: &Path, value: &str) -> Result<(&'static str, Vec<u8>)> {
    let bytes = read_workspace_bytes(workspace, value)?;
    let mime =
        image_mime(&bytes).ok_or_else(|| anyhow::anyhow!("output_image_format_unsupported"))?;
    Ok((mime, bytes))
}

pub(super) fn read_workspace_bytes(workspace: &Path, value: &str) -> Result<Vec<u8>> {
    ensure!(
        !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control),
        "output_image_path_invalid"
    );
    let path = Path::new(value.strip_prefix("/workspace/").unwrap_or(value));
    let components = path
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name.to_owned()),
            _ => Err(anyhow::anyhow!("output_image_path_invalid")),
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(!components.is_empty(), "output_image_path_invalid");
    let mut directory = private_dir(workspace)?;
    for name in &components[..components.len() - 1] {
        let next = OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW | DIRECTORY)
            .open(fd_child(&directory, name))?;
        let metadata = next.metadata()?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == std::fs::metadata("/proc/self")?.uid()
                && metadata.permissions().mode() & 0o022 == 0,
            "output_image_directory_invalid"
        );
        directory = next;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | NONBLOCK)
        .open(fd_child(
            &directory,
            components.last().expect("nonempty path"),
        ))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == std::fs::metadata("/proc/self")?.uid()
            && metadata.len() <= MAX_IMAGE_BYTES as u64,
        "output_image_file_invalid"
    );
    read_bounded(file)
}

/// Bounded latest-image batch for this conversation, available to explicit
/// follow-up requests for at most one day. Outbound snapshots are independent.
#[cfg(test)]
pub fn stage_inputs(workspace: &Path, data_urls: &[String]) -> Result<Vec<serde_json::Value>> {
    stage_inputs_for_retention(workspace, data_urls, INPUT_RETENTION)
}

fn stage_inputs_for_retention(
    workspace: &Path,
    data_urls: &[String],
    retention: Duration,
) -> Result<Vec<serde_json::Value>> {
    super::images::validate_social_data_urls(data_urls)?;
    let workspace = private_dir(workspace)?;
    let location = fd_child(&workspace, ".clawmingle-images");
    private_directory(&location)?;
    let directory = private_dir(&location)?;
    let entries = std::fs::read_dir(&location)?
        .take(17)
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(entries.len() <= 16, "image_cache_capacity_reached");
    let mut retained = Vec::new();
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((index, extension)) = input_name(name) else {
            continue;
        };
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW | NONBLOCK)
            .open(fd_child(&directory, name))?;
        private_regular(&file, MAX_IMAGE_BYTES as u64)?;
        let age = SystemTime::now().duration_since(file.metadata()?.modified()?);
        if !data_urls.is_empty() || age.is_err() || age.as_ref().is_ok_and(|age| *age >= retention)
        {
            std::fs::remove_file(fd_child(&directory, name))?;
            continue;
        }
        ensure!(
            retained.len() < super::images::MAX_IMAGES,
            "image_cache_capacity_reached"
        );
        let data = read_bounded(file)?;
        let expected = match extension {
            "png" => "image/png",
            "jpg" => "image/jpeg",
            "gif" => "image/gif",
            _ => "image/webp",
        };
        ensure!(
            super::images::sniff_mime(&data) == Some(expected),
            "image_cache_invalid"
        );
        retained.push((
            index,
            name.to_owned(),
            expected,
            retention
                .saturating_sub(age.expect("past timestamp"))
                .as_secs(),
        ));
    }
    if data_urls.is_empty() {
        retained.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        return Ok(retained.into_iter().map(|(index,name,mime,ttl)|
            serde_json::json!({"image_index":index,"image_ref":format!("input:{index}"),"path":format!("/workspace/.clawmingle-images/{name}"),"mime":mime,"current_request":false,"expires_in_seconds":ttl})
        ).collect());
    }
    let mut manifest = Vec::new();
    for (index, value) in data_urls.iter().enumerate() {
        let (prefix, encoded) = value.split_once(',').expect("validated data URL");
        let (mime, extension) = match prefix {
            "data:image/png;base64" => ("image/png", "png"),
            "data:image/jpeg;base64" => ("image/jpeg", "jpg"),
            "data:image/gif;base64" => ("image/gif", "gif"),
            "data:image/webp;base64" => ("image/webp", "webp"),
            _ => unreachable!("validated image MIME"),
        };
        let data = STANDARD
            .decode(encoded)
            .map_err(|_| anyhow::anyhow!("image_cache_invalid"))?;
        let name = format!(
            "input-{}-{}.{}",
            index + 1,
            ulid::Ulid::new().to_string().to_ascii_lowercase(),
            extension
        );
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(NOFOLLOW)
            .open(fd_child(&directory, &name))?;
        file.write_all(&data)?;
        manifest.push(serde_json::json!({"image_index":index+1,"image_ref":format!("input:{}",index+1),"path":format!("/workspace/.clawmingle-images/{name}"),"mime":mime,"current_request":true,"expires_in_seconds":retention.as_secs()}));
    }
    Ok(manifest)
}

fn input_name(value: &str) -> Option<(usize, &str)> {
    let (body, extension) = value.strip_prefix("input-")?.rsplit_once('.')?;
    let (index, id) = match body.split_once('-') {
        Some((index, id)) => (index.parse::<usize>().ok()?, id),
        None => (1, body), // First deployed cache format, also bounded and scoped.
    };
    (matches!(index, 1..=3)
        && id.len() == 26
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && matches!(extension, "png" | "jpg" | "gif" | "webp"))
    .then_some((index, extension))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    fn setup() -> (tempfile::TempDir, PathBuf, MediaStore) {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        private_directory(&workspace).unwrap();
        let store = MediaStore::new(root.path(), &format!("g_{}", "a".repeat(40))).unwrap();
        (root, workspace, store)
    }
    fn png() -> Vec<u8> {
        b"\x89PNG\r\n\x1a\nexample".to_vec()
    }

    #[test]
    fn source_bound_cache_rejects_changed_pixels_and_does_not_relabel_a_new_batch() {
        let (_root, workspace, store) = setup();
        let data = format!("data:image/png;base64,{}", STANDARD.encode(png()));
        let manifest = store
            .stage_bound_inputs(
                &workspace,
                std::slice::from_ref(&data),
                &["remote:original".into()],
            )
            .unwrap();
        assert_eq!(
            store
                .cached_input(&workspace, "remote:original")
                .unwrap()
                .as_deref(),
            Some(data.as_str())
        );
        let file = workspace.join(
            manifest[0]["path"]
                .as_str()
                .unwrap()
                .strip_prefix("/workspace/")
                .unwrap(),
        );
        std::fs::write(file, b"\x89PNG\r\n\x1a\nchanged").unwrap();
        assert!(store.cached_input(&workspace, "remote:original").is_err());
        assert!(
            store
                .reference_images(&workspace, &["input:1".into()])
                .is_err()
        );
        store
            .stage_bound_inputs(&workspace, &[data], &["remote:new".into()])
            .unwrap();
        assert!(
            store
                .cached_input(&workspace, "remote:original")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .cached_input(&workspace, "remote:new")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn source_bound_cache_expiry_is_not_extended_by_restaging_the_same_source() {
        let (_root, workspace, store) = setup();
        let data = format!("data:image/png;base64,{}", STANDARD.encode(png()));
        store
            .stage_bound_inputs(
                &workspace,
                std::slice::from_ref(&data),
                &["remote:original".into()],
            )
            .unwrap();
        let before: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(store.scope.join("input-cache.json")).unwrap(),
        )
        .unwrap();
        store
            .stage_bound_inputs(&workspace, &[data], &["remote:original".into()])
            .unwrap();
        let after: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(store.scope.join("input-cache.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(before[0]["expires_at_ms"], after[0]["expires_at_ms"]);
        let expired = store.with_input_retention(Duration::ZERO);
        assert!(
            expired
                .cached_input(&workspace, "remote:original")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn absolute_source_expiry_also_blocks_generic_input_references_after_restaging() {
        let (_root, workspace, store) = setup();
        let data = format!("data:image/png;base64,{}", STANDARD.encode(png()));
        store
            .stage_bound_inputs(
                &workspace,
                std::slice::from_ref(&data),
                &["remote:original".into()],
            )
            .unwrap();
        store
            .stage_bound_inputs(&workspace, &[data], &["remote:original".into()])
            .unwrap();
        let path = store.scope.join("input-cache.json");
        let mut index: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        index[0]["expires_at_ms"] = serde_json::json!(1);
        crate::util::fs::atomic_write(&path, &index.to_string()).unwrap();
        assert!(store.stage_inputs(&workspace, &[]).unwrap().is_empty());
        assert!(
            store
                .reference_images(&workspace, &["input:1".into()])
                .is_err()
        );
    }

    #[test]
    fn snapshots_are_immutable_and_scoped_even_if_source_changes() {
        let (root, workspace, store) = setup();
        std::fs::write(workspace.join("chart.png"), png()).unwrap();
        let images = store
            .snapshot(&workspace, &["/workspace/chart.png".into()])
            .unwrap();
        std::fs::write(workspace.join("chart.png"), b"changed").unwrap();
        assert_eq!(store.read(&images[0]).unwrap(), png());
        assert_eq!(
            std::fs::metadata(store.scope.join(&images[0].file))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let other = MediaStore::new(root.path(), &format!("p_{}", "b".repeat(40))).unwrap();
        assert!(other.read(&images[0]).is_err());
    }

    #[test]
    fn ordinary_file_snapshots_preserve_bytes_and_unknown_delivery_retention() {
        let (root, workspace, store) = setup();
        std::fs::write(workspace.join("计划.md"), "# 原计划").unwrap();
        let snapshot = store
            .snapshot_files(&workspace, &["/workspace/计划.md".into()])
            .unwrap()
            .remove(0);
        std::fs::write(workspace.join("计划.md"), "changed").unwrap();
        assert_eq!(store.read_file(&snapshot).unwrap(), "# 原计划".as_bytes());
        let other = MediaStore::new(root.path(), &format!("p_{}", "b".repeat(40))).unwrap();
        assert!(other.read_file(&snapshot).is_err());
        symlink(root.path(), workspace.join("escape")).unwrap();
        std::fs::hard_link(workspace.join("计划.md"), workspace.join("hard.md")).unwrap();
        for path in ["../secret", "/etc/passwd", "escape/secret", "hard.md"] {
            assert!(store.snapshot_files(&workspace, &[path.into()]).is_err());
        }
        File::open(store.scope.join(&snapshot.file))
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() - ORPHAN_RETENTION - Duration::from_secs(60)),
            )
            .unwrap();
        let mut state = GroupState::default();
        state.requests.insert(
            1,
            crate::model::social::RequestRecord {
                bubbles: vec![crate::model::social::Bubble {
                    file: Some(snapshot.clone()),
                    status: crate::model::social::DeliveryStatus::Unknown,
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        store.cleanup(&state).unwrap();
        assert!(store.read_file(&snapshot).is_ok());
        state.requests.clear();
        store.cleanup(&state).unwrap();
        assert!(!store.scope.join(snapshot.file).exists());
    }

    #[test]
    fn rejects_traversal_symlinks_hardlinks_special_files_and_bad_formats() {
        let (root, workspace, store) = setup();
        std::fs::write(root.path().join("secret.png"), png()).unwrap();
        symlink(root.path().join("secret.png"), workspace.join("link.png")).unwrap();
        symlink(root.path(), workspace.join("outside")).unwrap();
        std::fs::hard_link(root.path().join("secret.png"), workspace.join("hard.png")).unwrap();
        std::fs::write(workspace.join("bad.png"), b"not an image").unwrap();
        for value in [
            "../secret.png",
            "/state/auth.json",
            "link.png",
            "outside/secret.png",
            "hard.png",
            "bad.png",
            ".",
        ] {
            assert!(
                store.snapshot(&workspace, &[value.into()]).is_err(),
                "{value}"
            );
        }
        assert_eq!(std::fs::read_dir(&store.scope).unwrap().count(), 0);
    }

    #[test]
    fn validates_the_whole_plan_and_bounds_count_size_and_asset_references() {
        let (_root, workspace, store) = setup();
        std::fs::write(workspace.join("ok.png"), png()).unwrap();
        assert!(
            store
                .snapshot(&workspace, &["ok.png".into(), "missing.png".into()])
                .is_err()
        );
        assert_eq!(std::fs::read_dir(&store.scope).unwrap().count(), 0);
        assert!(
            store
                .snapshot(&workspace, &vec!["ok.png".into(); 3])
                .is_err()
        );
        std::fs::write(workspace.join("big.png"), vec![0u8; MAX_IMAGE_BYTES + 1]).unwrap();
        assert!(store.snapshot(&workspace, &["big.png".into()]).is_err());
        assert!(
            store
                .read(&OutputImage {
                    file: "../secret.png".into(),
                    ..Default::default()
                })
                .is_err()
        );
    }

    #[test]
    fn unknown_delivery_keeps_its_asset_during_cleanup() {
        let (_root, workspace, store) = setup();
        std::fs::write(workspace.join("ok.png"), png()).unwrap();
        let image = store
            .snapshot(&workspace, &["ok.png".into()])
            .unwrap()
            .remove(0);
        let orphan = store
            .snapshot(&workspace, &["ok.png".into()])
            .unwrap()
            .remove(0);
        for asset in [&image, &orphan] {
            File::open(store.scope.join(&asset.file))
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new().set_modified(
                        SystemTime::now() - ORPHAN_RETENTION - Duration::from_secs(60),
                    ),
                )
                .unwrap();
        }
        let mut state = GroupState::default();
        state.requests.insert(
            1,
            crate::model::social::RequestRecord {
                status: crate::model::social::RequestStatus::UnknownDelivery,
                bubbles: vec![crate::model::social::Bubble {
                    image: Some(image.clone()),
                    status: crate::model::social::DeliveryStatus::Unknown,
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        store.cleanup(&state).unwrap();
        assert_eq!(store.read(&image).unwrap(), png());
        assert!(!store.scope.join(&orphan.file).exists());
    }

    #[test]
    fn retains_recent_images_for_followups_expires_them_and_refuses_a_replaced_cache_directory() {
        let (root, workspace, _) = setup();
        let data = format!("data:image/png;base64,{}", STANDARD.encode(png()));
        let manifest = stage_inputs(&workspace, std::slice::from_ref(&data)).unwrap();
        let first = manifest[0]["path"]
            .as_str()
            .unwrap()
            .strip_prefix("/workspace/")
            .unwrap();
        assert_eq!(std::fs::read(workspace.join(first)).unwrap(), png());
        let recent = stage_inputs(&workspace, &[]).unwrap();
        assert!(workspace.join(first).exists());
        assert_eq!(recent[0]["current_request"], false);
        File::open(workspace.join(first))
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() - INPUT_RETENTION - Duration::from_secs(1)),
            )
            .unwrap();
        assert!(stage_inputs(&workspace, &[]).unwrap().is_empty());
        assert!(!workspace.join(first).exists());
        let another = root.path().join("other");
        private_directory(&another).unwrap();
        symlink(root.path(), another.join(".clawmingle-images")).unwrap();
        assert!(stage_inputs(&another, &[data]).is_err());
    }

    #[test]
    fn full_media_store_rejects_new_images_but_empty_text_plan_still_works() {
        let (_root, workspace, store) = setup();
        std::fs::write(workspace.join("ok.png"), png()).unwrap();
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(store.scope.join("capacity.bin"))
            .unwrap();
        file.set_len(MAX_STORE_BYTES).unwrap();
        assert!(store.snapshot(&workspace, &["ok.png".into()]).is_err());
        assert!(store.snapshot(&workspace, &[]).unwrap().is_empty());
        assert!(store.scope.join("capacity.bin").exists());
    }

    #[test]
    fn natural_followup_references_remain_scoped_and_new_images_replace_the_batch() {
        let (root, workspace, store) = setup();
        let original = png();
        let mut second = png();
        second.extend_from_slice(b"second");
        store
            .stage_inputs(
                &workspace,
                &[
                    format!("data:image/png;base64,{}", STANDARD.encode(&original)),
                    format!("data:image/png;base64,{}", STANDARD.encode(&second)),
                ],
            )
            .unwrap();
        let outputs = store
            .snapshot(&workspace, &["input:2".into(), "input:1".into()])
            .unwrap();
        assert_eq!(store.read(&outputs[0]).unwrap(), second);
        assert_eq!(store.read(&outputs[1]).unwrap(), original);
        for invalid in ["input:0", "input:4", "input:../1"] {
            assert!(store.snapshot(&workspace, &[invalid.into()]).is_err());
        }
        let other = root.path().join("other-workspace");
        private_directory(&other).unwrap();
        assert!(store.snapshot(&other, &["input:1".into()]).is_err());
        store
            .stage_inputs(
                &workspace,
                &[format!(
                    "data:image/png;base64,{}",
                    STANDARD.encode(&second)
                )],
            )
            .unwrap();
        let latest = store.snapshot(&workspace, &["input:1".into()]).unwrap();
        assert_eq!(store.read(&latest[0]).unwrap(), second);
        assert!(store.snapshot(&workspace, &["input:2".into()]).is_err());
        assert_eq!(store.read(&outputs[1]).unwrap(), original);
    }

    #[test]
    fn expiry_respects_config_and_does_not_reassign_or_extend_image_references() {
        let (_root, workspace, store) = setup();
        let store = store.with_input_retention(Duration::from_secs(3600));
        let data = format!("data:image/png;base64,{}", STANDARD.encode(png()));
        let manifest = store
            .stage_inputs(&workspace, &[data.clone(), data])
            .unwrap();
        let first = workspace.join(
            manifest[0]["path"]
                .as_str()
                .unwrap()
                .strip_prefix("/workspace/")
                .unwrap(),
        );
        File::open(&first)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(3601)),
            )
            .unwrap();
        let second = workspace.join(
            manifest[1]["path"]
                .as_str()
                .unwrap()
                .strip_prefix("/workspace/")
                .unwrap(),
        );
        let before = std::fs::metadata(&second).unwrap().modified().unwrap();
        let recent = store.stage_inputs(&workspace, &[]).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0]["image_ref"], "input:2");
        assert!(store.snapshot(&workspace, &["input:1".into()]).is_err());
        assert!(store.snapshot(&workspace, &["input:2".into()]).is_ok());
        assert_eq!(
            std::fs::metadata(&second).unwrap().modified().unwrap(),
            before
        );
        assert!(!first.exists());
    }
}
