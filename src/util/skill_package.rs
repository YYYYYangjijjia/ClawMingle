//! Private, immutable instruction packages. No discovery outside an explicit path.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

pub const MANIFEST: &str = ".clawmingle-package.json";
pub const MAX_FILES: usize = 64;
pub const MAX_FILE_BYTES: usize = 256 * 1024;
pub const MAX_PACKAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_SKILL_BYTES: usize = 16 * 1024;
pub const MAX_PACKAGES: usize = 32;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageFile {
    pub path: String,
    pub bytes: usize,
    pub sha1: String,
    pub executable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub name: String,
    pub source: String,
    pub commit: String,
    pub installed_at: i64,
    pub files: Vec<PackageFile>,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
}

pub fn valid_relative(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 256
        && path.split('/').count() <= 8
        && path.split('/').all(|part| {
            !part.is_empty()
                && !matches!(part, "." | ".." | ".git")
                && part.len() <= 100
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
        && path != MANIFEST
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

pub fn private_directory(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "skill_package_path_invalid");
    let mut current = PathBuf::new();
    for component in path.components() {
        ensure!(
            !matches!(component, Component::CurDir | Component::ParentDir),
            "skill_package_path_invalid"
        );
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|_| anyhow::anyhow!("skill_package_path_invalid"))?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "skill_package_path_invalid"
        );
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.uid() == fs::metadata("/proc/self")?.uid()
            && metadata.permissions().mode() & 0o7777 == 0o700,
        "skill_package_private_directory_required"
    );
    Ok(())
}

pub fn read_file(path: &Path, limit: usize, executable: bool) -> Result<Vec<u8>> {
    private_directory(
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("skill_package_path_invalid"))?,
    )?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(0o400000 | 0o4000)
        .open(path)
        .map_err(|_| anyhow::anyhow!("skill_package_file_invalid"))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == fs::metadata("/proc/self")?.uid()
            && metadata.permissions().mode() & 0o7777 == if executable { 0o700 } else { 0o600 }
            && metadata.len() <= limit as u64,
        "skill_package_file_invalid"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "skill_package_file_invalid");
    Ok(bytes)
}

/// Validate every descendant and hash before exposing any package to a turn.
/// A legacy directory containing only SKILL.md remains instruction-compatible.
pub fn inspect(directory: &Path) -> Result<Option<Manifest>> {
    private_directory(directory)?;
    ensure!(
        directory
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(valid_name),
        "skill_name_invalid"
    );
    let manifest = match fs::symlink_metadata(directory.join(MANIFEST)) {
        Ok(_) => Some(
            serde_json::from_slice::<Manifest>(&read_file(
                &directory.join(MANIFEST),
                32768,
                false,
            )?)
            .map_err(|_| anyhow::anyhow!("skill_package_manifest_invalid"))?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => anyhow::bail!("skill_package_manifest_invalid"),
    };
    let mut files = Vec::new();
    let mut folders = vec![(directory.to_owned(), String::new())];
    let mut count = 0;
    let mut total = 0;
    while let Some((folder, prefix)) = folders.pop() {
        private_directory(&folder)?;
        for entry in fs::read_dir(&folder)? {
            count += 1;
            ensure!(count <= 128, "skill_package_file_count_exceeded");
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("skill_package_path_invalid"))?;
            if prefix.is_empty() && name == MANIFEST {
                continue;
            }
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            ensure!(valid_relative(&relative), "skill_package_path_invalid");
            let metadata = fs::symlink_metadata(entry.path())?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "skill_package_symlink_rejected"
            );
            if metadata.is_dir() {
                folders.push((entry.path(), relative));
                continue;
            }
            let executable = metadata.permissions().mode() & 0o100 != 0;
            let bytes = read_file(
                &entry.path(),
                if relative == "SKILL.md" {
                    MAX_SKILL_BYTES
                } else {
                    MAX_FILE_BYTES
                },
                executable,
            )?;
            total += bytes.len();
            ensure!(
                total <= MAX_PACKAGE_BYTES && files.len() < MAX_FILES,
                "skill_package_size_exceeded"
            );
            files.push(PackageFile {
                path: relative,
                bytes: bytes.len(),
                sha1: digest(&bytes),
                executable,
            });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    ensure!(
        files.iter().any(|f| f.path == "SKILL.md"),
        "skill_document_missing"
    );
    let document = read_file(&directory.join("SKILL.md"), MAX_SKILL_BYTES, false)?;
    ensure!(
        std::str::from_utf8(&document).is_ok() && !document.contains(&0),
        "skill_document_invalid"
    );
    if let Some(manifest) = &manifest {
        ensure!(
            manifest.schema_version == 1
                && Some(manifest.name.as_str()) == directory.file_name().and_then(|n| n.to_str())
                && manifest.commit.len() == 40
                && manifest.commit.bytes().all(|b| b.is_ascii_hexdigit())
                && manifest
                    .source
                    .starts_with("https://raw.githubusercontent.com/")
                && !manifest.source.contains(['?', '#', '@'])
                && manifest.files == files,
            "skill_package_integrity_failed"
        );
    } else {
        ensure!(
            files.len() == 1 && files[0].path == "SKILL.md",
            "skill_package_manifest_required"
        );
    }
    Ok(manifest)
}

pub fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, symlink};

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("example");
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        put(
            &path.join("SKILL.md"),
            b"---\nname: example\ndescription: Example\n---\nRead the resource.\n",
            false,
        );
        (root, path)
    }
    fn put(path: &Path, bytes: &[u8], executable: bool) {
        use std::io::Write;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if executable { 0o700 } else { 0o600 })
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
    }
    fn seal(path: &Path, extra: &[(&str, &[u8], bool)]) -> Manifest {
        let mut entries = vec![PackageFile {
            path: "SKILL.md".into(),
            bytes: fs::metadata(path.join("SKILL.md")).unwrap().len() as usize,
            sha1: digest(&fs::read(path.join("SKILL.md")).unwrap()),
            executable: false,
        }];
        for (name, data, executable) in extra {
            let target = path.join(name);
            if target.parent().unwrap() != path {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(target.parent().unwrap())
                    .unwrap();
            }
            put(&target, data, *executable);
            entries.push(PackageFile {
                path: (*name).into(),
                bytes: data.len(),
                sha1: digest(data),
                executable: *executable,
            });
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let manifest = Manifest {
            schema_version: 1,
            name: "example".into(),
            source:
                "https://raw.githubusercontent.com/example/repository/main/skills/example/SKILL.md"
                    .into(),
            commit: "a".repeat(40),
            installed_at: 0,
            files: entries,
        };
        put(
            &path.join(MANIFEST),
            &serde_json::to_vec(&manifest).unwrap(),
            false,
        );
        manifest
    }
    #[test]
    fn legacy_is_compatible_and_full_package_requires_exact_manifest() {
        let (_root, path) = fixture();
        assert_eq!(inspect(&path).unwrap(), None);
        let manifest = seal(
            &path,
            &[
                ("scripts/run.py", b"print('ok')", true),
                ("resources/help.txt", b"data", false),
            ],
        );
        assert_eq!(inspect(&path).unwrap(), Some(manifest));
        fs::write(path.join("resources/help.txt"), b"changed").unwrap();
        assert_eq!(
            inspect(&path).unwrap_err().to_string(),
            "skill_package_integrity_failed"
        );
    }
    #[test]
    fn links_and_loose_permissions_are_rejected_without_reading_targets() {
        let (root, path) = fixture();
        symlink(root.path().join("missing"), path.join("resource")).unwrap();
        assert!(inspect(&path).is_err());
        fs::remove_file(path.join("resource")).unwrap();
        fs::hard_link(path.join("SKILL.md"), root.path().join("hardlink")).unwrap();
        assert!(inspect(&path).is_err());
        fs::remove_file(root.path().join("hardlink")).unwrap();
        fs::set_permissions(path.join("SKILL.md"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(inspect(&path).is_err());
    }
    #[test]
    fn path_names_and_file_limits_fail_closed() {
        for name in [
            "../x",
            "/tmp/x",
            "a//b",
            "a/../b",
            "a/.git/x",
            "a\\b",
            ".clawmingle-package.json",
        ] {
            assert!(!valid_relative(name));
        }
        assert!(valid_relative("scripts/run.py"));
        let (_root, path) = fixture();
        put(&path.join("oversized"), &vec![0; MAX_FILE_BYTES + 1], false);
        assert!(inspect(&path).is_err());
    }
    #[test]
    fn unmanifested_resources_cannot_become_executable_skills() {
        let (_root, path) = fixture();
        put(&path.join("helper.py"), b"print('no')", false);
        assert_eq!(
            inspect(&path).unwrap_err().to_string(),
            "skill_package_manifest_required"
        );
    }
}
