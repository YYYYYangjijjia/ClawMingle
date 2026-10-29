//! Parse explicit skill installation requests from constrained public GitHub URLs.
//! Installation pins a bounded package; it never runs setup commands, loads native Codex
//! skills, or changes a conversation's enabled-skill list. The caller must check
//! the real platform sender's administrator permission before calling `install`.

use std::path::Path;
#[cfg(test)]
use std::{
    io::{Read, Write},
    time::Duration,
};

use anyhow::{Result, ensure};
#[cfg(test)]
use reqwest::Client;
use reqwest::Url;

#[cfg(test)]
const MAX_DOCUMENT_BYTES: usize = 16_384;
#[cfg(test)]
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(test)]
pub const TEXT_ONLY_NOTICE: &str = "已保存文字型 Skill；只使用 SKILL.md 指令，不会安装或执行脚本、依赖、插件，也不会增加工具或文件权限。依赖这些能力的步骤暂不可用。";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallSpec {
    /// Directory links provide a name immediately. A repository-root SKILL.md
    /// gets its name from the fetched frontmatter, after validating that file.
    pub name: Option<String>,
    /// Canonical HTTPS raw.githubusercontent.com URL, without query or fragment.
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledSkill {
    pub name: String,
    pub description: String,
}

/// Recognize a deliberately narrow set of explicit installation requests. This
/// is an intent parser, never an authorization check or a general NL classifier.
/// Quoted, historical, negative, advisory and ambiguous messages remain ordinary
/// chat. An unambiguous request with missing/multiple URLs asks for clarification.
pub fn parse_install_request(text: &str) -> Result<Option<InstallSpec>> {
    let text = text.trim();
    if let Some(arguments) = text.strip_prefix("/skill-install") {
        if !arguments.is_empty() && !arguments.starts_with(char::is_whitespace) {
            return Ok(None);
        }
        let urls = urls_in(arguments);
        ensure!(urls.len() == 1, "skill_install_one_url_required");
        ensure!(
            arguments.trim() == urls[0],
            "skill_install_one_url_required"
        );
        return parse_source(urls[0]).map(Some);
    }
    if !is_install_imperative(text) {
        return Ok(None);
    }
    ensure!(text.len() <= 8192, "skill_install_request_too_large");
    let urls = urls_in(text);
    ensure!(urls.len() == 1, "skill_install_one_url_required");
    parse_source(urls[0]).map(Some)
}

pub fn parse_skill_list_command(text: &str) -> bool {
    text.trim() == "/skills"
}

fn is_install_imperative(text: &str) -> bool {
    if text.starts_with(['>', '\'', '"', '“', '「', '『', '`']) {
        return false;
    }
    // URL paths are not prose: words in an owner/repository/ref must not
    // influence classification of the administrator's installation request.
    let mut prose = text.to_owned();
    for url in urls_in(text) {
        prose = prose.replace(url, " ");
    }
    let lower = prose.to_ascii_lowercase();
    if !lower.contains("skill") {
        return false;
    }
    let mut body = lower.trim();
    if let Some(rest) = body.strip_prefix("codex") {
        body = rest.trim_start_matches([' ', '\t', '，', ',', '：', ':']);
    }
    if body.starts_with(['>', '\'', '"', '“', '「', '『', '`']) {
        return false;
    }
    if [
        "不安装",
        "不要安装",
        "别安装",
        "不想安装",
        "不用安装",
        "无需安装",
        "不需要安装",
        "暂不安装",
        "取消安装",
        "停止安装",
        "是否",
        "要不要",
        "你觉得",
        "我想知道",
        "有人说",
        "他说",
        "她说",
        "为什么",
        "怎么",
        "如何",
        "什么",
        "能不能",
        "可不可以",
        "吗",
        "么",
        "考虑",
        "建议",
        "讨论",
        "告诉",
        "解释",
        "教我",
        "演示",
        "引用",
        "昨天",
        "前天",
        "以前",
        "安装过",
        "安装了",
        "已经安装",
        "should i",
        "should we",
        "do you think",
        "how ",
        "why ",
        "someone said",
        "he said",
        "she said",
        "i installed",
        "i have installed",
        "yesterday",
        "before installing",
        "do not",
        "don't",
        "dont ",
        "not install",
        "never install",
        "without installing",
    ]
    .iter()
    .any(|phrase| body.contains(phrase))
    {
        return false;
    }
    if let Some(position) = body.find("安装") {
        let prefix = &body[..position];
        if prefix.contains(['不', '别']) || body.contains(['?', '？']) {
            return false;
        }
        return [
            "安装",
            "请",
            "帮我",
            "帮你",
            "帮忙",
            "麻烦",
            "给自己",
            "给你自己",
        ]
        .iter()
        .any(|prefix| body.starts_with(prefix));
    }
    // "Can you install ...?" is a direct request rather than an advice question.
    [
        "install ",
        "please install ",
        "can you install ",
        "could you install ",
    ]
    .iter()
    .any(|prefix| body.starts_with(prefix))
}

fn urls_in(text: &str) -> Vec<&str> {
    let lower = text.to_ascii_lowercase();
    let mut urls = Vec::new();
    let mut offset = 0;
    while offset < text.len() {
        let next = ["https://", "http://"]
            .iter()
            .filter_map(|prefix| {
                lower[offset..]
                    .find(prefix)
                    .map(|position| offset + position)
            })
            .min();
        let Some(start) = next else { break };
        let end = text[start..]
            .char_indices()
            .find(|(_, character)| {
                character.is_whitespace()
                    || matches!(
                        character,
                        '<' | '>'
                            | '\''
                            | '"'
                            | '`'
                            | ')'
                            | ']'
                            | '}'
                            | '，'
                            | '。'
                            | '；'
                            | '！'
                            | '？'
                            | '、'
                    )
            })
            .map(|(position, _)| start + position)
            .unwrap_or(text.len());
        // A sentence-final English question mark is prose, not an empty query.
        let url = text[start..end].trim_end_matches('?');
        urls.push(url);
        offset = end;
    }
    urls
}

fn parse_source(source: &str) -> Result<InstallSpec> {
    ensure!(source.len() <= 2048, "skill_source_url_invalid");
    // Reject traversal before URL parsing can normalize dot segments away.
    // This first version accepts ordinary GitHub owner/repo/ref/path segments;
    // encoded paths and slash-containing branch names require an explicit raw link.
    ensure!(
        !source.contains(['%', '\\', '?', '#'])
            && !source
                .chars()
                .any(|character| character.is_control() || character.is_whitespace()),
        "skill_source_url_invalid"
    );
    let url = Url::parse(source).map_err(|_| anyhow::anyhow!("skill_source_url_invalid"))?;
    ensure!(
        url.scheme() == "https"
            && matches!(
                url.host_str(),
                Some("github.com" | "raw.githubusercontent.com")
            )
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "skill_source_url_invalid"
    );
    let original_path = source
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/').map(|(_, path)| path))
        .ok_or_else(|| anyhow::anyhow!("skill_source_url_invalid"))?;
    let path = if url.host_str() == Some("github.com") {
        original_path.trim_end_matches('/')
    } else {
        original_path
    };
    let parts = path.split('/').collect::<Vec<_>>();
    ensure!(
        parts.iter().all(|part| !part.is_empty()
            && !matches!(*part, "." | "..")
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))),
        "skill_source_url_invalid"
    );
    let (owner, repository, reference, files) = if url.host_str() == Some("github.com") {
        ensure!(parts.len() >= 4, "skill_source_url_invalid");
        ensure!(
            matches!(parts[2], "tree" | "blob"),
            "skill_source_url_invalid"
        );
        let mut files = parts[4..].to_vec();
        if parts[2] == "tree" && files.last().copied() != Some("SKILL.md") {
            files.push("SKILL.md");
        }
        (parts[0], parts[1], parts[3], files)
    } else {
        ensure!(parts.len() >= 4, "skill_source_url_invalid");
        (parts[0], parts[1], parts[2], parts[3..].to_vec())
    };
    ensure!(
        files.last().copied() == Some("SKILL.md"),
        "skill_source_url_invalid"
    );
    let name = if files.len() > 1 {
        let name = files[files.len() - 2].to_ascii_lowercase();
        ensure!(valid_name(&name), "skill_name_invalid");
        Some(name)
    } else {
        None
    };
    let source = format!(
        "https://raw.githubusercontent.com/{owner}/{repository}/{reference}/{}",
        files.join("/")
    );
    Ok(InstallSpec { name, source })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
}

/// `directory` is the bot's data_dir/skills, never a native CODEX_HOME skill
/// directory. The caller enables the returned name only after this succeeds.
pub async fn install(spec: &InstallSpec, directory: &Path) -> Result<InstalledSkill> {
    validate_spec(spec)?;
    super::skill_package::install(spec, directory).await
}

fn validate_spec(spec: &InstallSpec) -> Result<()> {
    let canonical = parse_source(&spec.source)?;
    ensure!(canonical == *spec, "skill_install_spec_invalid");
    Ok(())
}

#[cfg(test)]
async fn fetch(client: &Client, url: Url) -> Result<String> {
    tokio::time::timeout(FETCH_TIMEOUT, async {
        let mut response = client
            .get(url)
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
                .is_none_or(|length| length <= MAX_DOCUMENT_BYTES as u64),
            "skill_document_too_large"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("skill_source_fetch_failed"))?
        {
            ensure!(
                bytes.len() + chunk.len() <= MAX_DOCUMENT_BYTES,
                "skill_document_too_large"
            );
            bytes.extend_from_slice(&chunk);
        }
        let markdown =
            String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("skill_document_not_utf8"))?;
        ensure!(!markdown.contains('\0'), "skill_document_invalid");
        Ok(markdown)
    })
    .await
    .map_err(|_| anyhow::anyhow!("skill_source_fetch_timeout"))?
}

#[cfg(test)]
fn finish_install(spec: &InstallSpec, directory: &Path, markdown: &str) -> Result<InstalledSkill> {
    ensure!(
        markdown.len() <= MAX_DOCUMENT_BYTES,
        "skill_document_too_large"
    );
    let (metadata_name, description) = metadata(markdown)?;
    let name = spec.name.clone().unwrap_or(metadata_name);
    ensure!(valid_name(&name), "skill_name_invalid");
    store_new(directory, &name, markdown.as_bytes())?;
    Ok(InstalledSkill { name, description })
}

pub(super) fn metadata(markdown: &str) -> Result<(String, String)> {
    let mut lines = markdown.trim_start_matches('\u{feff}').lines();
    ensure!(lines.next() == Some("---"), "skill_metadata_required");
    let mut frontmatter = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line == "---" {
            closed = true;
            break;
        }
        frontmatter.push(line);
    }
    ensure!(closed, "skill_metadata_required");
    ensure!(
        lines.any(|line| !line.trim().is_empty()),
        "skill_instructions_empty"
    );
    let mut name = None;
    let mut description = None;
    let mut index = 0;
    while index < frontmatter.len() {
        let line = frontmatter[index];
        index += 1;
        if line.starts_with(char::is_whitespace) || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let target = match key.trim() {
            "name" => &mut name,
            "description" => &mut description,
            _ => continue,
        };
        ensure!(target.is_none(), "skill_metadata_duplicate");
        let value = value.trim();
        let value = if matches!(value, ">" | ">-" | ">+" | "|" | "|-" | "|+") {
            let mut block = Vec::new();
            while index < frontmatter.len()
                && (frontmatter[index].starts_with(char::is_whitespace)
                    || frontmatter[index].is_empty())
            {
                block.push(frontmatter[index].trim());
                index += 1;
            }
            block.join(if value.starts_with('>') { " " } else { "\n" })
        } else {
            metadata_scalar(value)?
        };
        ensure!(!value.trim().is_empty(), "skill_metadata_empty");
        *target = Some(value.trim().to_owned());
    }
    let name = name
        .ok_or_else(|| anyhow::anyhow!("skill_metadata_name_required"))?
        .to_ascii_lowercase();
    let description =
        description.ok_or_else(|| anyhow::anyhow!("skill_metadata_description_required"))?;
    ensure!(valid_name(&name), "skill_name_invalid");
    ensure!(description.len() <= 2048, "skill_description_too_large");
    Ok((name, description))
}

fn metadata_scalar(value: &str) -> Result<String> {
    if value.starts_with('#') || matches!(value, "~" | "null" | "Null" | "NULL") {
        return Ok(String::new());
    }
    if value.starts_with('"') {
        return serde_json::from_str::<String>(value)
            .map_err(|_| anyhow::anyhow!("skill_metadata_invalid"));
    }
    if let Some(value) = value.strip_prefix('\'') {
        let value = value
            .strip_suffix('\'')
            .ok_or_else(|| anyhow::anyhow!("skill_metadata_invalid"))?;
        return Ok(value.replace("''", "'"));
    }
    ensure!(
        !value.starts_with(['[', '{', '&', '*']),
        "skill_metadata_invalid"
    );
    Ok(value
        .split_once(" #")
        .map(|(text, _)| text)
        .unwrap_or(value)
        .trim()
        .to_owned())
}

#[cfg(all(test, target_os = "linux"))]
fn check_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| anyhow::anyhow!("skill_store_directory_invalid"))?;
    let owner = std::fs::metadata("/proc/self")
        .map_err(|_| anyhow::anyhow!("skill_store_directory_invalid"))?
        .uid();
    ensure!(
        metadata.is_dir()
            && metadata.uid() == owner
            && metadata.permissions().mode() & 0o777 == 0o700,
        "skill_store_directory_invalid"
    );
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
fn store_new(directory: &Path, name: &str, markdown: &[u8]) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    ensure!(directory.is_absolute(), "skill_store_directory_invalid");
    let parent = directory
        .parent()
        .ok_or_else(|| anyhow::anyhow!("skill_store_directory_invalid"))?;
    check_private_directory(parent)?;
    match std::fs::symlink_metadata(directory) {
        Ok(_) => check_private_directory(directory)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::DirBuilder::new().mode(0o700).create(directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => anyhow::bail!("skill_store_directory_invalid"),
            }
            check_private_directory(directory)?;
        }
        Err(_) => anyhow::bail!("skill_store_directory_invalid"),
    }
    let destination = directory.join(name);
    match std::fs::DirBuilder::new().mode(0o700).create(&destination) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return reuse_identical(&destination, markdown);
        }
        Err(_) => anyhow::bail!("skill_store_write_failed"),
    }
    check_private_directory(&destination)?;
    let temporary = destination.join(".SKILL.md.installing");
    let canonical = destination.join("SKILL.md");
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(0o400000)
            .open(&temporary)?; // Linux O_NOFOLLOW
        file.write_all(markdown)?;
        file.sync_all()?;
        // Atomic publication without replacing an existing file, even if it
        // appeared between filesystem checks. Both links keep the same 0600 inode.
        std::fs::hard_link(&temporary, &canonical)?;
        std::fs::File::open(&destination)?.sync_all()?;
        std::fs::remove_file(&temporary)?;
        std::fs::File::open(&destination)?.sync_all()?;
        std::fs::File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        // Remove only our known staging file and an empty directory. Never
        // recursively clean or remove a successfully published SKILL.md.
        let _ = std::fs::remove_file(&temporary);
        let _ = std::fs::remove_dir(&destination);
        anyhow::bail!("skill_store_write_failed");
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
fn reuse_identical(destination: &Path, markdown: &[u8]) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    check_private_directory(destination).map_err(|_| anyhow::anyhow!("skill_existing_unsafe"))?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0o400000 | 0o4000) // O_NOFOLLOW | O_NONBLOCK rejects symlinks/FIFO waits.
        .open(destination.join("SKILL.md"))
        .map_err(|_| anyhow::anyhow!("skill_existing_unsafe"))?;
    let metadata = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("skill_existing_unsafe"))?;
    let owner = std::fs::metadata("/proc/self")
        .map_err(|_| anyhow::anyhow!("skill_existing_unsafe"))?
        .uid();
    ensure!(
        metadata.is_file()
            && metadata.uid() == owner
            && metadata.permissions().mode() & 0o777 == 0o600
            && metadata.len() <= MAX_DOCUMENT_BYTES as u64,
        "skill_existing_unsafe"
    );
    let mut bytes = Vec::new();
    file.take(MAX_DOCUMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("skill_existing_unsafe"))?;
    ensure!(bytes == markdown, "skill_already_exists");
    // The on-disk skill is already the exact validated document. This is the
    // restart gap between document publication and conversation state/ACK.
    Ok(())
}

#[cfg(all(test, not(target_os = "linux")))]
fn store_new(_directory: &Path, _name: &str, _markdown: &[u8]) -> Result<()> {
    anyhow::bail!("skill_install_requires_linux")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    const SOURCE: &str =
        "https://github.com/example-owner/example-repository/tree/main/skills/Example-Skill";
    const DOCUMENT: &str = "---\nname: example-skill\ndescription: Answer clearly with examples.\n---\n# Instructions\nExplain the answer with one example.\n";

    fn private_tempdir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn spec() -> InstallSpec {
        parse_source(SOURCE).unwrap()
    }

    /// A local transport override exists only in test builds. Production
    /// `install` always validates and fetches its canonical public HTTPS URL.
    async fn install_from_test_endpoint(
        spec: &InstallSpec,
        directory: &Path,
        endpoint: &str,
    ) -> Result<InstalledSkill> {
        validate_spec(spec)?;
        let endpoint = Url::parse(endpoint).unwrap();
        assert_eq!(endpoint.scheme(), "http");
        assert!(matches!(
            endpoint.host_str(),
            Some("127.0.0.1" | "localhost" | "::1")
        ));
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(FETCH_TIMEOUT)
            .build()
            .unwrap();
        let document = fetch(&client, endpoint).await?;
        finish_install(spec, directory, &document)
    }

    #[test]
    fn explicit_and_clear_imperatives_resolve_to_one_canonical_source() {
        for request in [
            format!("/skill-install {SOURCE}"),
            format!("Codex请给你自己安装这个skill {SOURCE}"),
            format!("Codex帮你安装这个skill {SOURCE}"),
            format!("帮我安装skill {SOURCE}"),
            format!("安装这个skill {SOURCE}"),
            format!("Please install this skill {SOURCE}"),
            format!("Can you install this skill {SOURCE}?"),
        ] {
            let result = parse_install_request(&request).unwrap().unwrap();
            assert_eq!(result.name.as_deref(), Some("example-skill"));
            assert_eq!(
                result.source,
                "https://raw.githubusercontent.com/example-owner/example-repository/main/skills/Example-Skill/SKILL.md"
            );
        }
        assert!(!parse_skill_list_command("/skills extra"));
        assert!(parse_skill_list_command(" /skills\n"));
        assert!(
            parse_install_request("/skill-installing something")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn negatives_history_questions_discussion_and_quotes_never_install() {
        for prose in [
            "Codex请不要安装这个skill",
            "不要安装这个skill",
            "Codex别安装这个skill",
            "请帮我不要安装这个skill",
            "请安装这个skill，算了别安装",
            "我昨天安装过这个skill",
            "Codex我安装过这个skill",
            "你觉得我是否应该安装这个skill",
            "Codex能不能安装这个skill",
            "我想知道为什么安装这个skill",
            "有人说安装这个skill",
            "请告诉我怎么安装skill",
            "请考虑安装这个skill",
            "请解释安装skill需要什么",
            "安装这个skill合适吗",
            "安装skill之前怎么配置",
            "> 安装这个skill",
            "“安装这个skill",
            "Codex ‘安装这个skill",
            "Should I install this skill",
            "Can you explain how to install this skill",
            "I installed this skill yesterday",
            "Please do not install this skill",
            "Please install this skill but don't execute it",
        ] {
            assert!(
                parse_install_request(&format!("{prose} {SOURCE}"))
                    .unwrap()
                    .is_none(),
                "misclassified prose fixture"
            );
        }
    }

    #[test]
    fn missing_multiple_or_extra_explicit_arguments_require_clarification() {
        for request in [
            "/skill-install".to_owned(),
            "安装这个skill".to_owned(),
            format!("/skill-install {SOURCE} {SOURCE}"),
            format!(
                "帮我安装skill {SOURCE} https://github.com/example-owner/another/tree/main/foo"
            ),
            format!("/skill-install {SOURCE} extra"),
        ] {
            assert_eq!(
                parse_install_request(&request).unwrap_err().to_string(),
                "skill_install_one_url_required"
            );
        }
    }

    #[test]
    fn blob_tree_and_root_raw_sources_are_supported_without_guessing_a_ref() {
        let file = parse_source(
            "https://github.com/example-owner/example-repository/blob/main/skills/foo/SKILL.md",
        )
        .unwrap();
        assert_eq!(file.name.as_deref(), Some("foo"));
        let root =
            parse_source("https://github.com/example-owner/example-repository/tree/main").unwrap();
        assert!(root.name.is_none());
        assert_eq!(
            root.source,
            "https://raw.githubusercontent.com/example-owner/example-repository/main/SKILL.md"
        );
        let raw = parse_source(
            "https://raw.githubusercontent.com/example-owner/example-repository/main/SKILL.md",
        )
        .unwrap();
        assert_eq!(raw, root);
    }

    #[test]
    fn arbitrary_hosts_credentials_query_fragments_and_traversal_are_rejected() {
        for source in [
            "https://localhost/owner/repo/main/SKILL.md",
            "https://127.0.0.1/owner/repo/main/SKILL.md",
            "https://github.com.evil.invalid/owner/repo/tree/main/foo",
            "http://github.com/owner/repo/tree/main/foo",
            "https://user@github.com/owner/repo/tree/main/foo",
            "https://github.com:444/owner/repo/tree/main/foo",
            "https://github.com/owner/repo/tree/main/foo?token=EXAMPLE_TOKEN",
            "https://github.com/owner/repo/tree/main/foo#fragment",
            "https://github.com/owner/repo/tree/main/foo/../bar",
            "https://github.com/owner/repo/tree/main/%2e%2e/bar",
            "https://raw.githubusercontent.com/owner/repo/main/foo/../SKILL.md",
            "https://raw.githubusercontent.com/owner/repo/main/README.md",
            "https://github.com/owner/repo/blob/main/skills/foo/skill.md",
            "https://github.com/owner/repo/tree/main/.hidden",
        ] {
            assert!(parse_source(source).is_err(), "unapproved source fixture");
        }
    }

    #[test]
    fn metadata_requires_name_description_and_instructions() {
        for document in [
            "# Instructions only",
            "---\n---\nInstructions",
            "---\nname: foo\n---\nInstructions",
            "---\nname: foo\ndescription: # nothing\n---\nInstructions",
            "---\nname: foo\ndescription: null\n---\nInstructions",
            "---\nname: foo\ndescription: foo\n---\n  \n",
            "---\nname: ../../escape\ndescription: foo\n---\nInstructions",
            "---\nname: foo\nname: bar\ndescription: foo\n---\nInstructions",
        ] {
            assert!(metadata(document).is_err());
        }
        let (name,description) = metadata("---\nname: 'Folded-Skill'\ndescription: >-\n  Explain a question\n  with one example.\nmetadata:\n  author: ignored\n---\nInstructions\n").unwrap();
        assert_eq!(name, "folded-skill");
        assert_eq!(description, "Explain a question with one example.");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn original_markdown_is_private_text_only_and_same_name_is_never_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let root = private_tempdir();
        let directory = root.path().join("skills");
        let installed = finish_install(&spec(), &directory, DOCUMENT).unwrap();
        assert_eq!(installed.name, "example-skill");
        let file = directory.join("example-skill/SKILL.md");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), DOCUMENT);
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(file.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_dir(file.parent().unwrap()).unwrap().count(),
            1
        );
        let changed = DOCUMENT.replace("one example", "a changed answer");
        assert_eq!(
            finish_install(&spec(), &directory, &changed)
                .unwrap_err()
                .to_string(),
            "skill_already_exists"
        );
        assert_eq!(std::fs::read_to_string(file).unwrap(), DOCUMENT);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identical_safe_install_is_idempotent_without_rewriting_the_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = private_tempdir();
        let directory = root.path().join("skills");
        let first = finish_install(&spec(), &directory, DOCUMENT).unwrap();
        let path = directory.join("example-skill/SKILL.md");
        let before = std::fs::metadata(&path).unwrap();
        assert_eq!(
            finish_install(&spec(), &directory, DOCUMENT).unwrap(),
            first
        );
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(before.ino(), after.ino());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            finish_install(&spec(), &directory, DOCUMENT)
                .unwrap_err()
                .to_string(),
            "skill_existing_unsafe"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn root_skill_uses_metadata_name_and_never_executes_referenced_scripts() {
        let root = private_tempdir();
        let directory = root.path().join("skills");
        let source = parse_source(
            "https://raw.githubusercontent.com/example-owner/example-repository/main/SKILL.md",
        )
        .unwrap();
        let document = "---\nname: root-skill\ndescription: Example text-only skill.\n---\nUse scripts/setup.sh and pip install when available.\n";
        assert_eq!(
            finish_install(&source, &directory, document).unwrap().name,
            "root-skill"
        );
        let path = directory.join("root-skill");
        assert_eq!(std::fs::read_dir(path).unwrap().count(), 1);
        assert!(TEXT_ONLY_NOTICE.contains("不会安装或执行脚本"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn symlinks_and_loose_directories_are_rejected_without_touching_targets() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = private_tempdir();
        let outside = private_tempdir();
        let directory = root.path().join("skills");
        symlink(outside.path(), &directory).unwrap();
        assert!(finish_install(&spec(), &directory, DOCUMENT).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        std::fs::remove_file(&directory).unwrap();
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(finish_install(&spec(), &directory, DOCUMENT).is_err());
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(outside.path(), directory.join("example-skill")).unwrap();
        assert_eq!(
            finish_install(&spec(), &directory, DOCUMENT)
                .unwrap_err()
                .to_string(),
            "skill_existing_unsafe"
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn local_mock_download_installs_only_the_canonical_file() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/SKILL.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DOCUMENT))
            .expect(1)
            .mount(&server)
            .await;
        let root = private_tempdir();
        let installed = install_from_test_endpoint(
            &spec(),
            &root.path().join("skills"),
            &format!("{}/SKILL.md", server.uri()),
        )
        .await
        .unwrap();
        assert_eq!(installed.name, "example-skill");
    }

    #[tokio::test]
    async fn oversized_and_chunked_downloads_are_rejected_before_creating_a_skill() {
        for chunked in [false, true] {
            let server = MockServer::start().await;
            let response =
                ResponseTemplate::new(200).set_body_string("x".repeat(MAX_DOCUMENT_BYTES + 1));
            let response = if chunked {
                response.insert_header("transfer-encoding", "chunked")
            } else {
                response
            };
            Mock::given(method("GET"))
                .and(path("/SKILL.md"))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;
            let root = private_tempdir();
            let directory = root.path().join("skills");
            let error = install_from_test_endpoint(
                &spec(),
                &directory,
                &format!("{}/SKILL.md", server.uri()),
            )
            .await
            .unwrap_err();
            assert_eq!(error.to_string(), "skill_document_too_large");
            assert!(!directory.exists());
        }
    }

    #[tokio::test]
    async fn redirects_never_fetch_a_second_address_and_http_errors_never_echo_bodies() {
        for status in [302, 404] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/SKILL.md"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("Location", format!("{}/unexpected", server.uri()))
                        .set_body_string("private-marker"),
                )
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/unexpected"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;
            let root = private_tempdir();
            let error = install_from_test_endpoint(
                &spec(),
                &root.path().join("skills"),
                &format!("{}/SKILL.md", server.uri()),
            )
            .await
            .unwrap_err();
            assert!(!error.to_string().contains("private-marker"));
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn invalid_public_spec_is_rejected_before_network_or_store_changes() {
        let root = private_tempdir();
        let directory = root.path().join("skills");
        let invalid = InstallSpec {
            name: Some("example-skill".into()),
            source: "https://localhost/owner/repo/main/SKILL.md".into(),
        };
        assert_eq!(
            install(&invalid, &directory).await.unwrap_err().to_string(),
            "skill_source_url_invalid"
        );
        assert!(!directory.exists());
    }
}
