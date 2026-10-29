//! Pinned public GitHub packages, published atomically without executing content.
use super::skill_install::{InstallSpec, InstalledSkill};
use crate::util::skill_package::{self as files, Manifest, PackageFile};
use anyhow::{Result, ensure};
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::{Client, Url};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

pub const PACKAGE_NOTICE: &str = "已保存固定Git版本的受限技能包（含脚本/资源）。只在本对话启用；脚本只能通过既有隔离工作区工具运行，不执行安装命令、不开放宿主权限或网络。";

struct Github {
    client: Client,
    api: Url,
    raw: Url,
}
#[derive(Deserialize)]
struct Commit {
    sha: String,
    commit: CommitBody,
}
#[derive(Deserialize)]
struct CommitBody {
    tree: TreeSha,
}
#[derive(Deserialize)]
struct TreeSha {
    sha: String,
}
#[derive(Deserialize)]
struct Tree {
    #[serde(default)]
    truncated: bool,
    tree: Vec<Entry>,
}
#[derive(Clone, Deserialize)]
struct Entry {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
    size: Option<usize>,
}
struct Source {
    owner: String,
    repository: String,
    reference: String,
    directory: Vec<String>,
}
struct Download {
    manifest: Manifest,
    description: String,
    files: Vec<(String, Vec<u8>, bool)>,
}

fn sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn source(spec: &InstallSpec) -> Result<Source> {
    let url = Url::parse(&spec.source)?;
    let parts = url
        .path_segments()
        .ok_or_else(|| anyhow::anyhow!("skill_source_url_invalid"))?
        .collect::<Vec<_>>();
    ensure!(
        parts.len() >= 4 && parts.last() == Some(&"SKILL.md"),
        "skill_source_url_invalid"
    );
    Ok(Source {
        owner: parts[0].into(),
        repository: parts[1].into(),
        reference: parts[2].into(),
        directory: parts[3..parts.len() - 1]
            .iter()
            .map(|s| (*s).into())
            .collect(),
    })
}
impl Github {
    fn endpoint(&self, parts: &[&str]) -> Result<Url> {
        let mut url = self.api.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("skill_source_url_invalid"))?
            .pop_if_empty()
            .extend(parts);
        Ok(url)
    }
    async fn bytes(&self, url: Url, limit: usize) -> Result<Vec<u8>> {
        let mut response = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("skill_source_fetch_failed"))?;
        ensure!(
            !response.status().is_redirection(),
            "skill_source_redirect_rejected"
        );
        ensure!(response.status().is_success(), "skill_source_fetch_failed");
        ensure!(
            response
                .content_length()
                .is_none_or(|size| size <= limit as u64),
            "skill_package_size_exceeded"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("skill_source_fetch_failed"))?
        {
            ensure!(
                bytes.len() + chunk.len() <= limit,
                "skill_package_size_exceeded"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    async fn tree(&self, src: &Source, tree: &str, recursive: bool) -> Result<Tree> {
        ensure!(sha(tree), "skill_source_tree_invalid");
        let mut url =
            self.endpoint(&["repos", &src.owner, &src.repository, "git", "trees", tree])?;
        if recursive {
            url.query_pairs_mut().append_pair("recursive", "1");
        }
        let tree: Tree = serde_json::from_slice(&self.bytes(url, 2 * 1024 * 1024).await?)
            .map_err(|_| anyhow::anyhow!("skill_source_tree_invalid"))?;
        ensure!(!tree.truncated, "skill_source_tree_truncated");
        Ok(tree)
    }
    async fn download(&self, spec: &InstallSpec) -> Result<Download> {
        let src = source(spec)?;
        let commit: Commit = serde_json::from_slice(
            &self
                .bytes(
                    self.endpoint(&[
                        "repos",
                        &src.owner,
                        &src.repository,
                        "commits",
                        &src.reference,
                    ])?,
                    65536,
                )
                .await?,
        )
        .map_err(|_| anyhow::anyhow!("skill_source_commit_invalid"))?;
        ensure!(
            sha(&commit.sha) && sha(&commit.commit.tree.sha),
            "skill_source_commit_invalid"
        );
        ensure!(src.directory.len() <= 8, "skill_package_path_invalid");
        let mut tree_sha = commit.commit.tree.sha;
        for component in &src.directory {
            let tree = self.tree(&src, &tree_sha, false).await?;
            let entry = tree
                .tree
                .into_iter()
                .find(|entry| &entry.path == component)
                .ok_or_else(|| anyhow::anyhow!("skill_source_directory_missing"))?;
            ensure!(
                entry.kind == "tree" && entry.mode == "040000",
                "skill_package_symlink_rejected"
            );
            tree_sha = entry.sha;
        }
        let tree = self.tree(&src, &tree_sha, true).await?;
        let entries = validated_entries(tree)?;
        let downloads = stream::iter(entries)
            .map(|entry| {
                let src = &src;
                let commit = &commit.sha;
                async move {
                    let mut url = self.raw.clone();
                    {
                        let mut path = url
                            .path_segments_mut()
                            .map_err(|_| anyhow::anyhow!("skill_source_url_invalid"))?;
                        path.pop_if_empty().extend([
                            src.owner.as_str(),
                            src.repository.as_str(),
                            commit.as_str(),
                        ]);
                        path.extend(src.directory.iter().map(String::as_str));
                        path.extend(entry.path.split('/'));
                    }
                    let data = self
                        .bytes(
                            url,
                            if entry.path == "SKILL.md" {
                                files::MAX_SKILL_BYTES
                            } else {
                                files::MAX_FILE_BYTES
                            },
                        )
                        .await?;
                    ensure!(Some(data.len()) == entry.size, "skill_source_blob_invalid");
                    let mut hash = Sha1::new();
                    hash.update(format!("blob {}\0", data.len()));
                    hash.update(&data);
                    ensure!(
                        format!("{:x}", hash.finalize()) == entry.sha,
                        "skill_source_blob_invalid"
                    );
                    Ok::<_, anyhow::Error>((entry.path, data, entry.mode == "100755"))
                }
            })
            .buffer_unordered(4)
            .try_collect::<Vec<_>>()
            .await?;
        let document = downloads
            .iter()
            .find(|(path, _, _)| path == "SKILL.md")
            .ok_or_else(|| anyhow::anyhow!("skill_document_missing"))?;
        ensure!(!document.2, "skill_document_invalid");
        let markdown = std::str::from_utf8(&document.1)
            .map_err(|_| anyhow::anyhow!("skill_document_not_utf8"))?;
        ensure!(!markdown.contains('\0'), "skill_document_invalid");
        let (metadata_name, description) = super::skill_install::metadata(markdown)?;
        let name = spec.name.clone().unwrap_or(metadata_name);
        let mut records = downloads
            .iter()
            .map(|(path, data, executable)| PackageFile {
                path: path.clone(),
                bytes: data.len(),
                sha1: files::digest(data),
                executable: *executable,
            })
            .collect::<Vec<_>>();
        records.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Download {
            manifest: Manifest {
                schema_version: 1,
                name,
                source: spec.source.clone(),
                commit: commit.sha,
                installed_at: chrono::Utc::now().timestamp(),
                files: records,
            },
            description,
            files: downloads,
        })
    }
}

fn validated_entries(tree: Tree) -> Result<Vec<Entry>> {
    ensure!(
        !tree.truncated && tree.tree.len() <= 128,
        "skill_source_tree_truncated"
    );
    let mut seen = BTreeSet::new();
    let mut files_out = Vec::new();
    let mut total = 0usize;
    for entry in tree.tree {
        ensure!(
            files::valid_relative(&entry.path) && seen.insert(entry.path.clone()),
            "skill_package_path_invalid"
        );
        if entry.kind == "tree" {
            ensure!(entry.mode == "040000", "skill_package_symlink_rejected");
            continue;
        }
        ensure!(
            entry.kind == "blob" && matches!(entry.mode.as_str(), "100644" | "100755"),
            "skill_package_symlink_rejected"
        );
        let size = entry
            .size
            .ok_or_else(|| anyhow::anyhow!("skill_source_blob_invalid"))?;
        let limit = if entry.path == "SKILL.md" {
            files::MAX_SKILL_BYTES
        } else {
            files::MAX_FILE_BYTES
        };
        total = total
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("skill_package_size_exceeded"))?;
        ensure!(
            size <= limit
                && total <= files::MAX_PACKAGE_BYTES
                && files_out.len() < files::MAX_FILES
                && sha(&entry.sha),
            "skill_package_size_exceeded"
        );
        files_out.push(entry);
    }
    ensure!(
        files_out.iter().any(|entry| entry.path == "SKILL.md"),
        "skill_document_missing"
    );
    Ok(files_out)
}

pub async fn install(spec: &InstallSpec, directory: &Path) -> Result<InstalledSkill> {
    let github = Github {
        client: Client::builder()
            .user_agent("ClawMingle-skill-installer")
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        api: Url::parse("https://api.github.com/")?,
        raw: Url::parse("https://raw.githubusercontent.com/")?,
    };
    let download = tokio::time::timeout(Duration::from_secs(60), github.download(spec))
        .await
        .map_err(|_| anyhow::anyhow!("skill_package_fetch_timeout"))??;
    publish(directory, &download)?;
    Ok(InstalledSkill {
        name: download.manifest.name,
        description: download.description,
    })
}

fn create_directory(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => anyhow::bail!("skill_store_write_failed"),
    }
    files::private_directory(path)
}
fn write(path: &Path, data: &[u8], executable: bool) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if executable { 0o700 } else { 0o600 })
        .custom_flags(0o400000)
        .open(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}
fn publish(directory: &Path, download: &Download) -> Result<()> {
    files::private_directory(
        directory
            .parent()
            .ok_or_else(|| anyhow::anyhow!("skill_store_directory_invalid"))?,
    )?;
    create_directory(directory)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(0o400000 | 0o4000)
        .open(directory.join(".install.lock"))?;
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let m = lock.metadata()?;
        ensure!(
            m.is_file()
                && m.nlink() == 1
                && m.uid() == fs::metadata("/proc/self")?.uid()
                && m.permissions().mode() & 0o7777 == 0o600,
            "skill_store_lock_invalid"
        );
    }
    fs2::FileExt::lock_exclusive(&lock)?;
    let destination = directory.join(&download.manifest.name);
    if fs::symlink_metadata(&destination).is_ok() {
        let existing = files::inspect(&destination)?
            .ok_or_else(|| anyhow::anyhow!("skill_existing_legacy_conflict"))?;
        ensure!(
            existing.source == download.manifest.source
                && existing.commit == download.manifest.commit
                && existing.files == download.manifest.files,
            "skill_already_exists"
        );
        return Ok(());
    }
    let entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        entries
            .iter()
            .filter(|entry| entry.file_name() != ".install.lock")
            .count()
            < files::MAX_PACKAGES,
        "skill_store_package_limit"
    );
    let staging = tempfile::Builder::new()
        .prefix(".install-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(directory)?;
    let mut directories = BTreeSet::new();
    for (relative, data, executable) in &download.files {
        let mut parent = staging.path().to_owned();
        let parts = relative.split('/').collect::<Vec<_>>();
        for component in &parts[..parts.len() - 1] {
            parent.push(component);
            create_directory(&parent)?;
            directories.insert(parent.clone());
        }
        write(&staging.path().join(relative), data, *executable)?;
    }
    write(
        &staging.path().join(files::MANIFEST),
        &serde_json::to_vec_pretty(&download.manifest)?,
        false,
    )?;
    for directory in directories.iter().rev() {
        files::sync_directory(directory)?;
    }
    files::sync_directory(staging.path())?;
    rename_new(staging.path(), &destination)?;
    files::sync_directory(directory)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn rename_new(source: &Path, destination: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    unsafe extern "C" {
        fn renameat2(
            olddirfd: i32,
            oldpath: *const std::ffi::c_char,
            newdirfd: i32,
            newpath: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    let result = unsafe { renameat2(-100, source.as_ptr(), -100, destination.as_ptr(), 1) };
    ensure!(result == 0, "skill_store_publication_failed");
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn rename_new(_source: &Path, _destination: &Path) -> Result<()> {
    anyhow::bail!("skill_install_requires_linux")
}

pub fn selected(config: &super::config::SocialConfig) -> Result<Vec<PathBuf>> {
    config
        .enabled_skills
        .iter()
        .map(|name| {
            ensure!(files::valid_name(name), "skill_name_invalid");
            let path = config.data_dir.join("skills").join(name);
            files::inspect(&path)?;
            Ok(path)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };
    const DOC: &[u8]=b"---\nname: example\ndescription: Example package\n---\nUse scripts/run.py and resources/help.txt.\n";
    fn blob(data: &[u8]) -> String {
        let mut digest = Sha1::new();
        digest.update(format!("blob {}\0", data.len()));
        digest.update(data);
        format!("{:x}", digest.finalize())
    }
    fn entry(name: &str, mode: &str, data: &[u8]) -> Entry {
        Entry {
            path: name.into(),
            mode: mode.into(),
            kind: "blob".into(),
            sha: blob(data),
            size: Some(data.len()),
        }
    }
    fn download() -> Download {
        let files: Vec<(String, Vec<u8>, bool)> = vec![
            ("SKILL.md".into(), DOC.to_vec(), false),
            ("scripts/run.py".into(), b"print('ok')".to_vec(), true),
        ];
        let records = files
            .iter()
            .map(|(path, data, executable)| PackageFile {
                path: path.clone(),
                bytes: data.len(),
                sha1: files::digest(data),
                executable: *executable,
            })
            .collect();
        Download{manifest:Manifest{schema_version:1,name:"example".into(),source:"https://raw.githubusercontent.com/example/repository/main/skills/example/SKILL.md".into(),commit:"a".repeat(40),installed_at:0,files:records},description:"Example".into(),files}
    }
    #[test]
    fn publication_is_private_atomic_idempotent_and_preserves_versions() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let skills = root.path().join("skills");
        let mut package = download();
        publish(&skills, &package).unwrap();
        assert_eq!(
            files::inspect(&skills.join("example")).unwrap(),
            Some(package.manifest.clone())
        );
        publish(&skills, &package).unwrap();
        package.manifest.commit = "b".repeat(40);
        assert_eq!(
            publish(&skills, &package).unwrap_err().to_string(),
            "skill_already_exists"
        );
        assert_eq!(
            files::inspect(&skills.join("example"))
                .unwrap()
                .unwrap()
                .commit,
            "a".repeat(40)
        );
        assert!(!fs::read_dir(&skills).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".install-")
        }));
    }
    #[test]
    fn unsafe_git_entries_and_capacity_are_rejected_before_fetch() {
        for mut item in [
            entry("../outside", "100644", b"x"),
            entry("link", "120000", b"target"),
            entry("module", "160000", b"x"),
            entry("large", "100644", b"x"),
        ] {
            if item.path == "large" {
                item.size = Some(files::MAX_FILE_BYTES + 1)
            }
            assert!(
                validated_entries(Tree {
                    truncated: false,
                    tree: vec![entry("SKILL.md", "100644", DOC), item]
                })
                .is_err()
            );
        }
        assert!(
            validated_entries(Tree {
                truncated: true,
                tree: vec![entry("SKILL.md", "100644", DOC)]
            })
            .is_err()
        );
        let mut entries = vec![entry("SKILL.md", "100644", DOC)];
        entries.extend((0..files::MAX_FILES).map(|i| entry(&format!("file-{i}"), "100644", b"x")));
        assert!(
            validated_entries(Tree {
                truncated: false,
                tree: entries
            })
            .is_err()
        );
    }
    #[tokio::test]
    async fn download_pins_commit_and_fetches_only_selected_regular_subtree() {
        let server = MockServer::start().await;
        let commit = "a".repeat(40);
        let root = "b".repeat(40);
        let selected = "c".repeat(40);
        Mock::given(method("GET"))
            .and(path("/repos/example/repository/commits/main"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"sha":commit,"commit":{"tree":{"sha":root}}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/example/repository/git/trees/{root}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"tree":[{"path":"example","mode":"040000","type":"tree","sha":selected}]}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let resources = b"local data";
        let tree = [
            entry("SKILL.md", "100644", DOC),
            entry("resources/help.txt", "100644", resources),
        ];
        let values = tree
            .iter()
            .map(|e| json!({"path":e.path,"mode":e.mode,"type":e.kind,"sha":e.sha,"size":e.size}))
            .collect::<Vec<_>>();
        Mock::given(method("GET"))
            .and(path(format!(
                "/repos/example/repository/git/trees/{selected}"
            )))
            .and(query_param("recursive", "1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"truncated":false,"tree":values})),
            )
            .expect(1)
            .mount(&server)
            .await;
        for (name, data) in [
            ("SKILL.md", DOC),
            ("resources/help.txt", resources.as_slice()),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/example/repository/{commit}/example/{name}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(data))
                .expect(1)
                .mount(&server)
                .await;
        }
        let github = Github {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            api: Url::parse(&server.uri()).unwrap(),
            raw: Url::parse(&server.uri()).unwrap(),
        };
        let package = github
            .download(&InstallSpec {
                name: Some("example".into()),
                source:
                    "https://raw.githubusercontent.com/example/repository/main/example/SKILL.md"
                        .into(),
            })
            .await
            .unwrap();
        assert_eq!(package.manifest.commit, commit);
        assert_eq!(package.manifest.files.len(), 2);
        assert!(
            package
                .files
                .iter()
                .any(|(name, data, _)| name == "resources/help.txt" && data == resources)
        );
    }
}
