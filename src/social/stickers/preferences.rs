//! Conversation-local preferences derived from confirmed human sticker events.
//!
//! Scores are never written back: the bounded observations and explicit rules
//! remain the source of truth. Callers must filter candidates for the current
//! context before sampling; liking a sticker does not make it appropriate.
use anyhow::{Result, ensure};
use fs2::FileExt;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const DAY: i64 = 86_400;
const MAX_EVENTS: usize = 2_000;
const MAX_TARGETS: usize = 8_192;
const MAX_RULES: usize = 1_024;
const MAX_RULE_EVENTS: usize = 2_000;
const MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PreferenceSettings {
    pub window_days: u32,
    pub rolling_events: usize,
    pub retention_days: u32,
    pub max_events: usize,
    pub wall_decay_days: u32,
    pub explicit_half_life_days: u32,
    pub explicit_active_half_life_days: u32,
    pub exploration_fraction: f64,
}
impl Default for PreferenceSettings {
    fn default() -> Self {
        Self {
            window_days: 14,
            rolling_events: 100,
            retention_days: 90,
            max_events: MAX_EVENTS,
            wall_decay_days: 180,
            explicit_half_life_days: 60,
            explicit_active_half_life_days: 20,
            exploration_fraction: 0.15,
        }
    }
}
impl PreferenceSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=90).contains(&self.window_days)
                && (1..=200).contains(&self.rolling_events)
                && (self.window_days..=90).contains(&self.retention_days)
                && (self.rolling_events..=MAX_EVENTS).contains(&self.max_events)
                && (30..=730).contains(&self.wall_decay_days)
                && (7..=365).contains(&self.explicit_half_life_days)
                && (3..=100).contains(&self.explicit_active_half_life_days)
                && (0.05..=0.5).contains(&self.exploration_fraction),
            "sticker_preference_settings_invalid"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ItemFeatures {
    pub id: String,
    pub series_id: String,
    pub styles: Vec<String>,
}
impl ItemFeatures {
    fn validate(&self) -> Result<()> {
        PreferenceTarget::asset(&self.id).validate()?;
        PreferenceTarget::series(&self.series_id).validate()?;
        ensure!(self.styles.len() <= 12, "sticker_preference_styles_invalid");
        let mut unique = BTreeSet::new();
        for style in &self.styles {
            PreferenceTarget::style(style).validate()?;
            ensure!(unique.insert(style), "sticker_preference_style_duplicate");
        }
        Ok(())
    }
    fn targets(&self) -> Vec<PreferenceTarget> {
        let mut targets = vec![
            PreferenceTarget::asset(&self.id),
            PreferenceTarget::series(&self.series_id),
        ];
        targets.extend(
            self.styles
                .iter()
                .map(|style| PreferenceTarget::style(style)),
        );
        targets
    }
    fn matches(&self, target: &PreferenceTarget) -> bool {
        match target.dimension {
            PreferenceDimension::Asset => self.id == target.value,
            PreferenceDimension::Series => self.series_id == target.value,
            PreferenceDimension::Style => self.styles.contains(&target.value),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreferenceDimension {
    Asset,
    Series,
    Style,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreferenceTarget {
    pub dimension: PreferenceDimension,
    pub value: String,
}
impl PreferenceTarget {
    pub fn asset(value: &str) -> Self {
        Self {
            dimension: PreferenceDimension::Asset,
            value: value.into(),
        }
    }
    pub fn series(value: &str) -> Self {
        Self {
            dimension: PreferenceDimension::Series,
            value: value.into(),
        }
    }
    pub fn style(value: &str) -> Self {
        Self {
            dimension: PreferenceDimension::Style,
            value: value.into(),
        }
    }
    fn key(&self) -> String {
        let dimension = match self.dimension {
            PreferenceDimension::Asset => "asset",
            PreferenceDimension::Series => "series",
            PreferenceDimension::Style => "style",
        };
        format!("{dimension}:{}", self.value)
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            valid_text(&self.value, 80),
            "sticker_preference_target_invalid"
        );
        if self.dimension == PreferenceDimension::Asset {
            ensure!(
                matches!(self.value.len(), 40 | 64)
                    && self
                        .value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "sticker_preference_asset_invalid"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationSource {
    Human,
    #[allow(dead_code)] // Explicitly rejected at the API boundary; exercises caller mistakes.
    Bot,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    Prefer,
    /// Explicit "never use" is an exclusion, not a score that silently recovers.
    Avoid,
    Reset,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplicitRule {
    pub target: PreferenceTarget,
    pub action: RuleAction,
    pub source_event_id: String,
    pub at: i64,
    /// Raw active-day opportunities since this instruction, not a cached level.
    /// Keeping this tiny counter prevents preference resurrection when the
    /// bounded event window rolls over. Late older days do not add opportunities.
    #[serde(default)]
    pub active_days_since_rule: u32,
    #[serde(default)]
    pub last_active_day: Option<i64>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    event_id: String,
    at: i64,
    item: ItemFeatures,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ScopeState {
    schema: u32,
    scope: String,
    events: Vec<Observation>,
    items: BTreeMap<String, ItemFeatures>,
    // Preserve the last observation timestamp after rolling evidence expires.
    last_uses: BTreeMap<String, i64>,
    rules: BTreeMap<String, ExplicitRule>,
    rule_events: Vec<ExplicitRule>,
}
impl ScopeState {
    fn reindex(&mut self, item: &ItemFeatures) -> Result<()> {
        ensure!(
            self.items.contains_key(&item.id) || self.items.len() < MAX_EVENTS,
            "sticker_preference_item_limit"
        );
        self.items.insert(item.id.clone(), item.clone());
        for event in &mut self.events {
            if event.item.id == item.id {
                event.item = item.clone();
            }
        }
        let asset_uses: BTreeMap<_, _> = self
            .last_uses
            .iter()
            .filter(|(key, _)| key.starts_with("asset:"))
            .map(|(key, at)| (key.clone(), *at))
            .collect();
        self.last_uses = asset_uses.clone();
        for known in self.items.values() {
            if let Some(at) = asset_uses.get(&PreferenceTarget::asset(&known.id).key()) {
                for target in known.targets() {
                    self.last_uses
                        .entry(target.key())
                        .and_modify(|last| *last = (*last).max(*at))
                        .or_insert(*at);
                }
            }
        }
        ensure!(
            self.last_uses.len() <= MAX_TARGETS,
            "sticker_preference_target_limit"
        );
        Ok(())
    }
    fn prune(&mut self, now: i64, settings: &PreferenceSettings) {
        let oldest = now.saturating_sub(i64::from(settings.retention_days) * DAY);
        self.events.retain(|event| event.at >= oldest);
        self.events
            .sort_by(|a, b| (a.at, &a.event_id, &a.item.id).cmp(&(b.at, &b.event_id, &b.item.id)));
        let excess = self.events.len().saturating_sub(settings.max_events);
        self.events.drain(..excess);
        let excess = self.rule_events.len().saturating_sub(MAX_RULE_EVENTS);
        self.rule_events.drain(..excess);
    }
    fn validate(&self, scope: &str) -> Result<()> {
        ensure!(
            self.schema == 1
                && self.scope == scope
                && self.events.len() <= MAX_EVENTS
                && self.items.len() <= MAX_EVENTS
                && self.last_uses.len() <= MAX_TARGETS
                && self.rules.len() <= MAX_RULES
                && self.rule_events.len() <= MAX_RULE_EVENTS,
            "sticker_preferences_invalid"
        );
        for event in &self.events {
            ensure!(
                valid_text(&event.event_id, 256) && event.at >= 0,
                "sticker_observation_invalid"
            );
            event.item.validate()?;
        }
        for (id, item) in &self.items {
            ensure!(id == &item.id, "sticker_preference_item_invalid");
            item.validate()?;
        }
        for (key, rule) in &self.rules {
            ensure!(
                key == &rule.target.key() && rule.action != RuleAction::Reset,
                "sticker_preference_rule_invalid"
            );
        }
        for rule in self.rules.values().chain(self.rule_events.iter()) {
            rule.target.validate()?;
            ensure!(
                valid_text(&rule.source_event_id, 256)
                    && rule.at >= 0
                    && rule.last_active_day.is_none_or(|day| day >= 0),
                "sticker_preference_rule_invalid"
            );
        }
        ensure!(
            self.last_uses.values().all(|at| *at >= 0),
            "sticker_preference_last_use_invalid"
        );
        Ok(())
    }
}

fn valid_text(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

#[derive(Clone, Debug, Serialize)]
pub struct DimensionScore {
    pub target: PreferenceTarget,
    pub last_used_at: Option<i64>,
    /// A: raw human uses in the configured recent time window.
    pub window_count: usize,
    /// B: raw human uses among the last configured N sticker events.
    pub rolling_count: usize,
    pub window_active_days: usize,
    pub rolling_active_days: usize,
    pub level: f64,
    pub excluded: bool,
    pub explicit_rule: Option<ExplicitRule>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ItemScore {
    pub id: String,
    pub level: f64,
    /// First-draw probability among this exact candidate set. Subsequent draws
    /// in a multi-item sample renormalize over the remaining candidates.
    pub probability: f64,
    pub excluded: bool,
    pub asset: DimensionScore,
    pub series: DimensionScore,
    pub styles: Vec<DimensionScore>,
    pub scope_window_events: usize,
    pub scope_rolling_events: usize,
    pub scope_window_active_days: usize,
    pub scope_rolling_active_days: usize,
}

pub struct PreferenceStore {
    root: PathBuf,
}
impl PreferenceStore {
    pub fn new(data_dir: &Path) -> Result<Self> {
        let root = super::StickerStore::new(data_dir)?.root.join("preferences");
        super::super::store::private_directory(&root)?;
        Ok(Self { root })
    }
    fn lock(&self) -> Result<(File, File)> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(super::NOFOLLOW | super::DIRECTORY)
            .open(&self.root)?;
        let metadata = directory.metadata()?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == std::fs::metadata("/proc/self")?.uid()
                && metadata.mode() & 0o777 == 0o700,
            "sticker_preference_directory_invalid"
        );
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(super::NOFOLLOW | super::NONBLOCK)
            .open(super::child(&directory, "preferences.lock"))?;
        super::checked(&lock, 1024)?;
        lock.lock_exclusive()?;
        Ok((directory, lock))
    }
    fn access<T>(
        &self,
        scope: &str,
        write: bool,
        action: impl FnOnce(&mut ScopeState) -> Result<T>,
    ) -> Result<T> {
        ensure!(valid_text(scope, 160), "sticker_preference_scope_invalid");
        let (directory, _lock) = self.lock()?;
        let filename = format!("{}.json", super::digest(scope.as_bytes()));
        let raw = match super::read_file(&directory, &filename, MAX_BYTES) {
            Ok(raw) => Some(raw),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        let mut state: ScopeState = match &raw {
            Some(raw) => serde_json::from_slice(raw)
                .map_err(|_| anyhow::anyhow!("sticker_preferences_invalid"))?,
            None => ScopeState {
                schema: 1,
                scope: scope.into(),
                ..Default::default()
            },
        };
        state.validate(scope)?;
        let result = action(&mut state)?;
        if write {
            state.validate(scope)?;
            let serialized = serde_json::to_string(&state)?;
            ensure!(
                serialized.len() as u64 <= MAX_BYTES,
                "sticker_preferences_full"
            );
            if raw.as_deref() != Some(serialized.as_bytes()) {
                crate::util::fs::atomic_write(&super::child(&directory, &filename), &serialized)?;
                directory.sync_all()?;
            }
        }
        Ok(result)
    }
    /// Confirmed incoming human stickers only. A bot echo cannot train its own
    /// preferences. Several distinct stickers in one message remain distinct
    /// events; receiving the same image in the same message twice is idempotent.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        scope: &str,
        event_id: &str,
        item: &ItemFeatures,
        occurred_at: i64,
        now: i64,
        source: ObservationSource,
        settings: &PreferenceSettings,
    ) -> Result<bool> {
        settings.validate()?;
        if source != ObservationSource::Human {
            return Ok(false);
        }
        ensure!(
            valid_text(event_id, 256) && occurred_at >= 0 && now >= occurred_at,
            "sticker_observation_invalid"
        );
        item.validate()?;
        self.access(scope, true, |state| {
            state.prune(now, settings);
            if occurred_at < now.saturating_sub(i64::from(settings.retention_days) * DAY)
                || state
                    .events
                    .iter()
                    .any(|event| event.event_id == event_id && event.item.id == item.id)
            {
                return Ok(false);
            }
            state
                .last_uses
                .entry(PreferenceTarget::asset(&item.id).key())
                .and_modify(|at| *at = (*at).max(occurred_at))
                .or_insert(occurred_at);
            state.reindex(item)?;
            let day = occurred_at.saturating_add(8 * 3600) / DAY;
            for rule in state
                .rules
                .values_mut()
                .filter(|rule| rule.action == RuleAction::Prefer)
            {
                if occurred_at > rule.at && rule.last_active_day.is_none_or(|last| day > last) {
                    rule.active_days_since_rule = rule.active_days_since_rule.saturating_add(1);
                    rule.last_active_day = Some(day);
                }
            }
            state.events.push(Observation {
                event_id: event_id.into(),
                at: occurred_at,
                item: item.clone(),
            });
            state.prune(now, settings);
            Ok(true)
        })
    }
    /// Reclassify existing observations after description/taxonomy completion
    /// or a catalogue series move. No new use, timestamp or score is created.
    /// Files are updated atomically under the preferences lock; retrying after
    /// a partial I/O failure safely completes any remaining conversation files.
    pub fn update_item(&self, item: &ItemFeatures) -> Result<usize> {
        item.validate()?;
        let (directory, _lock) = self.lock()?;
        let mut changed = 0;
        for entry in std::fs::read_dir(super::child(&directory, "."))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(filename) = name.to_str() else {
                continue;
            };
            let Some(id) = filename.strip_suffix(".json") else {
                continue;
            };
            if !super::valid_id(id) {
                continue;
            }
            let raw = super::read_file(&directory, filename, MAX_BYTES)?;
            let mut state: ScopeState = serde_json::from_slice(&raw)
                .map_err(|_| anyhow::anyhow!("sticker_preferences_invalid"))?;
            state.validate(&state.scope)?;
            ensure!(
                id == super::digest(state.scope.as_bytes()),
                "sticker_preference_scope_invalid"
            );
            if state.items.get(&item.id).is_none_or(|old| old == item) {
                continue;
            }
            state.reindex(item)?;
            state.validate(&state.scope)?;
            let serialized = serde_json::to_string(&state)?;
            ensure!(
                serialized.len() as u64 <= MAX_BYTES,
                "sticker_preferences_full"
            );
            crate::util::fs::atomic_write(&super::child(&directory, filename), &serialized)?;
            directory.sync_all()?;
            changed += 1;
        }
        Ok(changed)
    }
    /// Caller verifies that the current authenticated user's message authorizes
    /// this change in this conversation. Model text and quotes confer no rights.
    pub fn set_rule(
        &self,
        scope: &str,
        target: PreferenceTarget,
        action: RuleAction,
        source_event_id: &str,
        now: i64,
    ) -> Result<bool> {
        target.validate()?;
        ensure!(
            valid_text(source_event_id, 256) && now >= 0,
            "sticker_preference_rule_invalid"
        );
        self.access(scope, true, |state| {
            if let Some(prior) = state
                .rule_events
                .iter()
                .find(|rule| rule.target == target && rule.source_event_id == source_event_id)
            {
                ensure!(
                    prior.action == action,
                    "sticker_preference_rule_replay_conflict"
                );
                return Ok(false);
            }
            let key = target.key();
            let rule = ExplicitRule {
                target,
                action,
                source_event_id: source_event_id.into(),
                at: now,
                active_days_since_rule: 0,
                last_active_day: None,
            };
            if action == RuleAction::Reset {
                state.rules.remove(&key);
            } else {
                ensure!(
                    state.rules.contains_key(&key) || state.rules.len() < MAX_RULES,
                    "sticker_preference_rule_limit"
                );
                state.rules.insert(key, rule.clone());
            }
            state.rule_events.push(rule);
            let excess = state.rule_events.len().saturating_sub(MAX_RULE_EVENTS);
            state.rule_events.drain(..excess);
            Ok(true)
        })
    }
    pub fn maintain(&self, scope: &str, now: i64, settings: &PreferenceSettings) -> Result<()> {
        settings.validate()?;
        ensure!(now >= 0, "sticker_preference_time_invalid");
        self.access(scope, true, |state| {
            state.prune(now, settings);
            Ok(())
        })
    }
    pub fn scores(
        &self,
        scope: &str,
        candidates: &[ItemFeatures],
        now: i64,
        settings: &PreferenceSettings,
    ) -> Result<Vec<ItemScore>> {
        settings.validate()?;
        ensure!(
            now >= 0 && candidates.len() <= 1024,
            "sticker_preference_candidates_invalid"
        );
        let mut ids = BTreeSet::new();
        for item in candidates {
            item.validate()?;
            ensure!(
                ids.insert(&item.id),
                "sticker_preference_candidate_duplicate"
            );
        }
        self.access(scope, false, |state| {
            state.prune(now, settings);
            Ok(score_items(state, candidates, now, settings))
        })
    }
    /// Weighted sampling without replacement. Returns a recommendation, never
    /// substitutes preference for the caller's semantic/context filtering.
    pub fn sample(
        &self,
        scope: &str,
        candidates: &[ItemFeatures],
        limit: usize,
        now: i64,
        settings: &PreferenceSettings,
    ) -> Result<Vec<ItemScore>> {
        ensure!(
            (1..=12).contains(&limit),
            "sticker_preference_sample_limit_invalid"
        );
        let scores = self.scores(scope, candidates, now, settings)?;
        Ok(sample_scores(scores, limit, &mut rand::thread_rng()))
    }
}

fn active_days(events: &[&Observation]) -> BTreeSet<i64> {
    // Product reporting and the nightly maintenance use Beijing calendar days.
    events
        .iter()
        .map(|event| event.at.saturating_add(8 * 3600) / DAY)
        .collect()
}
fn evidence_signal(matched: usize, total: usize) -> f64 {
    // Six neutral pseudodays: one afternoon's burst is weak evidence. A day
    // with other stickers but none of this target is a modest negative signal.
    (2.0 * matched as f64 - 0.5 * (total - matched) as f64) / (6.0 + total as f64)
}
fn dimension_score(
    state: &ScopeState,
    target: PreferenceTarget,
    recent: &[&Observation],
    rolling: &[&Observation],
    now: i64,
    settings: &PreferenceSettings,
) -> DimensionScore {
    let matches_recent: Vec<_> = recent
        .iter()
        .copied()
        .filter(|event| event.item.matches(&target))
        .collect();
    let matches_rolling: Vec<_> = rolling
        .iter()
        .copied()
        .filter(|event| event.item.matches(&target))
        .collect();
    let recent_days = active_days(recent).len();
    let rolling_days = active_days(rolling).len();
    let matched_recent_days = active_days(&matches_recent).len();
    let matched_rolling_days = active_days(&matches_rolling).len();
    let key = target.key();
    let last_used_at = state.last_uses.get(&key).copied();
    let rolling_signal = evidence_signal(matched_rolling_days, rolling_days);
    let recent_signal = if recent_days == 0 {
        rolling_signal
    } else {
        evidence_signal(matched_recent_days, recent_days)
    };
    let trend = if recent_days > 0 && rolling_days > 0 {
        0.3 * (matched_recent_days as f64 / recent_days as f64
            - matched_rolling_days as f64 / rolling_days as f64)
            * (recent_days.min(6) as f64 / 6.0)
    } else {
        0.0
    };
    // Conversation inactivity is not evidence against this sticker. Only a
    // slow contraction to neutral applies when there are no new opportunities.
    let idle_days = state.events.last().map_or(0.0, |event| {
        now.saturating_sub(event.at) as f64 / DAY as f64
    });
    let wall_decay = 2.0f64.powf(-idle_days / f64::from(settings.wall_decay_days));
    let mut level = if last_used_at.is_some() {
        3.0 + (0.7 * recent_signal + 0.3 * rolling_signal + trend) * wall_decay
    } else {
        3.0
    };
    let explicit_rule = state.rules.get(&key).cloned();
    let mut excluded = false;
    if let Some(rule) = &explicit_rule {
        match rule.action {
            RuleAction::Avoid => {
                excluded = true;
                level = 0.0;
            }
            RuleAction::Prefer => {
                let elapsed_days = now.saturating_sub(rule.at) as f64 / DAY as f64;
                let opportunity_days = f64::from(rule.active_days_since_rule);
                let decay = 2.0f64.powf(
                    -elapsed_days / f64::from(settings.explicit_half_life_days)
                        - opportunity_days / f64::from(settings.explicit_active_half_life_days),
                );
                level = level.max(3.0 + 2.0 * decay);
            }
            RuleAction::Reset => {}
        }
    }
    DimensionScore {
        target,
        last_used_at,
        window_count: matches_recent.len(),
        rolling_count: matches_rolling.len(),
        window_active_days: matched_recent_days,
        rolling_active_days: matched_rolling_days,
        level: level.clamp(0.0, 5.0),
        excluded,
        explicit_rule,
    }
}
fn score_items(
    state: &ScopeState,
    candidates: &[ItemFeatures],
    now: i64,
    settings: &PreferenceSettings,
) -> Vec<ItemScore> {
    let oldest = now.saturating_sub(i64::from(settings.window_days) * DAY);
    let recent: Vec<_> = state
        .events
        .iter()
        .filter(|event| event.at >= oldest && event.at <= now)
        .collect();
    let rolling: Vec<_> = state
        .events
        .iter()
        .rev()
        .filter(|event| event.at <= now)
        .take(settings.rolling_events)
        .collect();
    let mut result: Vec<_> = candidates
        .iter()
        .map(|item| {
            let asset = dimension_score(
                state,
                PreferenceTarget::asset(&item.id),
                &recent,
                &rolling,
                now,
                settings,
            );
            let series = dimension_score(
                state,
                PreferenceTarget::series(&item.series_id),
                &recent,
                &rolling,
                now,
                settings,
            );
            let styles: Vec<_> = item
                .styles
                .iter()
                .map(|style| {
                    dimension_score(
                        state,
                        PreferenceTarget::style(style),
                        &recent,
                        &rolling,
                        now,
                        settings,
                    )
                })
                .collect();
            let excluded =
                asset.excluded || series.excluded || styles.iter().any(|style| style.excluded);
            let style_level = if styles.is_empty() {
                3.0
            } else {
                styles.iter().map(|style| style.level).sum::<f64>() / styles.len() as f64
            };
            let level = if excluded {
                0.0
            } else {
                0.45 * asset.level + 0.30 * series.level + 0.25 * style_level
            };
            ItemScore {
                id: item.id.clone(),
                level,
                probability: 0.0,
                excluded,
                asset,
                series,
                styles,
                scope_window_events: recent.len(),
                scope_rolling_events: rolling.len(),
                scope_window_active_days: active_days(&recent).len(),
                scope_rolling_active_days: active_days(&rolling).len(),
            }
        })
        .collect();
    let mut series_counts = BTreeMap::<&str, usize>::new();
    for item in result.iter().filter(|item| !item.excluded) {
        *series_counts.entry(&item.series.target.value).or_default() += 1;
    }
    let weights: Vec<_> = result
        .iter()
        .map(|item| {
            if item.excluded {
                0.0
            } else {
                // A huge imported series should not dominate merely by having more
                // near-duplicates. Square-root correction retains within-series choice.
                ((item.level - 3.0) * 1.1).exp()
                    / (*series_counts
                        .get(item.series.target.value.as_str())
                        .expect("counted") as f64)
                        .sqrt()
            }
        })
        .collect();
    let total: f64 = weights.iter().sum();
    let eligible = result.iter().filter(|item| !item.excluded).count();
    if eligible > 0 {
        for (item, weight) in result.iter_mut().zip(weights) {
            if !item.excluded {
                item.probability = (1.0 - settings.exploration_fraction) * weight / total
                    + settings.exploration_fraction / eligible as f64;
            }
        }
    }
    result
}
fn sample_scores<R: Rng + ?Sized>(
    mut candidates: Vec<ItemScore>,
    limit: usize,
    rng: &mut R,
) -> Vec<ItemScore> {
    candidates.retain(|item| !item.excluded && item.probability > 0.0);
    let mut chosen = Vec::new();
    while chosen.len() < limit && !candidates.is_empty() {
        let total: f64 = candidates.iter().map(|item| item.probability).sum();
        let mut threshold = rng.gen_range(0.0..total);
        let index = candidates
            .iter()
            .position(|item| {
                threshold -= item.probability;
                threshold < 0.0
            })
            .unwrap_or(candidates.len() - 1);
        chosen.push(candidates.remove(index));
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};
    use std::os::unix::fs::PermissionsExt;

    const NOW: i64 = 2_000_000_000;
    fn item(n: u8, series: &str, style: &str) -> ItemFeatures {
        ItemFeatures {
            id: format!("{n:040x}"),
            series_id: series.into(),
            styles: vec![style.into()],
        }
    }
    fn store() -> (tempfile::TempDir, PreferenceStore) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = PreferenceStore::new(dir.path()).unwrap();
        (dir, store)
    }
    fn add(store: &PreferenceStore, scope: &str, id: &str, item: &ItemFeatures, at: i64) {
        store
            .record(
                scope,
                id,
                item,
                at,
                NOW,
                ObservationSource::Human,
                &PreferenceSettings::default(),
            )
            .unwrap();
    }
    #[test]
    fn human_event_deduplication_bot_exclusion_and_scope_isolation() {
        let (_dir, store) = store();
        let item = item(1, "animals", "cute");
        let settings = PreferenceSettings::default();
        assert!(
            store
                .record(
                    "group-a",
                    "event-a",
                    &item,
                    NOW,
                    NOW,
                    ObservationSource::Human,
                    &settings
                )
                .unwrap()
        );
        assert!(
            !store
                .record(
                    "group-a",
                    "event-a",
                    &item,
                    NOW,
                    NOW,
                    ObservationSource::Human,
                    &settings
                )
                .unwrap()
        );
        assert!(
            !store
                .record(
                    "group-a",
                    "bot-a",
                    &item,
                    NOW,
                    NOW,
                    ObservationSource::Bot,
                    &settings
                )
                .unwrap()
        );
        let scores = store
            .scores("group-a", std::slice::from_ref(&item), NOW, &settings)
            .unwrap();
        assert_eq!(scores[0].asset.window_count, 1);
        assert_eq!(scores[0].asset.rolling_count, 1);
        assert_eq!(
            store.scores("private-a", &[item], NOW, &settings).unwrap()[0].level,
            3.0
        );
    }
    #[test]
    fn repeated_days_matter_more_than_same_day_spam_and_generalize() {
        let (_dir, store) = store();
        let liked = item(1, "cats", "cute");
        for n in 0..7 {
            add(&store, "days", &format!("day-{n}"), &liked, NOW - n * DAY);
        }
        for n in 0..100 {
            add(&store, "burst", &format!("burst-{n}"), &liked, NOW - n);
        }
        let candidates = vec![liked, item(2, "cats", "cute"), item(3, "other", "plain")];
        let scores = store
            .scores("days", &candidates, NOW, &PreferenceSettings::default())
            .unwrap();
        let burst = store
            .scores("burst", &candidates, NOW, &PreferenceSettings::default())
            .unwrap();
        assert!(scores[0].level > burst[0].level);
        assert!(scores[1].level > scores[2].level);
        assert!(scores[2].probability > 0.0);
        assert!((scores.iter().map(|s| s.probability).sum::<f64>() - 1.0).abs() < 1e-9);
    }
    #[test]
    fn inactive_chat_only_softens_toward_neutral_while_new_opportunities_change_taste() {
        let (_dir, store) = store();
        let liked = item(1, "cats", "cute");
        let other = item(2, "plain", "plain");
        for n in 0..7 {
            add(&store, "idle", &format!("old-{n}"), &liked, NOW - n * DAY);
            add(&store, "active", &format!("old-{n}"), &liked, NOW - n * DAY);
        }
        let future = NOW + 30 * DAY;
        let settings = PreferenceSettings::default();
        for n in 0..14 {
            store
                .record(
                    "active",
                    &format!("new-{n}"),
                    &other,
                    future - n * DAY,
                    future,
                    ObservationSource::Human,
                    &settings,
                )
                .unwrap();
        }
        let before = store
            .scores("idle", std::slice::from_ref(&liked), NOW, &settings)
            .unwrap()[0]
            .level;
        let idle = store
            .scores("idle", std::slice::from_ref(&liked), future, &settings)
            .unwrap()[0]
            .level;
        let active = store.scores("active", &[liked], future, &settings).unwrap()[0].level;
        assert!(idle > 3.0 && idle < before);
        assert!(idle > active + 0.3);
    }
    #[test]
    fn recent_interest_can_beat_old_rolling_popularity() {
        let (_dir, store) = store();
        let old = item(1, "old", "old");
        let new = item(2, "new", "new");
        for n in 0..50 {
            add(
                &store,
                "scope",
                &format!("old-{n}"),
                &old,
                NOW - (n / 2 + 15) * DAY,
            );
        }
        for n in 0..7 {
            add(&store, "scope", &format!("new-{n}"), &new, NOW - n * DAY);
        }
        let scores = store
            .scores("scope", &[old, new], NOW, &PreferenceSettings::default())
            .unwrap();
        assert!(scores[0].asset.rolling_count > scores[1].asset.rolling_count);
        assert!(scores[1].level > scores[0].level);
    }
    #[test]
    fn explicit_boost_decays_but_bans_never_reappear_and_rule_replay_does_not_reset_time() {
        let (_dir, store) = store();
        let a = item(1, "cats", "cute");
        let b = item(2, "dogs", "cute");
        let settings = PreferenceSettings::default();
        let target = PreferenceTarget::asset(&a.id);
        assert!(
            store
                .set_rule(
                    "scope",
                    target.clone(),
                    RuleAction::Prefer,
                    "prefer-message",
                    NOW
                )
                .unwrap()
        );
        assert!(
            !store
                .set_rule(
                    "scope",
                    target.clone(),
                    RuleAction::Prefer,
                    "prefer-message",
                    NOW + DAY
                )
                .unwrap()
        );
        let initial = store
            .scores("scope", &[a.clone(), b.clone()], NOW, &settings)
            .unwrap();
        let later = store
            .scores("scope", &[a.clone(), b.clone()], NOW + 60 * DAY, &settings)
            .unwrap();
        assert_eq!(initial[0].asset.level, 5.0);
        assert_eq!(later[0].asset.level, 4.0);
        store
            .set_rule(
                "scope",
                PreferenceTarget::style("cute"),
                RuleAction::Avoid,
                "ban-message",
                NOW,
            )
            .unwrap();
        assert!(
            store
                .sample(
                    "scope",
                    &[a.clone(), b.clone()],
                    2,
                    NOW + 300 * DAY,
                    &settings
                )
                .unwrap()
                .is_empty()
        );
        store
            .set_rule(
                "scope",
                PreferenceTarget::style("cute"),
                RuleAction::Reset,
                "reset-message",
                NOW + DAY,
            )
            .unwrap();
        assert!(
            !store
                .set_rule(
                    "scope",
                    PreferenceTarget::style("cute"),
                    RuleAction::Avoid,
                    "ban-message",
                    NOW + 2 * DAY
                )
                .unwrap()
        );
        assert_eq!(
            store
                .sample("scope", &[a, b], 2, NOW + 2 * DAY, &settings)
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn weighted_selection_is_probabilistic_and_never_samples_bans() {
        let (_dir, store) = store();
        let a = item(1, "same", "plain");
        let b = item(2, "same", "plain");
        let banned = item(3, "same", "plain");
        let settings = PreferenceSettings::default();
        store
            .set_rule(
                "scope",
                PreferenceTarget::asset(&a.id),
                RuleAction::Prefer,
                "prefer",
                NOW,
            )
            .unwrap();
        store
            .set_rule(
                "scope",
                PreferenceTarget::asset(&banned.id),
                RuleAction::Avoid,
                "ban",
                NOW,
            )
            .unwrap();
        let scores = store
            .scores(
                "scope",
                &[a.clone(), b.clone(), banned.clone()],
                NOW,
                &settings,
            )
            .unwrap();
        let mut rng = StdRng::seed_from_u64(7);
        let mut counts = BTreeMap::<String, usize>::new();
        for _ in 0..1000 {
            let chosen = sample_scores(scores.clone(), 1, &mut rng);
            *counts.entry(chosen[0].id.clone()).or_default() += 1;
        }
        assert!(counts[&a.id] > counts[&b.id]);
        assert!(counts[&b.id] > 0);
        assert!(!counts.contains_key(&banned.id));
    }
    #[test]
    fn maintenance_bounds_events_retains_last_use_and_does_not_change_explicit_rules() {
        let (_dir, store) = store();
        let a = item(1, "cats", "cute");
        let settings = PreferenceSettings {
            rolling_events: 2,
            max_events: 3,
            ..Default::default()
        };
        for n in 0..5 {
            store
                .record(
                    "scope",
                    &format!("e{n}"),
                    &a,
                    NOW - (5 - n) * DAY,
                    NOW,
                    ObservationSource::Human,
                    &settings,
                )
                .unwrap();
        }
        let scores = store
            .scores("scope", std::slice::from_ref(&a), NOW, &settings)
            .unwrap();
        assert_eq!(scores[0].asset.window_count, 3);
        assert_eq!(scores[0].asset.rolling_count, 2);
        store.maintain("scope", NOW + 91 * DAY, &settings).unwrap();
        let scores = store
            .scores("scope", &[a], NOW + 91 * DAY, &settings)
            .unwrap();
        assert_eq!(scores[0].asset.window_count, 0);
        assert_eq!(scores[0].asset.last_used_at, Some(NOW - DAY));
        assert_eq!(scores[0].level, 3.0);
    }
    #[test]
    fn metadata_completion_and_series_moves_reindex_without_inventing_uses() {
        let (_dir, store) = store();
        let pending = ItemFeatures {
            id: format!("{:040x}", 1),
            series_id: "default".into(),
            styles: vec![],
        };
        add(&store, "group-a", "event-a", &pending, NOW - DAY);
        add(&store, "private-a", "event-b", &pending, NOW);
        let classified = ItemFeatures {
            series_id: "cats".into(),
            styles: vec!["cute".into()],
            ..pending.clone()
        };
        assert_eq!(store.update_item(&classified).unwrap(), 2);
        assert_eq!(store.update_item(&classified).unwrap(), 0);
        let settings = PreferenceSettings::default();
        let score = store
            .scores("group-a", std::slice::from_ref(&classified), NOW, &settings)
            .unwrap();
        assert_eq!(score[0].asset.window_count, 1);
        assert_eq!(score[0].series.window_count, 1);
        assert_eq!(score[0].styles[0].window_count, 1);
        assert_eq!(score[0].styles[0].last_used_at, Some(NOW - DAY));
        store
            .maintain("group-a", NOW + 91 * DAY, &settings)
            .unwrap();
        let moved = ItemFeatures {
            series_id: "favorites".into(),
            styles: vec!["funny".into()],
            ..classified
        };
        assert_eq!(store.update_item(&moved).unwrap(), 2);
        let score = store
            .scores("group-a", &[moved], NOW + 91 * DAY, &settings)
            .unwrap();
        assert_eq!(score[0].series.last_used_at, Some(NOW - DAY));
        assert_eq!(score[0].styles[0].last_used_at, Some(NOW - DAY));
        assert_eq!(score[0].series.window_count, 0);
    }
    #[test]
    fn one_message_can_change_several_targets_but_conflicting_replays_fail() {
        let (_dir, store) = store();
        let target = PreferenceTarget::series("cats");
        assert!(
            store
                .set_rule("scope", target.clone(), RuleAction::Prefer, "message", NOW)
                .unwrap()
        );
        assert!(
            store
                .set_rule(
                    "scope",
                    PreferenceTarget::style("cute"),
                    RuleAction::Prefer,
                    "message",
                    NOW
                )
                .unwrap()
        );
        assert!(
            store
                .set_rule("scope", target, RuleAction::Avoid, "message", NOW)
                .is_err()
        );
    }
    #[test]
    fn preference_boost_does_not_recover_when_old_activity_is_pruned() {
        let (_dir, store) = store();
        let preferred = item(1, "cats", "cute");
        let other = item(2, "other", "plain");
        let settings = PreferenceSettings::default();
        store
            .set_rule(
                "scope",
                PreferenceTarget::asset(&preferred.id),
                RuleAction::Prefer,
                "prefer",
                NOW,
            )
            .unwrap();
        for day in 1..=20 {
            store
                .record(
                    "scope",
                    &format!("day-{day}"),
                    &other,
                    NOW + day * DAY,
                    NOW + day * DAY,
                    ObservationSource::Human,
                    &settings,
                )
                .unwrap();
        }
        let before = store
            .scores(
                "scope",
                std::slice::from_ref(&preferred),
                NOW + 100 * DAY,
                &settings,
            )
            .unwrap()[0]
            .asset
            .level;
        store.maintain("scope", NOW + 120 * DAY, &settings).unwrap();
        let after = store
            .scores("scope", &[preferred], NOW + 120 * DAY, &settings)
            .unwrap()[0]
            .asset
            .level;
        assert!(after > 3.0 && after < before);
    }
    #[test]
    fn private_preference_file_rejects_symlinks_and_unprotected_permissions() {
        let (dir, store) = store();
        let a = item(1, "cats", "cute");
        let settings = PreferenceSettings::default();
        add(&store, "scope", "event", &a, NOW);
        let path = dir
            .path()
            .join("stickers/preferences")
            .join(format!("{}.json", super::super::digest(b"scope")));
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            store
                .scores("scope", std::slice::from_ref(&a), NOW, &settings)
                .is_err()
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let other = path.with_extension("backup");
        std::fs::rename(&path, &other).unwrap();
        std::os::unix::fs::symlink(other, &path).unwrap();
        assert!(store.scores("scope", &[a], NOW, &settings).is_err());
    }
}
