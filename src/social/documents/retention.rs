//! Lifecycle labels only. No migration, recycling, capacity sweep or scheduler.
use super::*;

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RetentionClass {
    #[default]
    Standard,
    KeepOnline,
    Memory,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Policy {
    class: RetentionClass,
    updated_at: Option<String>,
}

impl Broker {
    // The caller must validate fresh remote membership before exposing or
    // changing these labels. A local policy file never grants remote access.
    pub(super) fn retention(&self, file: &str, update: Option<RetentionClass>) -> Result<Value> {
        let path = self.root.join(format!("retention-{}.json", hash(file)));
        let policy = match update {
            Some(class) => {
                let policy = Policy {
                    class,
                    updated_at: Some(chrono::Utc::now().to_rfc3339()),
                };
                atomic_write(&path, &serde_json::to_string(&policy)?)?;
                policy
            }
            None if path.exists() => serde_json::from_str(&private_read_limit(&path, 4096)?)?,
            None => Policy::default(),
        };
        Ok(json!({
            "file_id":file,"class":policy.class,"updated_at":policy.updated_at,
            "excluded_from_automatic_archive":!matches!(policy.class, RetentionClass::Standard),
            "automatic_archive_enabled":false,"scope":"local_lifecycle_policy",
            "message":"策略只标记归档豁免；目前没有自动迁移或删除。记忆文档仍不替代本地正式记忆。"
        }))
    }
}
