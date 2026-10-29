//! Read-only system runtime for social workspaces.
//!
//! This only describes bubblewrap mounts. The caller owns fresh-root setup,
//! Codex/auth isolation, per-conversation writable workspaces and tool policy.
//! Optional executable paths are operator settings read by the parent service;
//! their environment variables are never forwarded to the child here.

use std::{
    ffi::OsString,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use tokio::process::Command;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeTools {
    pub node: bool,
    pub rg: bool,
}

/// Append only read-only runtime mounts and fresh-root compatibility symlinks.
///
/// Call before binding any writable workspace/state directories. `/tools` must
/// be a fresh mountpoint, not below an existing writable bind. The caller should
/// put `/tools:/usr/bin` in the child's clean PATH. This function neither starts
/// programs nor copies files, and does not set any child environment variables.
///
/// `CLAWMINGLE_NODE_BINARY` and `CLAWMINGLE_RG_BINARY`, when present, must point
/// to trusted native executables. Invalid explicit paths fail rather than fall
/// back. Without an override, only fixed system paths are checked; missing tools
/// are reported by the returned flags and are never installed automatically.
pub fn append_runtime_mounts(cmd: &mut Command) -> Result<RuntimeTools> {
    let node = std::env::var_os("CLAWMINGLE_NODE_BINARY").map(PathBuf::from);
    let rg = std::env::var_os("CLAWMINGLE_RG_BINARY").map(PathBuf::from);
    let current_uid = current_uid()?;
    append_with_sources(cmd, node.as_deref(), rg.as_deref(), current_uid)
}

fn append_with_sources(
    cmd: &mut Command,
    node: Option<&Path>,
    rg: Option<&Path>,
    current_uid: u32,
) -> Result<RuntimeTools> {
    let (arguments, tools) = runtime_mount_arguments(node, rg, current_uid)?;
    // Build and validate the complete plan first: errors never leave a partial
    // runtime mount plan in the caller's command.
    cmd.args(arguments);
    Ok(tools)
}

pub(crate) fn validated_native_binary(path: &Path) -> Result<PathBuf> {
    validate_binary(path, current_uid()?)
}

fn runtime_mount_arguments(
    node_override: Option<&Path>,
    rg_override: Option<&Path>,
    current_uid: u32,
) -> Result<(Vec<OsString>, RuntimeTools)> {
    validate_usr()?;
    let node = optional_binary(
        node_override,
        &[Path::new("/usr/local/bin/node"), Path::new("/usr/bin/node")],
        current_uid,
    )
    .context("Node runtime executable is invalid")?;
    let rg = optional_binary(
        rg_override,
        &[Path::new("/usr/bin/rg"), Path::new("/usr/local/bin/rg")],
        current_uid,
    )
    .context("ripgrep runtime executable is invalid")?;

    let mut arguments = [
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/bin",
        "/bin",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib64",
        "/lib64",
        "--dir",
        "/tools",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    for (source, target) in [(&node, "/tools/node"), (&rg, "/tools/rg")] {
        if let Some(source) = source {
            arguments.push("--ro-bind".into());
            arguments.push(source.as_os_str().to_owned());
            arguments.push(target.into());
        }
    }
    Ok((
        arguments,
        RuntimeTools {
            node: node.is_some(),
            rg: rg.is_some(),
        },
    ))
}

fn optional_binary(
    explicit: Option<&Path>,
    candidates: &[&Path],
    current_uid: u32,
) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        return validate_binary(path, current_uid).map(Some);
    }
    for candidate in candidates {
        if candidate
            .try_exists()
            .context("cannot inspect optional system executable")?
        {
            return validate_binary(candidate, current_uid).map(Some);
        }
    }
    Ok(None)
}

#[cfg(target_os = "linux")]
fn current_uid() -> Result<u32> {
    use std::os::unix::fs::MetadataExt;
    // Metadata only: no process environment, descriptors or authentication data.
    Ok(fs::metadata("/proc/self")
        .context("cannot determine runtime owner")?
        .uid())
}

#[cfg(not(target_os = "linux"))]
fn current_uid() -> Result<u32> {
    anyhow::bail!("workspace runtime mounts require Linux")
}

#[cfg(unix)]
fn metadata_trusted(metadata: &fs::Metadata, current_uid: u32, system_only: bool) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    trusted_owner_and_mode(
        metadata.uid(),
        metadata.permissions().mode(),
        current_uid,
        system_only,
    )
}

#[cfg(not(unix))]
fn metadata_trusted(_metadata: &fs::Metadata, _current_uid: u32, _system_only: bool) -> bool {
    false
}

fn trusted_owner_and_mode(owner: u32, mode: u32, current_uid: u32, system_only: bool) -> bool {
    (owner == 0 || (!system_only && owner == current_uid)) && mode & 0o022 == 0
}

fn validate_usr() -> Result<()> {
    // `/usr` is fixed, not supplied by a model, config path or inherited PATH.
    let usr = fs::symlink_metadata("/usr").context("system /usr runtime is unavailable")?;
    let root = fs::symlink_metadata("/").context("system root metadata is unavailable")?;
    ensure!(
        usr.is_dir()
            && !usr.file_type().is_symlink()
            && metadata_trusted(&usr, 0, true)
            && root.is_dir()
            && metadata_trusted(&root, 0, true),
        "system runtime directories must be root-owned and not writable by others"
    );
    for required in ["/usr/bin", "/usr/lib", "/usr/lib64"] {
        let metadata =
            fs::symlink_metadata(required).context("system runtime directory is missing")?;
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata_trusted(&metadata, 0, true),
            "system runtime directories must be real root-owned directories"
        );
    }
    Ok(())
}

fn validate_binary(path: &Path, current_uid: u32) -> Result<PathBuf> {
    ensure!(
        path.is_absolute()
            && path
                .components()
                .all(|part| !matches!(part, Component::ParentDir | Component::CurDir)),
        "runtime executable path must be absolute and normalized"
    );
    let canonical = path
        .canonicalize()
        .context("runtime executable is unavailable")?;
    // Validate the canonical tree as well as the final file. In particular this
    // rejects sources under shared /tmp even if their immediate parent is private.
    for parent in canonical.ancestors().skip(1) {
        let metadata =
            fs::symlink_metadata(parent).context("cannot inspect runtime executable parent")?;
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata_trusted(&metadata, current_uid, false),
            "runtime executable ancestors must be trusted and not writable by others"
        );
    }
    let metadata = fs::symlink_metadata(&canonical).context("cannot inspect runtime executable")?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata_trusted(&metadata, current_uid, false)
            && executable_mode(&metadata),
        "runtime executable must be a trusted, non-writable native file"
    );
    let mut magic = [0; 4];
    fs::File::open(&canonical)
        .and_then(|mut file| file.read_exact(&mut magic))
        .context("cannot inspect native runtime executable header")?;
    ensure!(
        native_header(magic),
        "runtime executable must be a native ELF binary, not a wrapper"
    );
    Ok(canonical)
}

#[cfg(unix)]
fn executable_mode(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable_mode(_metadata: &fs::Metadata) -> bool {
    false
}

fn native_header(magic: [u8; 4]) -> bool {
    magic == *b"\x7fELF"
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn root_and_current_user_are_the_only_trusted_owners() {
        assert!(trusted_owner_and_mode(0, 0o755, 1000, false));
        assert!(trusted_owner_and_mode(1000, 0o700, 1000, false));
        assert!(!trusted_owner_and_mode(1001, 0o755, 1000, false));
        assert!(!trusted_owner_and_mode(1000, 0o755, 1000, true));
    }

    #[test]
    fn group_and_world_writes_are_rejected_even_with_sticky_or_setuid_bits() {
        for mode in [0o775, 0o777, 0o1777, 0o4777, 0o702] {
            assert!(!trusted_owner_and_mode(0, mode, 1000, false));
        }
        assert!(trusted_owner_and_mode(0, 0o4755, 1000, true));
    }

    #[test]
    fn only_elf_headers_are_accepted() {
        assert!(native_header(*b"\x7fELF"));
        assert!(!native_header(*b"#!/b"));
        assert!(!native_header(*b"text"));
    }

    #[test]
    fn missing_optional_candidates_do_not_install_or_fall_back_to_path() {
        let missing = Path::new("/usr/bin/clawmingle-test-tool-does-not-exist");
        assert_eq!(
            optional_binary(None, &[missing], current_uid().unwrap()).unwrap(),
            None
        );
    }

    #[test]
    fn bad_explicit_paths_do_not_fall_back_to_an_existing_system_binary() {
        let missing = Path::new("/usr/bin/clawmingle-test-tool-does-not-exist");
        assert!(
            optional_binary(
                Some(missing),
                &[Path::new("/usr/bin/true")],
                current_uid().unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn relative_and_traversing_executable_paths_are_rejected() {
        for path in ["node", "./node", "/usr/bin/../bin/node"] {
            assert!(validate_binary(Path::new(path), current_uid().unwrap()).is_err());
        }
        assert!(validate_binary(Path::new("/usr/bin"), current_uid().unwrap()).is_err());
    }

    #[test]
    fn validated_plan_uses_fresh_tools_targets_and_read_only_binds() {
        // Read metadata/header only; never execute this stand-in program.
        let stand_in = Path::new("/usr/bin/true");
        let (args, tools) =
            runtime_mount_arguments(Some(stand_in), Some(stand_in), current_uid().unwrap())
                .unwrap();
        assert_eq!(
            tools,
            RuntimeTools {
                node: true,
                rg: true
            }
        );
        assert_eq!(
            &args[..3],
            &[OsString::from("--ro-bind"), "/usr".into(), "/usr".into()]
        );
        assert!(
            args.windows(3)
                .any(|window| window == ["--symlink", "usr/bin", "/bin"])
        );
        assert!(
            args.windows(3)
                .any(|window| window == ["--symlink", "usr/lib", "/lib"])
        );
        assert!(
            args.windows(3)
                .any(|window| window == ["--symlink", "usr/lib64", "/lib64"])
        );
        assert!(args.iter().any(|argument| argument == "/tools/node"));
        assert!(args.iter().any(|argument| argument == "/tools/rg"));
        assert!(!args.iter().any(|argument| {
            ["--bind", "/etc", "/home", "/state", "/proc", "/workspace"]
                .contains(&argument.to_str().unwrap_or_default())
        }));
    }

    #[test]
    fn invalid_plan_does_not_mutate_the_callers_command() {
        let mut command = Command::new("fixture-bubblewrap");
        command.arg("existing-argument");
        let plan = append_with_sources(
            &mut command,
            Some(Path::new("relative")),
            None,
            current_uid().unwrap(),
        );
        assert!(plan.is_err());
        assert_eq!(
            command.as_std().get_args().collect::<Vec<_>>(),
            [std::ffi::OsStr::new("existing-argument")]
        );
        assert_eq!(command.as_std().get_envs().count(), 0);
    }
}
