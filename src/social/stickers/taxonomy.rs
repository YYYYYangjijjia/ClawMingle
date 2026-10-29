//! Small validated vocabulary shared by operator imports and model metadata tools.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const DEFAULT_SERIES: &str = "default";
pub const STYLE_OPTIONS: &[&str] = &[
    "cute", "goofy", "meme", "ordinary", "animal", "person", "cartoon", "text", "teasing",
];
pub const EMOTION_OPTIONS: &[&str] = &[
    "happy",
    "sad",
    "speechless",
    "amused",
    "calm",
    "angry",
    "surprised",
    "confused",
    "affectionate",
    "encouraging",
    "tired",
    "embarrassed",
    "celebratory",
    "neutral",
];

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct StickerClassification {
    pub summary: String,
    pub styles: Vec<String>,
    pub emotions: Vec<String>,
}
impl StickerClassification {
    pub fn validate(&self, ready: bool) -> Result<()> {
        ensure!(
            self.summary.chars().count() <= 300
                && !self.summary.contains(['\0', '\n', '\r'])
                && (!ready || !self.summary.trim().is_empty())
                && self.styles.len() <= STYLE_OPTIONS.len()
                && self.emotions.len() <= 6
                && self
                    .styles
                    .iter()
                    .all(|v| STYLE_OPTIONS.contains(&v.as_str()))
                && self
                    .emotions
                    .iter()
                    .all(|v| EMOTION_OPTIONS.contains(&v.as_str()))
                && self.styles.iter().collect::<BTreeSet<_>>().len() == self.styles.len()
                && self.emotions.iter().collect::<BTreeSet<_>>().len() == self.emotions.len(),
            "sticker_classification_invalid"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetadataStatus {
    #[default]
    Ready,
    Pending,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssetType {
    #[default]
    Static,
    Animated,
}
impl AssetType {
    pub(super) fn from_animation(animated: bool) -> Self {
        if animated {
            Self::Animated
        } else {
            Self::Static
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StickerSeries {
    pub id: String,
    pub name: String,
    pub created_at: i64,
}
impl StickerSeries {
    pub(super) fn default_series() -> Self {
        Self {
            id: DEFAULT_SERIES.into(),
            name: "默认收藏".into(),
            created_at: 0,
        }
    }
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            valid_series_id(&self.id)
                && !self.name.trim().is_empty()
                && self.name.chars().count() <= 60
                && !self.name.contains(['\0', '\n', '\r'])
                && self.created_at >= 0,
            "sticker_series_invalid"
        );
        Ok(())
    }
}
pub fn valid_series_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}
pub(super) fn default_series_id() -> String {
    DEFAULT_SERIES.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn teasing_style_and_embarrassed_emotion_are_distinct_valid_labels() {
        let mut labels = StickerClassification {
            summary: "尴尬地翻白眼".into(),
            styles: vec!["teasing".into()],
            emotions: vec!["embarrassed".into()],
        };
        labels.validate(true).unwrap();
        labels.emotions = vec!["teasing".into()];
        assert!(labels.validate(true).is_err());
    }
}
