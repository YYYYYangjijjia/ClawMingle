//! Categorized application memory. Every mutation shares the parent canonical
//! file lock and commit; generated documents are never read as authority.
use super::*;

pub const MAX_LEDGER_DRAFTS: usize = 256;
pub const MAX_LEDGER_RECORDS: usize = 256;
pub const MAX_REVIEW_BATCHES: usize = 64;
pub const MAX_DRAFT_CHARS: usize = 500;
pub const MAX_REVIEW_DRAFTS: usize = 8;
const MAX_SUBJECTS: usize = 8;
const MAX_EVIDENCE: usize = 8;
const MAX_REASON_CHARS: usize = 300;
const MAX_GRACE_SECS: u64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Profile,
    Relationship,
    GroupPreference,
    ShortTerm,
}

impl Category {
    fn filename(self) -> &'static str {
        match self {
            Self::Profile => "profiles.json",
            Self::Relationship => "relationships.json",
            Self::GroupPreference => "group_preferences.json",
            Self::ShortTerm => "short_term.json",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Retain,
    ContextOnly,
    Discard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Automatic,
    UserRequested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Certainty {
    Explicit,
    Inferred,
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftStatus {
    Buffered,
    NoticePending,
    Reviewing,
    Applied,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Pending,
    Sent,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemoryOperation {
    /// A correction link preserves the earlier record and its current text.
    Append {
        supersedes: Option<u64>,
    },
    Rewrite {
        record_id: u64,
        revision: u64,
    },
}

/// Trusted application metadata, resolved in the current conversation. This is
/// not a model-call argument contract: the broker must resolve IDs and authors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceInput {
    pub message_id: String,
    pub message_ref: String,
    pub author_id: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftInput {
    pub category: Category,
    pub subjects: Vec<String>,
    pub content: String,
    pub disposition: Disposition,
    pub selection_reason: String,
    pub source_kind: SourceKind,
    pub certainty: Certainty,
    pub evidence: Vec<EvidenceInput>,
    pub expires_at: Option<i64>,
    pub operation: MemoryOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryEvidence {
    pub message_hash: String,
    pub message_ref: String,
    pub author: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryContent {
    pub category: Category,
    pub subjects: Vec<String>,
    pub content: String,
    pub disposition: Disposition,
    pub selection_reason: String,
    pub source_kind: SourceKind,
    pub certainty: Certainty,
    pub evidence: Vec<MemoryEvidence>,
    pub expires_at: Option<i64>,
    pub operation: MemoryOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DraftRevision {
    pub revision: u64,
    pub target_revision: Option<u64>,
    #[serde(flatten)]
    pub data: MemoryContent,
    pub changed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Draft {
    pub id: u64,
    pub revision: u64,
    pub target_revision: Option<u64>,
    #[serde(flatten)]
    pub data: MemoryContent,
    pub created_at: i64,
    pub updated_at: i64,
    pub status: DraftStatus,
    pub review_batch_id: Option<u64>,
    pub applied_record_id: Option<u64>,
    pub created_by_operator: bool,
    pub history: Vec<DraftRevision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppliedBy {
    ScheduledArchive,
    SilenceGrace,
    ExplicitRequest,
    OperatorAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryRevision {
    pub revision: u64,
    #[serde(flatten)]
    pub data: MemoryContent,
    pub applied_at: i64,
    pub applied_by: AppliedBy,
    pub draft_id: u64,
    pub draft_revision: u64,
    pub confirmation: Option<MemoryEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryRecord {
    pub id: u64,
    pub revision: u64,
    pub status: RecordStatus,
    pub superseded_by: Option<u64>,
    #[serde(flatten)]
    pub data: MemoryContent,
    pub created_at: i64,
    pub updated_at: i64,
    pub history: Vec<MemoryRevision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordStatus {
    Active,
    Superseded,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftRef {
    pub id: u64,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewBatch {
    pub id: u64,
    pub drafts: Vec<DraftRef>,
    pub grace_secs: u64,
    pub created_at: i64,
    pub sent_at: Option<i64>,
    pub deadline: Option<i64>,
    pub ack_hash: Option<String>,
    pub status: ReviewStatus,
    pub notice: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceState {
    pub history_cursor: u64,
    pub last_completed_day: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LedgerView {
    pub revision: u64,
    pub drafts: Vec<Draft>,
    pub records: Vec<MemoryRecord>,
    pub reviews: Vec<ReviewBatch>,
    pub maintenance: MaintenanceState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct LedgerReport {
    pub applied: usize,
    pub duplicates: usize,
    pub conflicts: usize,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CategoryFile {
    pub category: Category,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryLayout {
    pub canonical: String,
    pub legacy_authority: String,
    pub drafts_json: String,
    pub drafts_markdown: String,
    pub revisions_json: String,
    pub categories: Vec<CategoryFile>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDraftRevision {
    revision: u64,
    input: DraftInput,
    changed_at: i64,
    #[serde(default)]
    target_revision: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDraft {
    id: u64,
    versions: Vec<StoredDraftRevision>,
    created_at: i64,
    updated_at: i64,
    status: DraftStatus,
    review_batch_id: Option<u64>,
    applied_record_id: Option<u64>,
    #[serde(default)]
    created_by_operator: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRecordRevision {
    input: DraftInput,
    applied_at: i64,
    applied_by: AppliedBy,
    draft_id: u64,
    draft_revision: u64,
    explicit_evidence: Option<EvidenceInput>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRecord {
    id: u64,
    versions: Vec<StoredRecordRevision>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Ledger {
    revision: u64,
    next_draft: u64,
    next_record: u64,
    next_review: u64,
    drafts: Vec<StoredDraft>,
    records: Vec<StoredRecord>,
    reviews: Vec<ReviewBatch>,
    #[serde(default)]
    maintenance: MaintenanceState,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            revision: 0,
            next_draft: 1,
            next_record: 1,
            next_review: 1,
            drafts: Vec::new(),
            records: Vec::new(),
            reviews: Vec::new(),
            maintenance: MaintenanceState::default(),
        }
    }
}

impl GroupMemoryStore {
    pub fn buffer(&self, input: DraftInput, now: i64) -> Result<Draft> {
        validate_input(&input)?;
        valid_now(now)?;
        if expired(&input, now) {
            return Err(GroupMemoryError::InvalidInput);
        }
        self.transact(|state| buffer_into(self, &mut state.ledger, input, now))
    }

    /// One capture checkpoint: stale concurrent readers and partial failures
    /// cannot append a different interpretation before advancing the cursor.
    /// This method uses the explicitly enabled no-notice archive policy.
    pub fn commit_capture(
        &self,
        expected_cursor: u64,
        new_cursor: u64,
        inputs: Vec<DraftInput>,
        now: i64,
    ) -> Result<LedgerReport> {
        self.commit_capture_checked(expected_cursor, None, new_cursor, inputs, now)
    }

    pub fn commit_capture_at_revision(
        &self,
        expected_cursor: u64,
        expected_ledger_revision: u64,
        new_cursor: u64,
        inputs: Vec<DraftInput>,
        now: i64,
    ) -> Result<LedgerReport> {
        self.commit_capture_checked(
            expected_cursor,
            Some(expected_ledger_revision),
            new_cursor,
            inputs,
            now,
        )
    }

    fn commit_capture_checked(
        &self,
        expected_cursor: u64,
        expected_ledger_revision: Option<u64>,
        new_cursor: u64,
        inputs: Vec<DraftInput>,
        now: i64,
    ) -> Result<LedgerReport> {
        valid_now(now)?;
        if new_cursor < expected_cursor || inputs.len() > MAX_REVIEW_DRAFTS {
            return Err(GroupMemoryError::InvalidInput);
        }
        for input in &inputs {
            validate_input(input)?;
            if expired(input, now) {
                return Err(GroupMemoryError::InvalidInput);
            }
        }
        self.transact(|state| {
            let ledger = &mut state.ledger;
            if ledger.maintenance.history_cursor != expected_cursor
                || expected_ledger_revision.is_some_and(|revision| revision != ledger.revision)
            {
                return Err(GroupMemoryError::StaleRevision);
            }
            for input in inputs {
                buffer_into(self, ledger, input, now)?;
            }
            let indices = ledger
                .drafts
                .iter()
                .enumerate()
                .filter_map(|(index, draft)| {
                    (draft.status == DraftStatus::Buffered
                        && auto_eligible(&current(draft).input, now))
                    .then_some(index)
                })
                .collect::<Vec<_>>();
            let report = apply_indices(self, ledger, &indices, now, AppliedBy::ScheduledArchive)?;
            ledger.maintenance.history_cursor = new_cursor;
            ledger.maintenance.last_error = None;
            ledger.maintenance.updated_at = Some(now);
            bump(ledger)?;
            Ok(LedgerReport {
                revision: ledger.revision,
                ..report
            })
        })
    }

    pub fn edit_draft(&self, id: u64, revision: u64, input: DraftInput, now: i64) -> Result<Draft> {
        validate_input(&input)?;
        valid_now(now)?;
        if expired(&input, now) {
            return Err(GroupMemoryError::InvalidInput);
        }
        self.transact(|state| {
            let ledger = &mut state.ledger;
            let input = normalize_append_target(ledger, input)?;
            let index = draft_index(ledger, id, revision)?;
            if matches!(
                ledger.drafts[index].status,
                DraftStatus::Applied | DraftStatus::Rejected
            ) {
                return Err(GroupMemoryError::NotPending);
            }
            if current(&ledger.drafts[index]).input == input {
                return Ok(self.project_draft(&ledger.drafts[index]));
            }
            if ledger.drafts[index].versions.len() >= MAX_FACT_REVISIONS {
                return Err(GroupMemoryError::RevisionLimit);
            }
            validate_target(ledger, &input)?;
            let target_revision = target_revision(ledger, &input);
            invalidate_review(ledger, index);
            let draft = &mut ledger.drafts[index];
            if now < draft.updated_at {
                return Err(GroupMemoryError::InvalidInput);
            }
            draft.versions.push(StoredDraftRevision {
                revision: revision + 1,
                input,
                changed_at: now,
                target_revision,
            });
            draft.updated_at = now;
            let public = self.project_draft(draft);
            bump(ledger)?;
            Ok(public)
        })
    }

    pub fn reject_draft(&self, id: u64, revision: u64, now: i64) -> Result<Draft> {
        valid_now(now)?;
        self.transact(|state| {
            let ledger = &mut state.ledger;
            let index = draft_index(ledger, id, revision)?;
            if ledger.drafts[index].status == DraftStatus::Applied {
                return Err(GroupMemoryError::AlreadyApplied);
            }
            if ledger.drafts[index].status == DraftStatus::Rejected {
                return Ok(self.project_draft(&ledger.drafts[index]));
            }
            if now < ledger.drafts[index].updated_at {
                return Err(GroupMemoryError::InvalidInput);
            }
            invalidate_review(ledger, index);
            let draft = &mut ledger.drafts[index];
            draft.status = DraftStatus::Rejected;
            draft.updated_at = now;
            let public = self.project_draft(draft);
            bump(ledger)?;
            Ok(public)
        })
    }

    /// Authenticated host UI convenience; identities and source evidence cannot
    /// be replaced by an editable text field.
    pub fn edit_content(&self, id: u64, revision: u64, content: &str, now: i64) -> Result<Draft> {
        valid_now(now)?;
        self.transact(|state| {
            let ledger = &mut state.ledger;
            let index = draft_index(ledger, id, revision)?;
            let draft = &ledger.drafts[index];
            if matches!(draft.status, DraftStatus::Applied | DraftStatus::Rejected) {
                return Err(GroupMemoryError::NotPending);
            }
            let mut input = current(draft).input.clone();
            input.content = content.to_owned();
            validate_input(&input)?;
            if expired(&input, now) || now < draft.updated_at {
                return Err(GroupMemoryError::InvalidInput);
            }
            if input == current(draft).input {
                return Ok(self.project_draft(draft));
            }
            if draft.versions.len() >= MAX_FACT_REVISIONS {
                return Err(GroupMemoryError::RevisionLimit);
            }
            let target_revision = current(draft).target_revision;
            invalidate_review(ledger, index);
            let draft = &mut ledger.drafts[index];
            draft.versions.push(StoredDraftRevision {
                revision: revision + 1,
                input,
                changed_at: now,
                target_revision,
            });
            draft.updated_at = now;
            let public = self.project_draft(draft);
            bump(ledger)?;
            Ok(public)
        })
    }

    /// Creates a correction proposal only. The previous formal record remains
    /// current until a separate authenticated operator or explicit QQ action.
    pub fn propose_record_correction(
        &self,
        record_id: u64,
        revision: u64,
        content: &str,
        now: i64,
    ) -> Result<Draft> {
        valid_now(now)?;
        self.transact(|state| {
            let ledger = &mut state.ledger;
            let record = ledger
                .records
                .iter()
                .find(|record| record.id == record_id)
                .ok_or(GroupMemoryError::NotFound)?;
            if record.versions.len() as u64 != revision {
                return Err(GroupMemoryError::StaleRevision);
            }
            let mut input = record
                .versions
                .last()
                .ok_or(GroupMemoryError::CorruptStore)?
                .input
                .clone();
            input.content = content.to_owned();
            input.source_kind = SourceKind::UserRequested;
            input.operation = MemoryOperation::Rewrite {
                record_id,
                revision,
            };
            validate_input(&input)?;
            if expired(&input, now) {
                return Err(GroupMemoryError::InvalidInput);
            }
            if let Some(draft) = ledger
                .drafts
                .iter()
                .find(|draft| draft.created_by_operator && current(draft).input == input)
            {
                return Ok(self.project_draft(draft));
            }
            if ledger.drafts.len() >= MAX_LEDGER_DRAFTS {
                return Err(GroupMemoryError::PendingLimit);
            }
            let draft = StoredDraft {
                id: next_id(&self.group_hash, "ledger-draft", &mut ledger.next_draft)?,
                versions: vec![StoredDraftRevision {
                    revision: 1,
                    input,
                    changed_at: now,
                    target_revision: Some(revision),
                }],
                created_at: now,
                updated_at: now,
                status: DraftStatus::Buffered,
                review_batch_id: None,
                applied_record_id: None,
                created_by_operator: true,
            };
            let public = self.project_draft(&draft);
            ledger.drafts.push(draft);
            bump(ledger)?;
            Ok(public)
        })
    }

    pub fn ledger_view(&self) -> Result<LedgerView> {
        self.ledger_view_at(unix_now()?)
    }

    pub fn ledger_view_at(&self, now: i64) -> Result<LedgerView> {
        valid_now(now)?;
        self.transact(|state| Ok(self.project_ledger(&state.ledger, now)))
    }

    pub fn prepare_review(
        &self,
        references: &[(u64, u64)],
        grace_secs: u64,
        now: i64,
    ) -> Result<ReviewBatch> {
        valid_now(now)?;
        if references.is_empty()
            || references.len() > MAX_REVIEW_DRAFTS
            || grace_secs > MAX_GRACE_SECS
            || references
                .iter()
                .map(|item| item.0)
                .collect::<BTreeSet<_>>()
                .len()
                != references.len()
        {
            return Err(GroupMemoryError::InvalidInput);
        }
        self.transact(|state| {
            let ledger = &mut state.ledger;
            let frozen = references
                .iter()
                .map(|&(id, revision)| DraftRef { id, revision })
                .collect::<Vec<_>>();
            if let Some(review) = ledger.reviews.iter().find(|review| {
                review.drafts == frozen
                    && review.grace_secs == grace_secs
                    && matches!(review.status, ReviewStatus::Pending | ReviewStatus::Sent)
            }) {
                return Ok(review.clone());
            }
            if ledger.reviews.len() >= MAX_REVIEW_BATCHES {
                return Err(GroupMemoryError::PendingLimit);
            }
            let mut notice =
                String::from("准备整理以下记忆草稿；送达后进入公示期，可在期限内撤回：\n");
            let mut indices = Vec::new();
            for &(id, revision) in references {
                let index = draft_index(ledger, id, revision)?;
                let draft = &ledger.drafts[index];
                if draft.status != DraftStatus::Buffered
                    || !auto_eligible(&current(draft).input, now)
                {
                    return Err(GroupMemoryError::NotPending);
                }
                let text = &current(draft).input.content;
                notice.push_str(&format!(
                    "\n{id} v{revision}：{text}\n撤回：/memory reject {id} {revision}\n"
                ));
                indices.push(index);
            }
            // Frozen content is never shortened: omit a draft from the batch
            // instead of claiming notice for text the recipient could not see.
            if notice.chars().count() > 4000 {
                return Err(GroupMemoryError::InvalidInput);
            }
            let review = ReviewBatch {
                id: next_id(&self.group_hash, "ledger-review", &mut ledger.next_review)?,
                drafts: frozen,
                grace_secs,
                created_at: now,
                sent_at: None,
                deadline: None,
                ack_hash: None,
                status: ReviewStatus::Pending,
                notice,
            };
            for index in indices {
                ledger.drafts[index].status = DraftStatus::NoticePending;
                ledger.drafts[index].review_batch_id = Some(review.id);
            }
            ledger.reviews.push(review.clone());
            bump(ledger)?;
            Ok(review)
        })
    }

    /// Only the host's positive QQ ACK may call this. Queueing a notice,
    /// generating it or receiving an unknown/failed send never starts grace.
    pub fn mark_review_sent(
        &self,
        batch_id: u64,
        ack_id: &str,
        sent_at: i64,
    ) -> Result<ReviewBatch> {
        validate_id(ack_id)?;
        valid_now(sent_at)?;
        self.transact(|state| {
            let ledger = &mut state.ledger;
            let index = ledger
                .reviews
                .iter()
                .position(|review| review.id == batch_id)
                .ok_or(GroupMemoryError::NotFound)?;
            let review = &ledger.reviews[index];
            if matches!(review.status, ReviewStatus::Sent | ReviewStatus::Completed)
                && review.ack_hash.as_deref() == Some(digest(ack_id.as_bytes()).as_str())
                && review.sent_at == Some(sent_at)
            {
                return Ok(review.clone());
            }
            if review.status != ReviewStatus::Pending || sent_at < review.created_at {
                return Err(GroupMemoryError::NotPending);
            }
            let deadline = sent_at
                .checked_add(
                    i64::try_from(review.grace_secs).map_err(|_| GroupMemoryError::InvalidInput)?,
                )
                .ok_or(GroupMemoryError::InvalidInput)?;
            let refs = review.drafts.clone();
            for reference in refs {
                let draft_index = draft_index(ledger, reference.id, reference.revision)?;
                let draft = &mut ledger.drafts[draft_index];
                if draft.status != DraftStatus::NoticePending
                    || draft.review_batch_id != Some(batch_id)
                {
                    return Err(GroupMemoryError::NotPending);
                }
                draft.status = DraftStatus::Reviewing;
            }
            let review = &mut ledger.reviews[index];
            review.status = ReviewStatus::Sent;
            review.sent_at = Some(sent_at);
            review.deadline = Some(deadline);
            review.ack_hash = Some(digest(ack_id.as_bytes()));
            let public = review.clone();
            bump(ledger)?;
            Ok(public)
        })
    }

    pub fn abandon_review(&self, batch_id: u64, now: i64) -> Result<()> {
        valid_now(now)?;
        self.transact(|state| {
            let ledger = &mut state.ledger;
            let review = ledger
                .reviews
                .iter()
                .find(|review| review.id == batch_id)
                .ok_or(GroupMemoryError::NotFound)?;
            if review.status == ReviewStatus::Completed {
                return Err(GroupMemoryError::AlreadyApplied);
            }
            if review.status == ReviewStatus::Cancelled {
                return Ok(());
            }
            if let Some(index) = ledger
                .drafts
                .iter()
                .position(|draft| draft.review_batch_id == Some(batch_id))
            {
                invalidate_review(ledger, index);
            }
            bump(ledger)
        })
    }

    /// Explicitly configured no-notice policy. Call only at the application's
    /// authorized archive boundary; it cannot bypass an already queued review.
    pub fn archive_ready(&self, now: i64) -> Result<LedgerReport> {
        valid_now(now)?;
        self.transact(|state| {
            let ledger = &mut state.ledger;
            let indices = ledger
                .drafts
                .iter()
                .enumerate()
                .filter_map(|(index, draft)| {
                    (draft.status == DraftStatus::Buffered
                        && auto_eligible(&current(draft).input, now))
                    .then_some(index)
                })
                .collect::<Vec<_>>();
            apply_indices(self, ledger, &indices, now, AppliedBy::ScheduledArchive)
        })
    }

    pub fn archive_due(&self, now: i64) -> Result<LedgerReport> {
        valid_now(now)?;
        self.transact(|state| {
            let ledger = &mut state.ledger;
            let batches = ledger
                .reviews
                .iter()
                .enumerate()
                .filter_map(|(index, review)| {
                    (review.status == ReviewStatus::Sent
                        && review.deadline.is_some_and(|deadline| deadline <= now))
                    .then_some(index)
                })
                .collect::<Vec<_>>();
            let mut indices = Vec::new();
            for &index in &batches {
                for reference in &ledger.reviews[index].drafts {
                    let draft_index = draft_index(ledger, reference.id, reference.revision)?;
                    if auto_eligible(&current(&ledger.drafts[draft_index]).input, now) {
                        indices.push(draft_index);
                    }
                }
            }
            let report = apply_indices(self, ledger, &indices, now, AppliedBy::SilenceGrace)?;
            for index in batches {
                let batch_id = ledger.reviews[index].id;
                for draft in &mut ledger.drafts {
                    if draft.review_batch_id == Some(batch_id) {
                        draft.review_batch_id = None;
                        if draft.status == DraftStatus::Reviewing {
                            draft.status = DraftStatus::Buffered;
                        }
                    }
                }
                ledger.reviews[index].status = ReviewStatus::Completed;
                bump(ledger)?;
            }
            Ok(LedgerReport {
                revision: ledger.revision,
                ..report
            })
        })
    }

    /// The caller must establish the user's explicit instruction from original
    /// text, not quotes/model output. Confirmation can be a later message; its
    /// identity must be among the draft's actual subjects or source authors.
    pub fn apply_explicit(
        &self,
        id: u64,
        revision: u64,
        evidence: EvidenceInput,
        now: i64,
    ) -> Result<MemoryRecord> {
        validate_evidence(&evidence)?;
        valid_now(now)?;
        if evidence.timestamp > now {
            return Err(GroupMemoryError::InvalidInput);
        }
        self.transact(|state| {
            let ledger = &mut state.ledger;
            let index = draft_index(ledger, id, revision)?;
            let draft = &ledger.drafts[index];
            let input = &current(draft).input;
            let authority = confirmation_authority(ledger, current(draft))?;
            if !may_confirm(authority, &evidence.author_id) {
                return Err(GroupMemoryError::NotOwner);
            }
            if input.disposition != Disposition::Retain || expired(input, now) {
                return Err(GroupMemoryError::NotOwner);
            }
            if draft.status == DraftStatus::Applied {
                let record = ledger
                    .records
                    .iter()
                    .find(|record| Some(record.id) == draft.applied_record_id)
                    .ok_or(GroupMemoryError::CorruptStore)?;
                return Ok(self.project_record(record, ledger, now));
            }
            if draft.status == DraftStatus::Rejected {
                return Err(GroupMemoryError::NotPending);
            }
            invalidate_review(ledger, index);
            let (record_id, _) = apply_one(
                self,
                ledger,
                index,
                now,
                AppliedBy::ExplicitRequest,
                Some(evidence),
            )?;
            bump(ledger)?;
            Ok(self.project_record(
                ledger
                    .records
                    .iter()
                    .find(|record| record.id == record_id)
                    .ok_or(GroupMemoryError::CorruptStore)?,
                ledger,
                now,
            ))
        })
    }

    /// Host-only authenticated administrator action; no synthetic QQ author,
    /// message reference or confirmation is manufactured for this provenance.
    pub fn apply_operator(&self, id: u64, revision: u64, now: i64) -> Result<MemoryRecord> {
        valid_now(now)?;
        self.transact(|state| {
            let ledger = &mut state.ledger;
            let index = draft_index(ledger, id, revision)?;
            let draft = &ledger.drafts[index];
            let input = &current(draft).input;
            if input.disposition != Disposition::Retain || expired(input, now) {
                return Err(GroupMemoryError::NotPending);
            }
            if draft.status == DraftStatus::Applied {
                let record = ledger
                    .records
                    .iter()
                    .find(|record| Some(record.id) == draft.applied_record_id)
                    .ok_or(GroupMemoryError::CorruptStore)?;
                return Ok(self.project_record(record, ledger, now));
            }
            if draft.status == DraftStatus::Rejected {
                return Err(GroupMemoryError::NotPending);
            }
            invalidate_review(ledger, index);
            let (record_id, _) =
                apply_one(self, ledger, index, now, AppliedBy::OperatorAction, None)?;
            bump(ledger)?;
            Ok(self.project_record(
                ledger
                    .records
                    .iter()
                    .find(|record| record.id == record_id)
                    .ok_or(GroupMemoryError::CorruptStore)?,
                ledger,
                now,
            ))
        })
    }

    pub fn advance_scan(
        &self,
        expected_cursor: u64,
        new_cursor: u64,
        now: i64,
    ) -> Result<MaintenanceState> {
        valid_now(now)?;
        self.transact(|state| {
            let ledger = &mut state.ledger;
            if ledger.maintenance.history_cursor != expected_cursor {
                return Err(GroupMemoryError::StaleRevision);
            }
            if new_cursor < expected_cursor {
                return Err(GroupMemoryError::InvalidInput);
            }
            ledger.maintenance.history_cursor = new_cursor;
            ledger.maintenance.last_error = None;
            ledger.maintenance.updated_at = Some(now);
            bump(ledger)?;
            Ok(ledger.maintenance.clone())
        })
    }

    pub fn complete_day(
        &self,
        expected_cursor: u64,
        day: &str,
        now: i64,
    ) -> Result<MaintenanceState> {
        valid_now(now)?;
        validate_day(day)?;
        self.transact(|state| {
            let ledger = &mut state.ledger;
            if ledger.maintenance.history_cursor != expected_cursor {
                return Err(GroupMemoryError::StaleRevision);
            }
            if ledger
                .maintenance
                .last_completed_day
                .as_deref()
                .is_some_and(|previous| previous > day)
            {
                return Err(GroupMemoryError::InvalidInput);
            }
            ledger.maintenance.last_completed_day = Some(day.to_owned());
            ledger.maintenance.last_error = None;
            ledger.maintenance.updated_at = Some(now);
            bump(ledger)?;
            Ok(ledger.maintenance.clone())
        })
    }

    pub fn record_scan_error(&self, code: &str, now: i64) -> Result<MaintenanceState> {
        valid_now(now)?;
        validate_error_code(code)?;
        self.transact(|state| {
            let ledger = &mut state.ledger;
            ledger.maintenance.last_error = Some(code.to_owned());
            ledger.maintenance.updated_at = Some(now);
            bump(ledger)?;
            Ok(ledger.maintenance.clone())
        })
    }

    pub fn inject_ledger_for(
        &self,
        subject_ids: &[&str],
        budget: usize,
        now: i64,
    ) -> Result<Injection> {
        valid_now(now)?;
        if budget > MAX_INJECTION_BYTES {
            return Err(GroupMemoryError::InvalidBudget);
        }
        let subjects = subject_ids.iter().copied().collect::<BTreeSet<_>>();
        for subject in &subjects {
            validate_id(subject)?;
        }
        self.transact(|state| {
            let eligible = state.ledger.records.iter().filter(|record| {
                let input = &record.versions.last().expect("validated record").input;
                record_status(&state.ledger,record,now)==RecordStatus::Active && input.subjects.iter().all(|subject| subjects.contains(subject.as_str()))
            }).collect::<Vec<_>>();
            let encode = |records: &[serde_json::Value]| serde_json::to_string(&serde_json::json!({
                "kind":"untrusted_categorized_memory", "basis":"application_memory_with_source_and_certainty",
                "revision":state.ledger.revision,"records":records
            })).map_err(|_|GroupMemoryError::CorruptStore);
            let mut records = Vec::new(); let mut json = encode(&records)?;
            if json.len() > budget { return Ok(Injection { json:String::new(), facts:0, truncated:!eligible.is_empty() }); }
            let mut counts = std::collections::BTreeMap::<&str, usize>::new();
            for record in eligible.iter().rev() {
                let input = &record.versions.last().expect("validated record").input;
                if records.len() >= MAX_INJECTION_FACTS || input.subjects.iter().any(|subject| counts.get(subject.as_str()).copied().unwrap_or(0) >= MAX_FACTS_PER_SUBJECT) { continue; }
                let public = self.project_record(record,&state.ledger,now);
                // Revision history remains queryable; inject only current data.
                records.push(serde_json::json!({"id":public.id,"revision":public.revision,"updated_at":public.updated_at,"data":public.data}));
                let candidate = encode(&records)?;
                if candidate.len() <= budget { json = candidate; for subject in &input.subjects { *counts.entry(subject.as_str()).or_default() += 1; } }
                else { records.pop(); }
            }
            Ok(Injection { facts:records.len(), truncated:records.len() < eligible.len(), json })
        })
    }

    pub fn memory_layout(&self) -> MemoryLayout {
        let root = format!("memory/groups/G{}", self.group_hash);
        MemoryLayout {
            canonical: format!("{root}/store.json"),
            legacy_authority: format!("{root}/authority.json"),
            drafts_json: format!("{root}/documents/memory/CHANGES.json"),
            drafts_markdown: format!("{root}/documents/memory/CHANGES.md"),
            revisions_json: format!("{root}/authority/revisions.json"),
            categories: categories()
                .into_iter()
                .map(|category| CategoryFile {
                    category,
                    path: format!("{root}/authority/{}", category.filename()),
                })
                .collect(),
        }
    }

    fn project_input(&self, input: &DraftInput) -> MemoryContent {
        MemoryContent {
            category: input.category,
            subjects: input
                .subjects
                .iter()
                .map(|subject| self.alias(subject))
                .collect(),
            content: input.content.clone(),
            disposition: input.disposition,
            selection_reason: input.selection_reason.clone(),
            source_kind: input.source_kind,
            certainty: input.certainty,
            evidence: input
                .evidence
                .iter()
                .map(|evidence| MemoryEvidence {
                    message_hash: digest(evidence.message_id.as_bytes()),
                    message_ref: evidence.message_ref.clone(),
                    author: self.alias(&evidence.author_id),
                    timestamp: evidence.timestamp,
                })
                .collect(),
            expires_at: input.expires_at,
            operation: input.operation.clone(),
        }
    }

    fn project_draft(&self, draft: &StoredDraft) -> Draft {
        Draft {
            id: draft.id,
            revision: current(draft).revision,
            target_revision: current(draft).target_revision,
            data: self.project_input(&current(draft).input),
            created_at: draft.created_at,
            updated_at: draft.updated_at,
            status: draft.status,
            review_batch_id: draft.review_batch_id,
            applied_record_id: draft.applied_record_id,
            created_by_operator: draft.created_by_operator,
            history: draft
                .versions
                .iter()
                .map(|version| DraftRevision {
                    revision: version.revision,
                    target_revision: version.target_revision,
                    data: self.project_input(&version.input),
                    changed_at: version.changed_at,
                })
                .collect(),
        }
    }

    fn project_record(&self, record: &StoredRecord, ledger: &Ledger, now: i64) -> MemoryRecord {
        let live = record.versions.last().expect("validated record");
        MemoryRecord {
            id: record.id,
            revision: record.versions.len() as u64,
            status: record_status(ledger, record, now),
            superseded_by: superseded_by(ledger, record.id),
            data: self.project_input(&live.input),
            created_at: record.versions[0].applied_at,
            updated_at: live.applied_at,
            history: record
                .versions
                .iter()
                .enumerate()
                .map(|(index, version)| MemoryRevision {
                    revision: index as u64 + 1,
                    data: self.project_input(&version.input),
                    applied_at: version.applied_at,
                    applied_by: version.applied_by,
                    draft_id: version.draft_id,
                    draft_revision: version.draft_revision,
                    confirmation: version
                        .explicit_evidence
                        .as_ref()
                        .map(|source| MemoryEvidence {
                            message_hash: digest(source.message_id.as_bytes()),
                            message_ref: source.message_ref.clone(),
                            author: self.alias(&source.author_id),
                            timestamp: source.timestamp,
                        }),
                })
                .collect(),
        }
    }

    fn project_ledger(&self, ledger: &Ledger, now: i64) -> LedgerView {
        LedgerView {
            revision: ledger.revision,
            drafts: ledger
                .drafts
                .iter()
                .map(|draft| self.project_draft(draft))
                .collect(),
            records: ledger
                .records
                .iter()
                .map(|record| self.project_record(record, ledger, now))
                .collect(),
            reviews: ledger.reviews.clone(),
            maintenance: ledger.maintenance.clone(),
        }
    }
}

fn current(draft: &StoredDraft) -> &StoredDraftRevision {
    draft.versions.last().expect("validated draft")
}
fn valid_now(now: i64) -> Result<()> {
    if now < 0 {
        Err(GroupMemoryError::InvalidInput)
    } else {
        Ok(())
    }
}
fn expired(input: &DraftInput, now: i64) -> bool {
    input.expires_at.is_some_and(|expiry| expiry <= now)
}
fn bump(ledger: &mut Ledger) -> Result<()> {
    ledger.revision = ledger
        .revision
        .checked_add(1)
        .ok_or(GroupMemoryError::StoreLimit)?;
    Ok(())
}
fn categories() -> [Category; 4] {
    [
        Category::Profile,
        Category::Relationship,
        Category::GroupPreference,
        Category::ShortTerm,
    ]
}

fn buffer_into(
    store: &GroupMemoryStore,
    ledger: &mut Ledger,
    input: DraftInput,
    now: i64,
) -> Result<Draft> {
    let input = normalize_append_target(ledger, input)?;
    if let Some(existing) = ledger
        .drafts
        .iter()
        .find(|draft| draft.versions.iter().any(|version| version.input == input))
    {
        return Ok(store.project_draft(existing));
    }
    if let Some(record) = duplicate_record(ledger, &input, now) {
        let draft_id = record
            .versions
            .last()
            .ok_or(GroupMemoryError::CorruptStore)?
            .draft_id;
        let draft = ledger
            .drafts
            .iter()
            .find(|draft| draft.id == draft_id)
            .ok_or(GroupMemoryError::CorruptStore)?;
        return Ok(store.project_draft(draft));
    }
    validate_target(ledger, &input)?;
    let target_revision = target_revision(ledger, &input);
    if ledger.drafts.len() >= MAX_LEDGER_DRAFTS {
        return Err(GroupMemoryError::PendingLimit);
    }
    let draft = StoredDraft {
        id: next_id(&store.group_hash, "ledger-draft", &mut ledger.next_draft)?,
        versions: vec![StoredDraftRevision {
            revision: 1,
            input,
            changed_at: now,
            target_revision,
        }],
        created_at: now,
        updated_at: now,
        status: DraftStatus::Buffered,
        review_batch_id: None,
        applied_record_id: None,
        created_by_operator: false,
    };
    let public = store.project_draft(&draft);
    ledger.drafts.push(draft);
    bump(ledger)?;
    Ok(public)
}

fn validate_evidence(evidence: &EvidenceInput) -> Result<()> {
    validate_id(&evidence.message_id)?;
    validate_id(&evidence.author_id)?;
    let seq = evidence
        .message_ref
        .strip_prefix('M')
        .and_then(|value| value.parse::<u64>().ok());
    if evidence.timestamp < 0
        || seq.is_none_or(|seq| seq == 0)
        || evidence.message_ref.len() > 22
        || seq.is_some_and(|seq| format!("M{seq}") != evidence.message_ref)
    {
        return Err(GroupMemoryError::InvalidInput);
    }
    Ok(())
}

fn validate_input(input: &DraftInput) -> Result<()> {
    if (input.subjects.is_empty() && input.category != Category::GroupPreference)
        || input.subjects.len() > MAX_SUBJECTS
        || input.subjects.iter().collect::<BTreeSet<_>>().len() != input.subjects.len()
        || input.content.trim().is_empty()
        || input.content.contains('\0')
        || input.content.chars().count() > MAX_DRAFT_CHARS
        || input.content.len() > MAX_DRAFT_CHARS * 4
        || input.selection_reason.trim().is_empty()
        || input.selection_reason.contains('\0')
        || input.selection_reason.chars().count() > MAX_REASON_CHARS
        || input.evidence.is_empty()
        || input.evidence.len() > MAX_EVIDENCE
        || input
            .evidence
            .iter()
            .map(|evidence| (&evidence.message_id, &evidence.author_id))
            .collect::<BTreeSet<_>>()
            .len()
            != input.evidence.len()
        || input.expires_at.is_some_and(|expiry| expiry < 0)
    {
        return Err(GroupMemoryError::InvalidInput);
    }
    for subject in &input.subjects {
        validate_id(subject)?;
    }
    for evidence in &input.evidence {
        validate_evidence(evidence)?;
    }
    match input.operation {
        MemoryOperation::Append {
            supersedes: Some(0),
        }
        | MemoryOperation::Rewrite { record_id: 0, .. }
        | MemoryOperation::Rewrite { revision: 0, .. } => {
            return Err(GroupMemoryError::InvalidInput);
        }
        _ => {}
    }
    Ok(())
}

fn validate_target(ledger: &Ledger, input: &DraftInput) -> Result<()> {
    if let Some(id) = target_id(input) {
        let record = ledger
            .records
            .iter()
            .find(|record| record.id == id)
            .ok_or(GroupMemoryError::NotFound)?;
        validate_target_shape(record, input)?;
        if let MemoryOperation::Rewrite { revision, .. } = input.operation
            && record.versions.len() as u64 != revision
        {
            return Err(GroupMemoryError::StaleRevision);
        }
    }
    Ok(())
}

fn target_id(input: &DraftInput) -> Option<u64> {
    match input.operation {
        MemoryOperation::Append { supersedes } => supersedes,
        MemoryOperation::Rewrite { record_id, .. } => Some(record_id),
    }
}

fn validate_target_shape(record: &StoredRecord, input: &DraftInput) -> Result<()> {
    let target = &record
        .versions
        .last()
        .ok_or(GroupMemoryError::CorruptStore)?
        .input;
    if target.category != input.category
        || target.subjects.iter().collect::<BTreeSet<_>>()
            != input.subjects.iter().collect::<BTreeSet<_>>()
    {
        return Err(GroupMemoryError::NotOwner);
    }
    Ok(())
}

fn may_confirm(input: &DraftInput, author: &str) -> bool {
    if input.subjects.is_empty() {
        input
            .evidence
            .iter()
            .any(|source| source.author_id == author)
    } else {
        input.subjects.iter().any(|subject| subject == author)
    }
}

fn confirmation_authority<'a>(
    ledger: &'a Ledger,
    version: &'a StoredDraftRevision,
) -> Result<&'a DraftInput> {
    if let Some(target) = target_id(&version.input) {
        let revision = version
            .target_revision
            .and_then(|revision| revision.checked_sub(1))
            .and_then(|revision| usize::try_from(revision).ok())
            .ok_or(GroupMemoryError::CorruptStore)?;
        let target = ledger
            .records
            .iter()
            .find(|record| record.id == target)
            .ok_or(GroupMemoryError::NotFound)?;
        Ok(&target
            .versions
            .get(revision)
            .ok_or(GroupMemoryError::CorruptStore)?
            .input)
    } else {
        Ok(&version.input)
    }
}

/// A third party can report an inference, but cannot automatically retire an
/// affected member's existing formal record. Preserve it as an unlinked claim.
fn normalize_append_target(ledger: &Ledger, mut input: DraftInput) -> Result<DraftInput> {
    if let Some(id) = target_id(&input) {
        let record = ledger
            .records
            .iter()
            .find(|record| record.id == id)
            .ok_or(GroupMemoryError::NotFound)?;
        validate_target_shape(record, &input)?;
    }
    if matches!(
        input.operation,
        MemoryOperation::Append {
            supersedes: Some(_)
        }
    ) && !automatic_correction_allowed(ledger, &input)
    {
        input.operation = MemoryOperation::Append { supersedes: None };
        if input.certainty == Certainty::Explicit {
            input.certainty = Certainty::Inferred;
        }
    }
    Ok(input)
}

fn automatic_correction_allowed(ledger: &Ledger, input: &DraftInput) -> bool {
    let MemoryOperation::Append {
        supersedes: Some(id),
    } = input.operation
    else {
        return true;
    };
    let Some(target) = ledger
        .records
        .iter()
        .find(|record| record.id == id)
        .and_then(|record| record.versions.last())
    else {
        return false;
    };
    target.input.subjects.is_empty()
        || target.input.subjects.iter().any(|subject| {
            input
                .evidence
                .iter()
                .any(|source| &source.author_id == subject)
        })
}

fn target_revision(ledger: &Ledger, input: &DraftInput) -> Option<u64> {
    let target = match input.operation {
        MemoryOperation::Append { supersedes } => supersedes,
        MemoryOperation::Rewrite { record_id, .. } => Some(record_id),
    };
    target.and_then(|id| {
        ledger
            .records
            .iter()
            .find(|record| record.id == id)
            .map(|record| record.versions.len() as u64)
    })
}

fn target_is_current(ledger: &Ledger, version: &StoredDraftRevision) -> bool {
    target_revision(ledger, &version.input) == version.target_revision
}

fn auto_eligible(input: &DraftInput, now: i64) -> bool {
    input.disposition == Disposition::Retain
        && input.certainty != Certainty::Uncertain
        && matches!(input.operation, MemoryOperation::Append { .. })
        && !expired(input, now)
}

fn draft_index(ledger: &Ledger, id: u64, revision: u64) -> Result<usize> {
    let index = ledger
        .drafts
        .iter()
        .position(|draft| draft.id == id)
        .ok_or(GroupMemoryError::NotFound)?;
    if current(&ledger.drafts[index]).revision != revision {
        return Err(GroupMemoryError::StaleRevision);
    }
    Ok(index)
}

fn invalidate_review(ledger: &mut Ledger, index: usize) {
    if let Some(batch) = ledger.drafts[index].review_batch_id {
        if let Some(review) = ledger.reviews.iter_mut().find(|review| review.id == batch) {
            review.status = ReviewStatus::Cancelled;
        }
        for draft in &mut ledger.drafts {
            if draft.review_batch_id == Some(batch) {
                draft.review_batch_id = None;
                if matches!(
                    draft.status,
                    DraftStatus::NoticePending | DraftStatus::Reviewing
                ) {
                    draft.status = DraftStatus::Buffered;
                }
            }
        }
    }
}

fn same_fact(left: &DraftInput, right: &DraftInput) -> bool {
    left.category == right.category
        && left.subjects.iter().collect::<BTreeSet<_>>()
            == right.subjects.iter().collect::<BTreeSet<_>>()
        && left.content == right.content
        && left.expires_at == right.expires_at
}

fn duplicate_record<'a>(
    ledger: &'a Ledger,
    input: &DraftInput,
    now: i64,
) -> Option<&'a StoredRecord> {
    if !matches!(input.operation, MemoryOperation::Append { .. }) {
        return None;
    }
    ledger.records.iter().find(|record| {
        record_status(ledger, record, now) == RecordStatus::Active
            && record
                .versions
                .last()
                .is_some_and(|live| !expired(&live.input, now) && same_fact(&live.input, input))
    })
}

fn superseded_by(ledger: &Ledger, id: u64) -> Option<u64> {
    ledger.records.iter().rev().find_map(|record| {
        match record
            .versions
            .first()
            .map(|version| &version.input.operation)
        {
            Some(MemoryOperation::Append {
                supersedes: Some(target),
            }) if *target == id => Some(record.id),
            _ => None,
        }
    })
}

fn record_status(ledger: &Ledger, record: &StoredRecord, now: i64) -> RecordStatus {
    if superseded_by(ledger, record.id).is_some() {
        RecordStatus::Superseded
    } else if record
        .versions
        .last()
        .is_some_and(|version| expired(&version.input, now))
    {
        RecordStatus::Expired
    } else {
        RecordStatus::Active
    }
}

fn apply_one(
    store: &GroupMemoryStore,
    ledger: &mut Ledger,
    index: usize,
    now: i64,
    applied_by: AppliedBy,
    explicit: Option<EvidenceInput>,
) -> Result<(u64, bool)> {
    let draft = &ledger.drafts[index];
    let input = current(draft).input.clone();
    if now < draft.updated_at {
        return Err(GroupMemoryError::InvalidInput);
    }
    validate_target(ledger, &input)?;
    if !target_is_current(ledger, current(draft)) {
        return Err(GroupMemoryError::StaleRevision);
    }
    let existing = duplicate_record(ledger, &input, now).map(|record| record.id);
    let duplicate = existing.is_some();
    let id = if let Some(id) = existing {
        id
    } else {
        let id = match input.operation {
            MemoryOperation::Append { .. } => {
                if ledger.records.len() >= MAX_LEDGER_RECORDS {
                    return Err(GroupMemoryError::FactLimit);
                }
                next_id(&store.group_hash, "ledger-record", &mut ledger.next_record)?
            }
            MemoryOperation::Rewrite { record_id, .. } => {
                if !matches!(
                    applied_by,
                    AppliedBy::ExplicitRequest | AppliedBy::OperatorAction
                ) || (applied_by == AppliedBy::ExplicitRequest && explicit.is_none())
                {
                    return Err(GroupMemoryError::NotOwner);
                }
                record_id
            }
        };
        let version = StoredRecordRevision {
            input,
            applied_at: now,
            applied_by,
            draft_id: draft.id,
            draft_revision: current(draft).revision,
            explicit_evidence: explicit,
        };
        if let Some(record) = ledger.records.iter_mut().find(|record| record.id == id) {
            if record.versions.len() >= MAX_FACT_REVISIONS {
                return Err(GroupMemoryError::RevisionLimit);
            }
            if record
                .versions
                .last()
                .is_some_and(|previous| previous.applied_at > now)
            {
                return Err(GroupMemoryError::InvalidInput);
            }
            record.versions.push(version);
        } else {
            ledger.records.push(StoredRecord {
                id,
                versions: vec![version],
            });
        }
        id
    };
    let draft = &mut ledger.drafts[index];
    draft.status = DraftStatus::Applied;
    draft.applied_record_id = Some(id);
    draft.updated_at = now;
    draft.review_batch_id = None;
    Ok((id, duplicate))
}

fn apply_indices(
    store: &GroupMemoryStore,
    ledger: &mut Ledger,
    indices: &[usize],
    now: i64,
    by: AppliedBy,
) -> Result<LedgerReport> {
    let mut report = LedgerReport::default();
    for &index in indices {
        let version = current(&ledger.drafts[index]);
        let target_active = match version.input.operation {
            MemoryOperation::Append {
                supersedes: Some(id),
            } => ledger
                .records
                .iter()
                .find(|record| record.id == id)
                .is_some_and(|record| record_status(ledger, record, now) == RecordStatus::Active),
            _ => true,
        };
        if !target_is_current(ledger, version)
            || !target_active
            || !automatic_correction_allowed(ledger, &version.input)
        {
            report.conflicts += 1;
            continue;
        }
        let (_, duplicate) = apply_one(store, ledger, index, now, by, None)?;
        if duplicate {
            report.duplicates += 1;
        } else {
            report.applied += 1;
        }
        bump(ledger)?;
    }
    report.revision = ledger.revision;
    Ok(report)
}

fn validate_day(day: &str) -> Result<()> {
    if day.len() != 10
        || chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_err()
        || chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
            .is_ok_and(|parsed| parsed.format("%Y-%m-%d").to_string() != day)
    {
        return Err(GroupMemoryError::InvalidInput);
    }
    Ok(())
}

fn validate_error_code(code: &str) -> Result<()> {
    if code.is_empty()
        || code.len() > 80
        || !code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(GroupMemoryError::InvalidInput);
    }
    Ok(())
}

impl Ledger {
    pub(super) fn is_empty(&self) -> bool {
        self.revision == 0
            && self.next_draft == 1
            && self.next_record == 1
            && self.next_review == 1
            && self.drafts.is_empty()
            && self.records.is_empty()
            && self.reviews.is_empty()
            && self.maintenance == MaintenanceState::default()
    }

    pub(super) fn validate(&self, store: &GroupMemoryStore) -> Result<()> {
        self.validate_inner(store)
            .map_err(|_| GroupMemoryError::CorruptStore)
    }

    fn validate_inner(&self, store: &GroupMemoryStore) -> Result<()> {
        if self.next_draft == 0
            || self.next_record == 0
            || self.next_review == 0
            || self.drafts.len() > MAX_LEDGER_DRAFTS
            || self.records.len() > MAX_LEDGER_RECORDS
            || self.reviews.len() > MAX_REVIEW_BATCHES
        {
            return Err(GroupMemoryError::CorruptStore);
        }
        if self.next_draft != self.drafts.len() as u64 + 1
            || self.next_record != self.records.len() as u64 + 1
            || self.next_review != self.reviews.len() as u64 + 1
        {
            return Err(GroupMemoryError::CorruptStore);
        }
        if let Some(day) = &self.maintenance.last_completed_day {
            validate_day(day)?;
        }
        if let Some(code) = &self.maintenance.last_error {
            validate_error_code(code)?;
        }
        if self.maintenance.updated_at.is_some_and(|time| time < 0) {
            return Err(GroupMemoryError::CorruptStore);
        }
        let mut ids = BTreeSet::new();
        for draft in &self.drafts {
            if draft.id == 0
                || !ids.insert(draft.id)
                || draft.versions.is_empty()
                || draft.versions.len() > MAX_FACT_REVISIONS
                || draft.created_at < 0
                || draft.updated_at < draft.created_at
            {
                return Err(GroupMemoryError::CorruptStore);
            }
            let mut previous = draft.created_at;
            for (index, version) in draft.versions.iter().enumerate() {
                validate_input(&version.input)?;
                if version.revision != index as u64 + 1
                    || version.changed_at < previous
                    || version.changed_at > draft.updated_at
                    || version
                        .input
                        .expires_at
                        .is_some_and(|expiry| expiry <= version.changed_at)
                {
                    return Err(GroupMemoryError::CorruptStore);
                }
                // Target existence is checked here; stale rewrite drafts remain
                // readable after another explicit rewrite, but cannot be applied.
                let target = match version.input.operation {
                    MemoryOperation::Append { supersedes } => supersedes,
                    MemoryOperation::Rewrite { record_id, .. } => Some(record_id),
                };
                if target.is_some_and(|id| !self.records.iter().any(|record| record.id == id)) {
                    return Err(GroupMemoryError::CorruptStore);
                }
                if let Some(id) = target {
                    validate_target_shape(
                        self.records
                            .iter()
                            .find(|record| record.id == id)
                            .ok_or(GroupMemoryError::CorruptStore)?,
                        &version.input,
                    )?;
                }
                if target.is_some() != version.target_revision.is_some()
                    || version.target_revision.is_some_and(|revision| {
                        revision == 0
                            || target_revision(self, &version.input)
                                .is_none_or(|current| revision > current)
                    })
                    || matches!(version.input.operation,MemoryOperation::Rewrite{revision,..} if Some(revision)!=version.target_revision)
                {
                    return Err(GroupMemoryError::CorruptStore);
                }
                previous = version.changed_at;
            }
            match draft.status {
                DraftStatus::Applied => {
                    if draft.review_batch_id.is_some()
                        || !draft.applied_record_id.is_some_and(|id| {
                            self.records.iter().any(|record| {
                                record.id == id
                                    && record.versions.iter().any(|version| {
                                        same_fact(&version.input, &current(draft).input)
                                    })
                            })
                        })
                    {
                        return Err(GroupMemoryError::CorruptStore);
                    }
                }
                DraftStatus::Buffered | DraftStatus::Rejected => {
                    if draft.review_batch_id.is_some() || draft.applied_record_id.is_some() {
                        return Err(GroupMemoryError::CorruptStore);
                    }
                }
                DraftStatus::NoticePending | DraftStatus::Reviewing => {
                    if draft.applied_record_id.is_some()
                        || !draft.review_batch_id.is_some_and(|id| {
                            self.reviews.iter().any(|review| {
                                review.id == id
                                    && review.drafts.contains(&DraftRef {
                                        id: draft.id,
                                        revision: current(draft).revision,
                                    })
                                    && review.status
                                        == if draft.status == DraftStatus::NoticePending {
                                            ReviewStatus::Pending
                                        } else {
                                            ReviewStatus::Sent
                                        }
                            })
                        })
                    {
                        return Err(GroupMemoryError::CorruptStore);
                    }
                }
            }
        }
        let mut record_ids = BTreeSet::new();
        for record in &self.records {
            if record.id == 0
                || !record_ids.insert(record.id)
                || record.versions.is_empty()
                || record.versions.len() > MAX_FACT_REVISIONS
            {
                return Err(GroupMemoryError::CorruptStore);
            }
            let mut previous = 0;
            for (index, version) in record.versions.iter().enumerate() {
                validate_input(&version.input)?;
                if version.applied_at < previous
                    || version.input.disposition != Disposition::Retain
                    || expired(&version.input, version.applied_at)
                {
                    return Err(GroupMemoryError::CorruptStore);
                }
                let draft = self
                    .drafts
                    .iter()
                    .find(|draft| draft.id == version.draft_id)
                    .ok_or(GroupMemoryError::CorruptStore)?;
                let source = draft
                    .versions
                    .iter()
                    .find(|source| source.revision == version.draft_revision)
                    .ok_or(GroupMemoryError::CorruptStore)?;
                if source.input != version.input
                    || draft.status != DraftStatus::Applied
                    || draft.applied_record_id != Some(record.id)
                {
                    return Err(GroupMemoryError::CorruptStore);
                }
                match version.input.operation {
                    MemoryOperation::Rewrite {
                        record_id,
                        revision,
                    } => {
                        if index == 0
                            || record_id != record.id
                            || revision != index as u64
                            || !matches!(
                                version.applied_by,
                                AppliedBy::ExplicitRequest | AppliedBy::OperatorAction
                            )
                        {
                            return Err(GroupMemoryError::CorruptStore);
                        }
                    }
                    MemoryOperation::Append { .. } => {
                        if index != 0 {
                            return Err(GroupMemoryError::CorruptStore);
                        }
                    }
                }
                match version.applied_by {
                    AppliedBy::ExplicitRequest => {
                        let evidence = version
                            .explicit_evidence
                            .as_ref()
                            .ok_or(GroupMemoryError::CorruptStore)?;
                        validate_evidence(evidence)?;
                        let authority = confirmation_authority(self, source)?;
                        if evidence.timestamp > version.applied_at
                            || !may_confirm(authority, &evidence.author_id)
                        {
                            return Err(GroupMemoryError::CorruptStore);
                        }
                    }
                    AppliedBy::OperatorAction => {
                        if version.explicit_evidence.is_some() {
                            return Err(GroupMemoryError::CorruptStore);
                        }
                    }
                    AppliedBy::ScheduledArchive | AppliedBy::SilenceGrace => {
                        if version.explicit_evidence.is_some()
                            || !auto_eligible(&version.input, version.applied_at)
                        {
                            return Err(GroupMemoryError::CorruptStore);
                        }
                        if version.applied_by == AppliedBy::SilenceGrace
                            && !self.reviews.iter().any(|review| {
                                review.status == ReviewStatus::Completed
                                    && review
                                        .deadline
                                        .is_some_and(|deadline| deadline <= version.applied_at)
                                    && review.ack_hash.is_some()
                                    && review.drafts.contains(&DraftRef {
                                        id: version.draft_id,
                                        revision: version.draft_revision,
                                    })
                            })
                        {
                            return Err(GroupMemoryError::CorruptStore);
                        }
                    }
                }
                previous = version.applied_at;
            }
        }
        let mut review_ids = BTreeSet::new();
        for review in &self.reviews {
            if review.id == 0
                || !review_ids.insert(review.id)
                || review.drafts.is_empty()
                || review.drafts.len() > MAX_REVIEW_DRAFTS
                || review
                    .drafts
                    .iter()
                    .map(|draft| draft.id)
                    .collect::<BTreeSet<_>>()
                    .len()
                    != review.drafts.len()
                || review.created_at < 0
                || review.grace_secs > MAX_GRACE_SECS
                || review.notice.chars().count() > 4000
            {
                return Err(GroupMemoryError::CorruptStore);
            }
            let mut expected =
                String::from("准备整理以下记忆草稿；送达后进入公示期，可在期限内撤回：\n");
            for reference in &review.drafts {
                let draft = self
                    .drafts
                    .iter()
                    .find(|draft| draft.id == reference.id)
                    .ok_or(GroupMemoryError::CorruptStore)?;
                let version = draft
                    .versions
                    .iter()
                    .find(|version| version.revision == reference.revision)
                    .ok_or(GroupMemoryError::CorruptStore)?;
                if !auto_eligible(&version.input, review.created_at) {
                    return Err(GroupMemoryError::CorruptStore);
                }
                expected.push_str(&format!(
                    "\n{} v{}：{}\n撤回：/memory reject {} {}\n",
                    reference.id,
                    reference.revision,
                    version.input.content,
                    reference.id,
                    reference.revision
                ));
                if matches!(review.status, ReviewStatus::Pending | ReviewStatus::Sent)
                    && (draft.review_batch_id != Some(review.id)
                        || current(draft).revision != reference.revision
                        || draft.status
                            != if review.status == ReviewStatus::Pending {
                                DraftStatus::NoticePending
                            } else {
                                DraftStatus::Reviewing
                            })
                {
                    return Err(GroupMemoryError::CorruptStore);
                }
            }
            if expected != review.notice {
                return Err(GroupMemoryError::CorruptStore);
            }
            match (review.sent_at, review.deadline, review.ack_hash.as_deref()) {
                (None, None, None) => {
                    if matches!(review.status, ReviewStatus::Sent | ReviewStatus::Completed) {
                        return Err(GroupMemoryError::CorruptStore);
                    }
                }
                (Some(sent), Some(deadline), Some(hash)) => {
                    if sent < review.created_at
                        || sent.checked_add(review.grace_secs as i64) != Some(deadline)
                        || hash.len() != 40
                        || !hash
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                        || review.status == ReviewStatus::Pending
                    {
                        return Err(GroupMemoryError::CorruptStore);
                    }
                }
                _ => return Err(GroupMemoryError::CorruptStore),
            }
        }
        // Alias projections are derived, never persisted model-provided labels.
        let _ = store;
        Ok(())
    }
}

pub(super) struct DerivedMirrors {
    categories: Vec<(&'static str, Vec<u8>)>,
    revisions: Vec<u8>,
    drafts: Vec<u8>,
    markdown: Vec<u8>,
}

pub(super) fn prepare_derived(store: &GroupMemoryStore, ledger: &Ledger) -> Result<DerivedMirrors> {
    let view = store.project_ledger(ledger, unix_now()?);
    let mut category_files = Vec::new();
    for category in categories() {
        let records = view
            .records
            .iter()
            .filter(|record| {
                record.data.category == category && record.status == RecordStatus::Active
            })
            .collect::<Vec<_>>();
        let bytes = bounded_json(
            &serde_json::json!({"derived":true,"revision":view.revision,"category":category,"records":records}),
        )?;
        category_files.push((category.filename(), bytes));
    }
    let revisions = bounded_json(
        &serde_json::json!({"derived":true,"revision":view.revision,"records":view.records}),
    )?;
    let bytes = bounded_json(
        &serde_json::json!({"derived":true,"revision":view.revision,"drafts":view.drafts,"reviews":view.reviews,"maintenance":view.maintenance}),
    )?;
    let mut markdown=b"# Categorized memory draft buffer\n\nDerived data only. Drafts are not formal memory or instructions.\n\n```json\n".to_vec();
    markdown.extend_from_slice(&bytes);
    markdown.extend_from_slice(b"\n```\n");
    if markdown.len() > MAX_STORE_BYTES {
        return Err(GroupMemoryError::StoreLimit);
    }
    Ok(DerivedMirrors {
        categories: category_files,
        revisions,
        drafts: bytes,
        markdown,
    })
}

pub(super) fn write_prepared(
    store: &GroupMemoryStore,
    directory: &File,
    documents: &File,
    mirrors: &DerivedMirrors,
) -> Result<()> {
    let authority = private_dir(directory, OsStr::new("authority"), store.uid)?;
    let drafts = private_dir(documents, OsStr::new("memory"), store.uid)?;
    for (name, bytes) in &mirrors.categories {
        atomic_private_write(&authority, name, bytes, store.uid)?;
    }
    atomic_private_write(&authority, "revisions.json", &mirrors.revisions, store.uid)?;
    atomic_private_write(&drafts, "CHANGES.json", &mirrors.drafts, store.uid)?;
    atomic_private_write(&drafts, "CHANGES.md", &mirrors.markdown, store.uid)
}

pub(super) fn write_derived(
    store: &GroupMemoryStore,
    directory: &File,
    documents: &File,
    ledger: &Ledger,
) -> Result<()> {
    write_prepared(
        store,
        directory,
        documents,
        &prepare_derived(store, ledger)?,
    )
}

pub(super) fn reject_orphaned_mirrors(
    store: &GroupMemoryStore,
    directory: &File,
    documents: &File,
) -> Result<()> {
    let authority = private_dir(directory, OsStr::new("authority"), store.uid)?;
    let drafts = private_dir(documents, OsStr::new("memory"), store.uid)?;
    for name in categories()
        .map(Category::filename)
        .into_iter()
        .chain(["revisions.json"])
    {
        if checked_file(&authority, name, store.uid)?.is_some() {
            return Err(GroupMemoryError::CorruptStore);
        }
    }
    for name in ["CHANGES.json", "CHANGES.md"] {
        if checked_file(&drafts, name, store.uid)?.is_some() {
            return Err(GroupMemoryError::CorruptStore);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn setup() -> (tempfile::TempDir, GroupMemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupMemoryStore::new(dir.path(), "synthetic-ledger-group").unwrap();
        (dir, store)
    }

    fn evidence(seq: u64, author: &str) -> EvidenceInput {
        EvidenceInput {
            message_id: format!("synthetic-message-{seq}"),
            message_ref: format!("M{seq}"),
            author_id: author.to_owned(),
            timestamp: 90,
        }
    }

    fn input(seq: u64, text: &str) -> DraftInput {
        DraftInput {
            category: Category::Profile,
            subjects: vec!["synthetic-member-a".into()],
            content: text.into(),
            disposition: Disposition::Retain,
            selection_reason: "A durable self-reported preference.".into(),
            source_kind: SourceKind::Automatic,
            certainty: Certainty::Explicit,
            evidence: vec![evidence(seq, "synthetic-member-a")],
            expires_at: None,
            operation: MemoryOperation::Append { supersedes: None },
        }
    }

    fn raw_path(store: &GroupMemoryStore) -> PathBuf {
        store.data_dir.join(store.memory_layout().canonical)
    }

    #[test]
    fn legacy_self_facts_remain_untouched_and_ledger_starts_empty() {
        let (_dir, store) = setup();
        let before: serde_json::Value =
            serde_json::from_slice(&fs::read(raw_path(&store)).unwrap()).unwrap();
        assert!(before.get("ledger").is_none());
        let old = store
            .propose(
                "synthetic-member-a",
                "legacy preference",
                None,
                "legacy-message",
                90,
            )
            .unwrap();
        store
            .confirm("synthetic-member-a", old.change_id, 1)
            .unwrap();
        store.maintain(100).unwrap();
        let old_facts = store.confirmed("synthetic-member-a").unwrap();
        store.buffer(input(1, "new preference"), 101).unwrap();
        store.archive_ready(102).unwrap();
        assert_eq!(store.confirmed("synthetic-member-a").unwrap(), old_facts);
        assert_eq!(store.summary().unwrap().authority_revision, 1);
        let view = store.ledger_view().unwrap();
        assert_eq!(view.records.len(), 1);
        assert_eq!(
            view.records[0].history[0].applied_by,
            AppliedBy::ScheduledArchive
        );
        let projection = serde_json::to_string(&view).unwrap();
        assert!(!projection.contains("synthetic-member-a"));
        assert!(!projection.contains("synthetic-message-1"));
        assert!(projection.contains("M1"));
    }

    #[test]
    fn no_notice_archive_filters_disposition_uncertainty_expiry_and_rewrites() {
        let (_dir, store) = setup();
        store.buffer(input(1, "durable"), 100).unwrap();
        let mut context = input(2, "temporary context");
        context.disposition = Disposition::ContextOnly;
        store.buffer(context, 100).unwrap();
        let mut discard = input(3, "not selected");
        discard.disposition = Disposition::Discard;
        store.buffer(discard, 100).unwrap();
        let mut uncertain = input(4, "maybe true");
        uncertain.certainty = Certainty::Uncertain;
        store.buffer(uncertain, 100).unwrap();
        let mut expired = input(5, "expires before archive");
        expired.expires_at = Some(102);
        store.buffer(expired, 100).unwrap();
        let report = store.archive_ready(103).unwrap();
        assert_eq!(report.applied, 1);
        let record = store.ledger_view().unwrap().records.remove(0);
        let correction = store
            .propose_record_correction(record.id, record.revision, "corrected", 104)
            .unwrap();
        assert_eq!(store.archive_ready(105).unwrap().applied, 0);
        assert_eq!(
            store
                .ledger_view()
                .unwrap()
                .drafts
                .iter()
                .find(|draft| draft.id == correction.id)
                .unwrap()
                .status,
            DraftStatus::Buffered
        );
    }

    #[test]
    fn notices_require_positive_ack_and_exact_grace_deadline() {
        let (_dir, store) = setup();
        let draft = store.buffer(input(1, "notice actual text"), 100).unwrap();
        let review = store.prepare_review(&[(draft.id, 1)], 60, 101).unwrap();
        assert!(review.notice.contains("notice actual text"));
        assert_eq!(store.archive_due(1000).unwrap().applied, 0);
        assert_eq!(store.archive_ready(1000).unwrap().applied, 0);
        assert_eq!(
            store.mark_review_sent(review.id, "", 102),
            Err(GroupMemoryError::InvalidInput)
        );
        let delivered = store
            .mark_review_sent(review.id, "synthetic-qq-ack", 105)
            .unwrap();
        assert_eq!(delivered.deadline, Some(165));
        assert_eq!(store.archive_due(164).unwrap().applied, 0);
        assert_eq!(store.archive_due(165).unwrap().applied, 1);
        assert_eq!(store.archive_due(166).unwrap().applied, 0);
        assert_eq!(
            store
                .mark_review_sent(review.id, "synthetic-qq-ack", 105)
                .unwrap()
                .status,
            ReviewStatus::Completed
        );
    }

    #[test]
    fn edit_or_rejection_invalidates_frozen_review_for_all_members() {
        let (_dir, store) = setup();
        let first = store.buffer(input(1, "first"), 100).unwrap();
        let second = store.buffer(input(2, "second"), 100).unwrap();
        let review = store
            .prepare_review(&[(first.id, 1), (second.id, 1)], 60, 101)
            .unwrap();
        store
            .mark_review_sent(review.id, "synthetic-ack", 102)
            .unwrap();
        let edited = store
            .edit_content(first.id, 1, "edited first", 103)
            .unwrap();
        assert_eq!(edited.revision, 2);
        assert_eq!(edited.history[0].data.content, "first");
        assert_eq!(store.archive_due(1000).unwrap().applied, 0);
        let view = store.ledger_view().unwrap();
        assert_eq!(view.reviews[0].status, ReviewStatus::Cancelled);
        assert!(
            view.drafts
                .iter()
                .all(|draft| draft.status == DraftStatus::Buffered
                    && draft.review_batch_id.is_none())
        );
        let fresh = store.prepare_review(&[(first.id, 2)], 1, 104).unwrap();
        store.reject_draft(first.id, 2, 105).unwrap();
        assert_eq!(
            store.mark_review_sent(fresh.id, "late-ack", 106),
            Err(GroupMemoryError::NotPending)
        );
        assert_eq!(
            store.buffer(input(1, "first"), 107).unwrap().status,
            DraftStatus::Rejected
        );
    }

    #[test]
    fn followup_qq_confirmation_is_independent_evidence_with_actual_identity() {
        let (_dir, store) = setup();
        let draft = store.buffer(input(1, "original statement"), 100).unwrap();
        assert_eq!(
            store.apply_explicit(draft.id, 1, evidence(2, "synthetic-outsider"), 101),
            Err(GroupMemoryError::NotOwner)
        );
        let record = store
            .apply_explicit(draft.id, 1, evidence(3, "synthetic-member-a"), 102)
            .unwrap();
        assert_eq!(record.history[0].applied_by, AppliedBy::ExplicitRequest);
        assert_eq!(
            record.history[0].confirmation.as_ref().unwrap().message_ref,
            "M3"
        );
        assert_eq!(record.data.evidence[0].message_ref, "M1");
    }

    #[test]
    fn operator_rewrite_is_explicit_cas_and_keeps_previous_text_and_sources() {
        let (_dir, store) = setup();
        store.buffer(input(1, "old fact"), 100).unwrap();
        store.archive_ready(101).unwrap();
        let record = store.ledger_view().unwrap().records.remove(0);
        let rewrite = store
            .propose_record_correction(record.id, 1, "new fact", 102)
            .unwrap();
        let stale = store
            .propose_record_correction(record.id, 1, "stale other correction", 102)
            .unwrap();
        assert!(rewrite.created_by_operator);
        assert_eq!(store.archive_ready(103).unwrap().applied, 0);
        let updated = store.apply_operator(rewrite.id, 1, 104).unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.history[0].data.content, "old fact");
        assert_eq!(updated.history[1].data.content, "new fact");
        assert_eq!(updated.history[1].applied_by, AppliedBy::OperatorAction);
        assert!(updated.history[1].confirmation.is_none());
        assert_eq!(
            store.apply_operator(stale.id, 1, 105),
            Err(GroupMemoryError::StaleRevision)
        );
        assert_eq!(store.ledger_view().unwrap().records[0].revision, 2);
        let mut append = input(2, "correction as a separate record");
        append.operation = MemoryOperation::Append {
            supersedes: Some(record.id),
        };
        store.buffer(append, 106).unwrap();
        assert_eq!(store.archive_ready(107).unwrap().applied, 1);
        assert_eq!(store.ledger_view().unwrap().records.len(), 2);
    }

    #[test]
    fn duplicate_records_do_not_gain_versions_and_expired_records_never_inject() {
        let (_dir, store) = setup();
        let mut original = input(1, "expires later");
        original.expires_at = Some(200);
        let draft = store.buffer(original.clone(), 100).unwrap();
        store.archive_ready(101).unwrap();
        let mut duplicate = original;
        duplicate.evidence = vec![evidence(2, "synthetic-member-a")];
        assert_eq!(store.buffer(duplicate, 102).unwrap().id, draft.id);
        assert_eq!(store.ledger_view().unwrap().records[0].revision, 1);
        assert_eq!(
            store
                .inject_ledger_for(&["synthetic-member-a"], 8192, 199)
                .unwrap()
                .facts,
            1
        );
        assert_eq!(
            store
                .inject_ledger_for(&["synthetic-member-a"], 8192, 200)
                .unwrap()
                .facts,
            0
        );
        assert_eq!(
            store
                .inject_ledger_for(&["unrelated-member"], 8192, 199)
                .unwrap()
                .facts,
            0
        );
        assert_eq!(
            store
                .inject_ledger_for(&["synthetic-member-a"], 2, 199)
                .unwrap()
                .json,
            ""
        );
    }

    #[test]
    fn superseding_correction_expires_without_resurrecting_old_fact() {
        let (_dir, store) = setup();
        store.buffer(input(1, "old preference"), 100).unwrap();
        store.archive_ready(101).unwrap();
        let old = store.ledger_view_at(101).unwrap().records.remove(0);
        let mut correction = input(2, "corrected preference");
        correction.operation = MemoryOperation::Append {
            supersedes: Some(old.id),
        };
        correction.expires_at = Some(200);
        store.buffer(correction, 102).unwrap();
        store.archive_ready(103).unwrap();
        let view = store.ledger_view_at(104).unwrap();
        assert_eq!(view.records[0].status, RecordStatus::Superseded);
        assert_eq!(view.records[0].superseded_by, Some(view.records[1].id));
        assert_eq!(
            store
                .inject_ledger_for(&["synthetic-member-a"], 8192, 199)
                .unwrap()
                .facts,
            1
        );
        assert_eq!(
            store
                .inject_ledger_for(&["synthetic-member-a"], 8192, 200)
                .unwrap()
                .facts,
            0
        );
        let view = store.ledger_view_at(201).unwrap();
        assert_eq!(view.records[0].status, RecordStatus::Superseded);
        assert_eq!(view.records[1].status, RecordStatus::Expired);
    }

    #[test]
    fn extraction_snapshot_cannot_supersede_a_concurrently_rewritten_record() {
        let (_dir, store) = setup();
        store
            .commit_capture(0, 10, vec![input(1, "old fact")], 100)
            .unwrap();
        let snapshot = store.ledger_view_at(101).unwrap();
        let record = &snapshot.records[0];
        let correction = store
            .propose_record_correction(record.id, record.revision, "operator update", 102)
            .unwrap();
        store
            .apply_operator(correction.id, correction.revision, 103)
            .unwrap();
        let before = fs::read(raw_path(&store)).unwrap();
        let mut stale = input(2, "model obsolete correction");
        stale.operation = MemoryOperation::Append {
            supersedes: Some(record.id),
        };
        assert_eq!(
            store.commit_capture_at_revision(10, snapshot.revision, 20, vec![stale], 104),
            Err(GroupMemoryError::StaleRevision)
        );
        assert_eq!(fs::read(raw_path(&store)).unwrap(), before);
        assert_eq!(
            store.ledger_view_at(104).unwrap().records[0].data.content,
            "operator update"
        );
    }

    #[test]
    fn buffered_correction_cannot_supersede_a_later_explicit_revision() {
        let (_dir, store) = setup();
        store.buffer(input(1, "old formal"), 100).unwrap();
        store.archive_ready(101).unwrap();
        let old = store.ledger_view_at(101).unwrap().records.remove(0);
        let mut correction = input(2, "old snapshot correction");
        correction.operation = MemoryOperation::Append {
            supersedes: Some(old.id),
        };
        let pending = store.buffer(correction, 102).unwrap();
        assert_eq!(pending.target_revision, Some(1));
        let rewrite = store
            .propose_record_correction(old.id, 1, "explicit new formal", 103)
            .unwrap();
        store.apply_operator(rewrite.id, 1, 104).unwrap();
        store
            .buffer(input(3, "unrelated eligible fact"), 105)
            .unwrap();
        let report = store.archive_ready(106).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(report.conflicts, 1);
        let view = store.ledger_view_at(106).unwrap();
        assert_eq!(view.records[0].data.content, "explicit new formal");
        assert_eq!(view.records[0].status, RecordStatus::Active);
        assert_eq!(
            view.drafts
                .iter()
                .find(|draft| draft.id == pending.id)
                .unwrap()
                .status,
            DraftStatus::Buffered
        );
    }

    #[test]
    fn replacement_cannot_change_target_subjects_or_category_even_for_operator() {
        let (_dir, store) = setup();
        let mut bob = input(1, "Bob's formal preference");
        bob.subjects = vec!["synthetic-member-b".into()];
        bob.evidence[0].author_id = "synthetic-member-b".into();
        store.buffer(bob, 100).unwrap();
        store.archive_ready(101).unwrap();
        let record = store.ledger_view_at(101).unwrap().records.remove(0);
        let mut attack = input(2, "Alice-controlled replacement");
        attack.operation = MemoryOperation::Rewrite {
            record_id: record.id,
            revision: 1,
        };
        assert_eq!(
            store.buffer(attack.clone(), 102),
            Err(GroupMemoryError::NotOwner)
        );
        attack.operation = MemoryOperation::Append {
            supersedes: Some(record.id),
        };
        assert_eq!(
            store.buffer(attack.clone(), 102),
            Err(GroupMemoryError::NotOwner)
        );
        attack.subjects = vec!["synthetic-member-b".into()];
        attack.category = Category::Relationship;
        assert_eq!(store.buffer(attack, 102), Err(GroupMemoryError::NotOwner));
        let correction = store
            .propose_record_correction(record.id, 1, "operator correction", 103)
            .unwrap();
        assert_eq!(
            store
                .apply_operator(correction.id, 1, 104)
                .unwrap()
                .data
                .subjects,
            record.data.subjects
        );
    }

    #[test]
    fn proposed_evidence_author_cannot_self_confirm_another_subjects_fact_or_rewrite() {
        let (_dir, store) = setup();
        let mut report = input(1, "Alice's report about Bob");
        report.subjects = vec!["synthetic-member-b".into()];
        let draft = store.buffer(report, 100).unwrap();
        assert_eq!(
            store.apply_explicit(draft.id, 1, evidence(2, "synthetic-member-a"), 101),
            Err(GroupMemoryError::NotOwner)
        );
        let bob = store
            .apply_explicit(draft.id, 1, evidence(3, "synthetic-member-b"), 102)
            .unwrap();
        let mut rewrite = input(4, "Alice's proposed rewrite of Bob");
        rewrite.subjects = vec!["synthetic-member-b".into()];
        rewrite.operation = MemoryOperation::Rewrite {
            record_id: bob.id,
            revision: 1,
        };
        let draft = store.buffer(rewrite, 103).unwrap();
        assert_eq!(
            store.apply_explicit(draft.id, 1, evidence(5, "synthetic-member-a"), 104),
            Err(GroupMemoryError::NotOwner)
        );
        assert_eq!(
            store.ledger_view_at(104).unwrap().records[0].data.content,
            "Alice's report about Bob"
        );
    }

    #[test]
    fn third_party_correction_is_retained_as_inferred_without_retiring_the_target() {
        let (_dir, store) = setup();
        let mut bob = input(1, "Bob's original preference");
        bob.subjects = vec!["synthetic-member-b".into()];
        bob.evidence[0].author_id = "synthetic-member-b".into();
        store.buffer(bob, 100).unwrap();
        store.archive_ready(101).unwrap();
        let record = store.ledger_view_at(101).unwrap().records.remove(0);
        let mut third_party = input(2, "Alice says Bob now prefers something else");
        third_party.subjects = vec!["synthetic-member-b".into()];
        third_party.source_kind = SourceKind::UserRequested;
        third_party.operation = MemoryOperation::Append {
            supersedes: Some(record.id),
        };
        let draft = store.buffer(third_party, 102).unwrap();
        assert_eq!(
            draft.data.operation,
            MemoryOperation::Append { supersedes: None }
        );
        assert_eq!(draft.data.certainty, Certainty::Inferred);
        assert_eq!(draft.target_revision, None);
        assert_eq!(store.archive_ready(103).unwrap().applied, 1);
        let view = store.ledger_view_at(103).unwrap();
        assert_eq!(view.records.len(), 2);
        assert_eq!(view.records[0].status, RecordStatus::Active);
        assert!(view.records[0].superseded_by.is_none());
    }

    #[test]
    fn commit_capture_has_one_cas_boundary_and_failed_capture_cannot_advance() {
        let (_dir, store) = setup();
        store
            .commit_capture(0, 10, vec![input(1, "first captured")], 100)
            .unwrap();
        assert_eq!(
            store.commit_capture(0, 20, vec![input(2, "stale interpretation")], 101),
            Err(GroupMemoryError::StaleRevision)
        );
        let before = fs::read(raw_path(&store)).unwrap();
        let mut invalid = input(3, "invalid rewrite");
        invalid.operation = MemoryOperation::Rewrite {
            record_id: 111,
            revision: 1,
        };
        assert_eq!(
            store.commit_capture(
                10,
                20,
                vec![input(2, "valid until later failure"), invalid],
                102
            ),
            Err(GroupMemoryError::NotFound)
        );
        assert_eq!(fs::read(raw_path(&store)).unwrap(), before);
        assert_eq!(store.ledger_view().unwrap().maintenance.history_cursor, 10);
        store.complete_day(10, "2026-10-24", 103).unwrap();
        assert_eq!(
            store.complete_day(9, "2026-10-25", 104),
            Err(GroupMemoryError::StaleRevision)
        );
        assert_eq!(
            store.complete_day(10, "not-a-date", 104),
            Err(GroupMemoryError::InvalidInput)
        );
        assert_eq!(
            store.record_scan_error("raw error with data", 104),
            Err(GroupMemoryError::InvalidInput)
        );
    }

    #[test]
    fn corrupted_canonical_metadata_is_rejected_before_injection_or_mirror_repair() {
        let (_dir, store) = setup();
        store.buffer(input(1, "data"), 100).unwrap();
        let path = raw_path(&store);
        let original = fs::read(&path).unwrap();
        let mut data: serde_json::Value = serde_json::from_slice(&original).unwrap();
        data["ledger"]["drafts"][0]["versions"][0]["input"]["evidence"][0]["message_ref"] =
            serde_json::json!("M0001");
        let corrupt = serde_json::to_vec(&data).unwrap();
        fs::write(&path, &corrupt).unwrap();
        assert_eq!(store.ledger_view(), Err(GroupMemoryError::CorruptStore));
        assert_eq!(
            store.inject_ledger_for(&["synthetic-member-a"], 8192, 101),
            Err(GroupMemoryError::CorruptStore)
        );
        assert_eq!(fs::read(path).unwrap(), corrupt);
    }

    #[test]
    fn category_mirrors_rebuild_safely_and_new_orphans_do_not_reset_truth() {
        let (_dir, store) = setup();
        let layout = store.memory_layout();
        assert!(
            layout
                .categories
                .iter()
                .all(|file| !Path::new(&file.path).is_absolute() && !file.path.contains(".."))
        );
        let category = store.data_dir.join(&layout.categories[0].path);
        assert_eq!(
            fs::metadata(&category).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(&category, b"edited mirror").unwrap();
        store.ledger_view().unwrap();
        assert_ne!(fs::read(&category).unwrap(), b"edited mirror");
        fs::remove_file(&category).unwrap();
        let outside = store.data_dir.join("outside-safe-target");
        fs::write(&outside, b"keep").unwrap();
        symlink(&outside, &category).unwrap();
        assert_eq!(store.ledger_view(), Err(GroupMemoryError::UnsafePath));
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
        fs::remove_file(&category).unwrap();
        store.ledger_view().unwrap();
        fs::remove_file(raw_path(&store)).unwrap();
        let root = store
            .data_dir
            .join(format!("memory/groups/G{}", store.group_hash));
        for path in [
            root.join("authority.json"),
            root.join("documents/changes.json"),
            root.join("documents/CHANGES.md"),
        ] {
            fs::remove_file(path).unwrap();
        }
        assert!(matches!(
            GroupMemoryStore::new(&store.data_dir, "synthetic-ledger-group"),
            Err(GroupMemoryError::CorruptStore)
        ));
        assert!(!raw_path(&store).exists());
    }
}
