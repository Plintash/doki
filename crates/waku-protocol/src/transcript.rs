//! Explicit, bounded transcript reads. Pages are observations of current state,
//! not immutable snapshots: a streaming field can change between calls.
use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ts_rs::TS;
use uuid::Uuid;

use crate::model::{ActivityItem, AgentSession, AgentTurn, Message};

pub const MAX_TRANSCRIPT_ITEMS: usize = 100;
pub const TRANSCRIPT_PREVIEW_BYTES: usize = 256;
pub const MAX_TRANSCRIPT_TEXT_BYTES: usize = 64 * 1024;

fn default_limit() -> usize {
    50
}
fn default_max_bytes() -> usize {
    32 * 1024
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TranscriptCollection {
    Messages,
    Activities,
    Turns,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum TranscriptQuery {
    Index {
        collection: TranscriptCollection,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_limit")]
        limit: usize,
    },
    Text {
        collection: TranscriptCollection,
        item_id: Uuid,
        /// A text leaf or bounded metadata path advertised by the index.
        field: String,
        #[serde(default)]
        offset_bytes: usize,
        #[serde(default = "default_max_bytes")]
        max_bytes: usize,
    },
}

impl TranscriptQuery {
    pub fn collection(&self) -> TranscriptCollection {
        match self {
            Self::Index { collection, .. } | Self::Text { collection, .. } => *collection,
        }
    }
    pub fn validate(&self) -> Result<(), TranscriptError> {
        match self {
            Self::Index { offset, limit, .. } => {
                if !(1..=MAX_TRANSCRIPT_ITEMS).contains(limit) || *offset > i64::MAX as usize {
                    return Err(TranscriptError::invalid(
                        "index limit must be 1..100 and offset must fit a signed 64-bit integer",
                    ));
                }
            }
            Self::Text {
                collection,
                field,
                offset_bytes,
                max_bytes,
                ..
            } => {
                if !(4..=MAX_TRANSCRIPT_TEXT_BYTES).contains(max_bytes)
                    || *offset_bytes > i64::MAX as usize
                {
                    return Err(TranscriptError::invalid(
                        "text maxBytes must be 4..65536 and offsetBytes must fit a signed 64-bit integer",
                    ));
                }
                if !valid_field_path(*collection, field) {
                    return Err(TranscriptError::new(
                        "invalid_field",
                        "field is not supported for this collection; use names returned by the index",
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TranscriptSource {
    Live,
    Persisted,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TranscriptEncoding {
    Text,
    Json,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptField {
    pub field: String,
    pub encoding: TranscriptEncoding,
    /// Null for structured fields: indexing never serializes large collections.
    pub total_bytes: Option<usize>,
    pub preview: Option<String>,
    pub preview_truncated: bool,
    pub item_count: Option<usize>,
    /// Start here, then replace offsetBytes with the returned nextOffsetBytes.
    pub query: Option<TranscriptQuery>,
    /// Replace {index}/{span} with canonical zero-based integers.
    pub text_fields: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TranscriptItemIdentity {
    Native,
    /// Derived for old reasoning records without IDs; re-index after hydration.
    LegacyDerived,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptItem {
    pub id: Uuid,
    pub identity: TranscriptItemIdentity,
    pub position: usize,
    /// Small scalar metadata only; arbitrary-length data lives in fields.
    pub metadata: Value,
    pub fields: Vec<TranscriptField>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TranscriptPage {
    Index {
        collection: TranscriptCollection,
        offset: usize,
        total: usize,
        next_offset: Option<usize>,
        items: Vec<TranscriptItem>,
    },
    Text {
        collection: TranscriptCollection,
        item_id: Uuid,
        field: String,
        encoding: TranscriptEncoding,
        offset_bytes: usize,
        total_bytes: usize,
        next_offset_bytes: Option<usize>,
        text: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptResult {
    pub conversation_id: Uuid,
    pub observed_at_ms: u64,
    pub source: TranscriptSource,
    /// Whether the app read loaded detail. Persisted fallback does not hydrate it.
    pub detail_loaded: bool,
    pub page: TranscriptPage,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptError {
    pub code: String,
    pub message: String,
}
impl TranscriptError {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
    pub fn invalid(message: &str) -> Self {
        Self::new("invalid_arguments", message)
    }
}
impl std::fmt::Display for TranscriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for TranscriptError {}

pub fn field_names(collection: TranscriptCollection) -> &'static [&'static str] {
    match collection {
        TranscriptCollection::Messages => &[
            "content",
            "display_content",
            "attachments",
            "annotations",
            "undelivered_reason",
        ],
        TranscriptCollection::Activities => &[
            "title",
            "source_id",
            "tool_name",
            "mcp_server",
            "detail",
            "arguments",
            "output",
            "image_urls",
            "file_changes",
            "display_target",
            "display_description",
            "reasoning",
        ],
        TranscriptCollection::Turns => &["provider_resume_at", "checkpoint"],
    }
}

pub fn is_structured_field(field: &str) -> bool {
    matches!(
        field,
        "attachments" | "annotations" | "image_urls" | "file_changes" | "reasoning" | "checkpoint"
    )
}

pub fn field_encoding(field: &str) -> TranscriptEncoding {
    if field.ends_with("/metadata") {
        TranscriptEncoding::Json
    } else {
        TranscriptEncoding::Text
    }
}

pub fn leaf_templates(field: &str) -> &'static [&'static str] {
    match field {
        "attachments" => &[
            "attachments/{index}/path",
            "attachments/{index}/mention",
            "attachments/{index}/name",
            "attachments/{index}/blob_reference",
            "attachments/{index}/metadata",
        ],
        "annotations" => &[
            "annotations/{index}/comment",
            "annotations/{index}/metadata",
            "annotations/{index}/target/quote",
            "annotations/{index}/target/block",
            "annotations/{index}/target/spans/{span}/quote",
            "annotations/{index}/target/spans/{span}/metadata",
        ],
        "image_urls" => &["image_urls/{index}"],
        "file_changes" => &[
            "file_changes/{index}/path",
            "file_changes/{index}/diff",
            "file_changes/{index}/metadata",
        ],
        "reasoning" => &["reasoning/content", "reasoning/metadata"],
        "checkpoint" => &[
            "checkpoint/git_ref",
            "checkpoint/metadata",
            "checkpoint/files/{index}/path",
            "checkpoint/files/{index}/metadata",
        ],
        _ => &[],
    }
}

pub fn valid_field_path(collection: TranscriptCollection, field: &str) -> bool {
    if field.len() > 160 {
        return false;
    }
    let parts: Vec<_> = field.split('/').collect();
    let root = parts[0];
    if !field_names(collection).contains(&root) {
        return false;
    }
    if !is_structured_field(root) {
        return parts.len() == 1;
    }
    leaf_templates(root).iter().any(|template| {
        let expected: Vec<_> = template.split('/').collect();
        parts.len() == expected.len()
            && parts.iter().zip(expected).all(|(actual, expected)| {
                if matches!(expected, "{index}" | "{span}") {
                    canonical_index(actual).is_some()
                } else {
                    *actual == expected
                }
            })
    })
}

fn canonical_index(value: &str) -> Option<usize> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    value
        .parse::<usize>()
        .ok()
        .filter(|index| *index <= i64::MAX as usize)
}

/// Validates a bounded raw byte prefix returned by memory or SQLite. Invalid
/// start offsets fail; incomplete final codepoints are deferred to the next read.
pub fn text_page(
    collection: TranscriptCollection,
    item_id: Uuid,
    field: &str,
    offset: usize,
    total: usize,
    mut bytes: Vec<u8>,
) -> Result<TranscriptPage, TranscriptError> {
    if offset > total {
        return Err(TranscriptError::invalid(
            "offsetBytes exceeds the field length",
        ));
    }
    match std::str::from_utf8(&bytes) {
        Ok(_) => {}
        Err(error) if error.error_len().is_none() => bytes.truncate(error.valid_up_to()),
        Err(_) => {
            return Err(TranscriptError::invalid(
                "offsetBytes must be a UTF-8 character boundary",
            ));
        }
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| TranscriptError::invalid("field contains invalid UTF-8"))?;
    let end = offset + text.len();
    if end < total && text.is_empty() {
        return Err(TranscriptError::invalid(
            "slice cannot advance from this UTF-8 boundary",
        ));
    }
    Ok(TranscriptPage::Text {
        collection,
        item_id,
        field: field.into(),
        encoding: field_encoding(field),
        offset_bytes: offset,
        total_bytes: total,
        next_offset_bytes: (end < total).then_some(end),
        text,
    })
}

pub fn field_preview(
    collection: TranscriptCollection,
    item_id: Uuid,
    field: &str,
    total: usize,
    bytes: Vec<u8>,
) -> Result<TranscriptField, TranscriptError> {
    let TranscriptPage::Text { text, .. } = text_page(collection, item_id, field, 0, total, bytes)?
    else {
        unreachable!()
    };
    Ok(TranscriptField {
        field: field.into(),
        encoding: field_encoding(field),
        total_bytes: Some(total),
        preview_truncated: text.len() < total,
        preview: Some(text),
        item_count: None,
        query: Some(field_query(collection, item_id, field)),
        text_fields: Vec::new(),
    })
}

pub fn structured_field(
    _collection: TranscriptCollection,
    _item_id: Uuid,
    field: &str,
    item_count: Option<usize>,
) -> TranscriptField {
    TranscriptField {
        field: field.into(),
        encoding: TranscriptEncoding::Json,
        total_bytes: None,
        preview: None,
        preview_truncated: false,
        item_count,
        query: None,
        text_fields: leaf_templates(field)
            .iter()
            .map(|value| (*value).into())
            .collect(),
    }
}

fn field_query(collection: TranscriptCollection, item_id: Uuid, field: &str) -> TranscriptQuery {
    TranscriptQuery::Text {
        collection,
        item_id,
        field: field.into(),
        offset_bytes: 0,
        max_bytes: default_max_bytes(),
    }
}

enum Content<'a> {
    Text(&'a str),
    Group(Option<usize>),
}
struct Field<'a> {
    name: &'static str,
    content: Content<'a>,
}
impl<'a> Field<'a> {
    fn text(name: &'static str, value: &'a str) -> Self {
        Self {
            name,
            content: Content::Text(value),
        }
    }
    fn group(name: &'static str, count: Option<usize>) -> Self {
        Self {
            name,
            content: Content::Group(count),
        }
    }
}

fn activity_fields(activity: &ActivityItem) -> Vec<Field<'_>> {
    let mut fields = vec![Field::text("title", &activity.title)];
    for (name, value) in [
        ("source_id", &activity.source_id),
        ("tool_name", &activity.tool_name),
        ("mcp_server", &activity.mcp_server),
        ("detail", &activity.detail),
        ("arguments", &activity.arguments),
        ("output", &activity.output),
        ("display_target", &activity.display_target),
        ("display_description", &activity.display_description),
    ] {
        if let Some(value) = value {
            fields.push(Field::text(name, value));
        }
    }
    fields.push(Field::group("image_urls", Some(activity.image_urls.len())));
    fields.push(Field::group(
        "file_changes",
        Some(activity.file_changes.len()),
    ));
    if activity.reasoning.is_some() {
        fields.push(Field::group("reasoning", None));
    }
    fields
}

fn bounded_bytes(text: &str, offset: usize, limit: usize) -> Vec<u8> {
    text.as_bytes()
        .get(offset..offset.saturating_add(limit).min(text.len()))
        .unwrap_or_default()
        .to_vec()
}
fn metadata(value: Value) -> Cow<'static, str> {
    Cow::Owned(value.to_string())
}
fn borrowed(value: &str) -> Cow<'_, str> {
    Cow::Borrowed(value)
}

fn message_leaf<'a>(message: &'a Message, parts: &[&str]) -> Option<Cow<'a, str>> {
    match parts {
        ["content"] => Some(borrowed(&message.content)),
        ["display_content"] => message.display_content.as_deref().map(borrowed),
        ["undelivered_reason"] => message.undelivered_reason.as_deref().map(borrowed),
        ["attachments", index, field] => {
            let item = message.attachments.get(canonical_index(index)?)?;
            match *field {
                "path" => item.path.to_str().map(borrowed),
                "mention" => Some(borrowed(&item.mention)),
                "name" => Some(borrowed(&item.name)),
                "blob_reference" => item.blob_reference.as_deref().map(borrowed),
                "metadata" => Some(metadata(
                    json!({"isDir": item.is_dir, "isImage":item.is_image}),
                )),
                _ => None,
            }
        }
        ["annotations", index, tail @ ..] => {
            let item = message.annotations.get(canonical_index(index)?)?;
            match tail {
                ["comment"] => item.comment.as_deref().map(borrowed),
                ["metadata"] => Some(metadata(
                    json!({"id":item.id,"type":"messageSpan","messageId":item.target.message_id(),"spanCount":item.target.spans().len()}),
                )),
                ["target", "quote"] => Some(borrowed(item.target.quote())),
                ["target", "block"] => Some(borrowed(item.target.block())),
                ["target", "spans", span, field] => {
                    let span = item.target.spans().get(canonical_index(span)?)?;
                    match *field {
                        "quote" => Some(borrowed(&span.quote)),
                        "metadata" => Some(metadata(
                            json!({"ordinal":span.ordinal,"start":span.span.start,"end":span.span.end}),
                        )),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn activity_leaf<'a>(activity: &'a ActivityItem, parts: &[&str]) -> Option<Cow<'a, str>> {
    match parts {
        ["image_urls", index] => activity
            .image_urls
            .get(canonical_index(index)?)
            .map(|value| borrowed(value)),
        ["file_changes", index, field] => {
            let item = activity.file_changes.get(canonical_index(index)?)?;
            match *field {
                "path" => Some(borrowed(&item.path)),
                "diff" => item.diff.as_deref().map(borrowed),
                "metadata" => Some(metadata(
                    json!({"additions":item.additions,"deletions":item.deletions,"status":item.status}),
                )),
                _ => None,
            }
        }
        ["reasoning", field] => {
            let item = activity.reasoning.as_ref()?;
            match *field {
                "content" => Some(borrowed(&item.content)),
                "metadata" => Some(metadata(
                    json!({"startedAtMs":item.started_at_ms,"finishedAtMs":item.finished_at_ms}),
                )),
                _ => None,
            }
        }
        [name] => activity_fields(activity)
            .into_iter()
            .find_map(|field| match field.content {
                Content::Text(value) if field.name == *name => Some(borrowed(value)),
                _ => None,
            }),
        _ => None,
    }
}

fn turn_leaf<'a>(turn: &'a AgentTurn, parts: &[&str]) -> Option<Cow<'a, str>> {
    match parts {
        ["provider_resume_at"] => turn.provider_resume_at.as_deref().map(borrowed),
        ["checkpoint", tail @ ..] => {
            let item = turn.checkpoint.as_ref()?;
            match tail {
                ["git_ref"] => Some(borrowed(&item.git_ref)),
                ["metadata"] => Some(metadata(
                    json!({"turnCount":item.turn_count,"status":item.status,"fileCount":item.files.len(),"additions":item.additions,"deletions":item.deletions,"createdAt":item.created_at}),
                )),
                ["files", index, field] => {
                    let file = item.files.get(canonical_index(index)?)?;
                    match *field {
                        "path" => Some(borrowed(&file.path)),
                        "metadata" => Some(metadata(
                            json!({"additions":file.additions,"deletions":file.deletions}),
                        )),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Read a loaded conversation only. Text leaves are sliced directly; metadata
/// serialization is fixed-size, never a walk over large strings or collections.
/// This never selects, hydrates, starts, or modifies a session.
pub fn read_transcript(
    session: &AgentSession,
    query: &TranscriptQuery,
) -> Result<TranscriptResult, TranscriptError> {
    query.validate()?;
    if !session.detail_loaded {
        return Err(TranscriptError::new(
            "detail_not_loaded",
            "use the persisted transcript read for unloaded conversations",
        ));
    }
    let collection = query.collection();
    let page = match query {
        TranscriptQuery::Text {
            item_id,
            field,
            offset_bytes,
            max_bytes,
            ..
        } => {
            let parts: Vec<_> = field.split('/').collect();
            let not_found = || {
                TranscriptError::new(
                    "item_not_found",
                    "the requested item is not in this conversation and collection; re-index if the source changed or legacy history was hydrated/migrated",
                )
            };
            let value = match collection {
                TranscriptCollection::Messages => message_leaf(
                    session
                        .messages
                        .iter()
                        .find(|item| item.id == *item_id)
                        .ok_or_else(not_found)?,
                    &parts,
                ),
                TranscriptCollection::Activities => activity_leaf(
                    session
                        .transcript_blocks
                        .iter()
                        .flat_map(|block| &block.activities)
                        .find(|item| item.id == *item_id)
                        .ok_or_else(not_found)?,
                    &parts,
                ),
                TranscriptCollection::Turns => turn_leaf(
                    session
                        .turns
                        .iter()
                        .find(|item| item.id == *item_id)
                        .ok_or_else(not_found)?,
                    &parts,
                ),
            }
            .ok_or_else(|| {
                TranscriptError::new(
                    "field_absent",
                    "this item has no value for the requested field or element",
                )
            })?;
            text_page(
                collection,
                *item_id,
                field,
                *offset_bytes,
                value.len(),
                bounded_bytes(&value, *offset_bytes, *max_bytes),
            )?
        }
        TranscriptQuery::Index { offset, limit, .. } => {
            let total = match collection {
                TranscriptCollection::Messages => session.messages.len(),
                TranscriptCollection::Activities => session
                    .transcript_blocks
                    .iter()
                    .map(|block| block.activities.len())
                    .sum(),
                TranscriptCollection::Turns => session.turns.len(),
            };
            if *offset > total {
                return Err(TranscriptError::invalid(
                    "index offset exceeds collection length",
                ));
            }
            let mut items = Vec::new();
            let mut visit = |position: usize,
                             id: Uuid,
                             metadata: Value,
                             mut fields: Vec<Field<'_>>|
             -> Result<(), TranscriptError> {
                fields.sort_by_key(|field| {
                    field_names(collection)
                        .iter()
                        .position(|name| *name == field.name)
                });
                let fields = fields
                    .into_iter()
                    .map(|field| match field.content {
                        Content::Text(text) => field_preview(
                            collection,
                            id,
                            field.name,
                            text.len(),
                            bounded_bytes(text, 0, TRANSCRIPT_PREVIEW_BYTES),
                        ),
                        Content::Group(count) => {
                            Ok(structured_field(collection, id, field.name, count))
                        }
                    })
                    .collect::<Result<_, _>>()?;
                items.push(TranscriptItem {
                    id,
                    identity: TranscriptItemIdentity::Native,
                    position,
                    metadata,
                    fields,
                });
                Ok(())
            };
            match collection {
                TranscriptCollection::Messages => {
                    for (position, item) in session
                        .messages
                        .iter()
                        .enumerate()
                        .skip(*offset)
                        .take(*limit)
                    {
                        let mut fields = vec![
                            Field::text("content", &item.content),
                            Field::group("attachments", Some(item.attachments.len())),
                            Field::group("annotations", Some(item.annotations.len())),
                        ];
                        if let Some(value) = &item.display_content {
                            fields.push(Field::text("display_content", value));
                        }
                        if let Some(value) = &item.undelivered_reason {
                            fields.push(Field::text("undelivered_reason", value));
                        }
                        visit(
                            position,
                            item.id,
                            json!({"turnId":item.turn_id,"role":item.role,"createdAt":item.created_at,"streaming":item.streaming,"pending":item.pending}),
                            fields,
                        )?;
                    }
                }
                TranscriptCollection::Activities => {
                    for (position, (block, item)) in session
                        .transcript_blocks
                        .iter()
                        .flat_map(|block| block.activities.iter().map(move |item| (block, item)))
                        .enumerate()
                        .skip(*offset)
                        .take(*limit)
                    {
                        visit(
                            position,
                            item.id,
                            json!({"turnId":block.turn_id,"afterMessage":block.after_message,"kind":item.kind,"complete":item.complete,"failed":item.failed}),
                            activity_fields(item),
                        )?;
                    }
                }
                TranscriptCollection::Turns => {
                    for (position, item) in
                        session.turns.iter().enumerate().skip(*offset).take(*limit)
                    {
                        let mut fields = Vec::new();
                        if let Some(value) = &item.provider_resume_at {
                            fields.push(Field::text("provider_resume_at", value));
                        }
                        if let Some(value) = &item.checkpoint {
                            fields.push(Field::group("checkpoint", Some(value.files.len())));
                        }
                        visit(
                            position,
                            item.id,
                            json!({"turnCount":item.turn_count,"status":item.status,"providerTurnStarted":item.provider_turn_started,"startedAt":item.started_at,"completedAt":item.completed_at}),
                            fields,
                        )?;
                    }
                }
            }
            let end = *offset + items.len();
            TranscriptPage::Index {
                collection,
                offset: *offset,
                total,
                next_offset: (end < total).then_some(end),
                items,
            }
        }
    };
    Ok(TranscriptResult {
        conversation_id: session.id,
        observed_at_ms: crate::model::unix_time_millis(),
        source: TranscriptSource::Live,
        detail_loaded: true,
        page,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ActivityKind, Message, MessageRole, ProviderKind, TranscriptBlock};

    #[test]
    fn huge_utf8_message_reassembles_without_truncation() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let content = "日😀\"\\\n".repeat(120_000);
        session
            .messages
            .push(Message::new(MessageRole::Assistant, &content));
        let id = session.messages[0].id;
        let mut offset = 0;
        let mut actual = String::new();
        loop {
            let result = read_transcript(
                &session,
                &TranscriptQuery::Text {
                    collection: TranscriptCollection::Messages,
                    item_id: id,
                    field: "content".into(),
                    offset_bytes: offset,
                    max_bytes: 65536,
                },
            )
            .unwrap();
            assert!(serde_json::to_vec(&result).unwrap().len() < 4 * 1024 * 1024);
            let TranscriptPage::Text {
                text,
                total_bytes,
                next_offset_bytes,
                ..
            } = result.page
            else {
                panic!()
            };
            assert_eq!(total_bytes, content.len());
            assert!(text.len() <= 65536);
            actual.push_str(&text);
            let Some(next) = next_offset_bytes else {
                break;
            };
            assert!(next > offset);
            offset = next;
        }
        assert_eq!(actual, content);
        assert!(
            read_transcript(
                &session,
                &TranscriptQuery::Text {
                    collection: TranscriptCollection::Messages,
                    item_id: id,
                    field: "content".into(),
                    offset_bytes: 1,
                    max_bytes: 4
                }
            )
            .is_err()
        );
    }

    #[test]
    fn huge_activity_index_is_bounded_and_preserves_full_field_lengths() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let mut activity =
            ActivityItem::new(None, ActivityKind::Tool, "\0".repeat(2_000_000), None, true);
        activity.output = Some("😀".repeat(2_000_000));
        activity.image_urls = vec!["x".repeat(3_000_000)];
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 0,
            turn_id: None,
            activities: vec![activity],
        });
        let result = read_transcript(
            &session,
            &TranscriptQuery::Index {
                collection: TranscriptCollection::Activities,
                offset: 0,
                limit: 100,
            },
        )
        .unwrap();
        assert!(serde_json::to_vec(&result).unwrap().len() < 8192);
        let TranscriptPage::Index { items, .. } = result.page else {
            panic!()
        };
        assert_eq!(
            items[0]
                .fields
                .iter()
                .find(|field| field.field == "output")
                .unwrap()
                .total_bytes,
            Some(8_000_000)
        );
        assert!(items[0].fields.iter().all(|field| {
            field
                .preview
                .as_ref()
                .is_none_or(|preview| preview.len() <= 256)
        }));
    }

    #[test]
    fn malformed_queries_unloaded_shells_and_wrong_ids_fail() {
        for value in [
            json!({"type":"index","collection":"messages","limit":-1}),
            json!({"type":"index","collection":"messages","surprise":true}),
            json!({"type":"text","collection":"messages","itemId":"bad","field":"content"}),
        ] {
            assert!(serde_json::from_value::<TranscriptQuery>(value).is_err());
        }
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let query = TranscriptQuery::Index {
            collection: TranscriptCollection::Messages,
            offset: 0,
            limit: 0,
        };
        assert!(read_transcript(&session, &query).is_err());
        let query: TranscriptQuery =
            serde_json::from_value(json!({"type":"index","collection":"messages"})).unwrap();
        session.detail_loaded = false;
        assert_eq!(
            read_transcript(&session, &query).unwrap_err().code,
            "detail_not_loaded"
        );
        assert!(!session.detail_loaded);
    }
    #[test]
    fn ranges_eof_empty_and_missing_are_explicit() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        session
            .messages
            .push(Message::new(MessageRole::Assistant, "日😀"));
        let id = session.messages[0].id;
        let query = |offset_bytes, max_bytes| TranscriptQuery::Text {
            collection: TranscriptCollection::Messages,
            item_id: id,
            field: "content".into(),
            offset_bytes,
            max_bytes,
        };
        for (offset, max) in [(0, 0), (0, 3), (0, 65537), (8, 4), (usize::MAX, 4), (4, 4)] {
            assert!(read_transcript(&session, &query(offset, max)).is_err());
        }
        let TranscriptPage::Text {
            text,
            next_offset_bytes,
            ..
        } = read_transcript(&session, &query(7, 4)).unwrap().page
        else {
            panic!()
        };
        assert!(text.is_empty());
        assert!(next_offset_bytes.is_none());
        let absent = TranscriptQuery::Text {
            collection: TranscriptCollection::Messages,
            item_id: id,
            field: "display_content".into(),
            offset_bytes: 0,
            max_bytes: 4,
        };
        assert_eq!(
            read_transcript(&session, &absent).unwrap_err().code,
            "field_absent"
        );
        session.messages[0].display_content = Some(String::new());
        let TranscriptPage::Text {
            text,
            total_bytes,
            next_offset_bytes,
            ..
        } = read_transcript(&session, &absent).unwrap().page
        else {
            panic!()
        };
        assert!(text.is_empty());
        assert_eq!(total_bytes, 0);
        assert!(next_offset_bytes.is_none());
        let missing = TranscriptQuery::Text {
            collection: TranscriptCollection::Messages,
            item_id: Uuid::new_v4(),
            field: "content".into(),
            offset_bytes: 0,
            max_bytes: 4,
        };
        assert_eq!(
            read_transcript(&session, &missing).unwrap_err().code,
            "item_not_found"
        );
        for (offset, limit) in [(0, 0), (0, 101), (2, 1), (usize::MAX, 1)] {
            assert!(
                read_transcript(
                    &session,
                    &TranscriptQuery::Index {
                        collection: TranscriptCollection::Messages,
                        offset,
                        limit
                    }
                )
                .is_err()
            );
        }
    }

    #[test]
    fn maximum_escaped_index_stays_below_observation_limit() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let mut activities = Vec::new();
        for _ in 0..MAX_TRANSCRIPT_ITEMS {
            let text = "\0".repeat(512);
            let mut item = ActivityItem::new(
                Some(text.clone()),
                ActivityKind::Tool,
                &text,
                Some(text.clone()),
                true,
            );
            item.tool_name = Some(text.clone());
            item.mcp_server = Some(text.clone());
            item.arguments = Some(text.clone());
            item.output = Some(text.clone());
            item.display_target = Some(text.clone());
            item.display_description = Some(text);
            activities.push(item);
        }
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 0,
            turn_id: None,
            activities,
        });
        let result = read_transcript(
            &session,
            &TranscriptQuery::Index {
                collection: TranscriptCollection::Activities,
                offset: 0,
                limit: MAX_TRANSCRIPT_ITEMS,
            },
        )
        .unwrap();
        assert!(
            serde_json::to_vec(&result).unwrap().len() < crate::MAX_AUTOMATION_OBSERVATION_BYTES
        );
        let TranscriptPage::Index { items, .. } = result.page else {
            panic!()
        };
        assert_eq!(items.len(), MAX_TRANSCRIPT_ITEMS);
        for item in items {
            for field in item.fields {
                let Some(query) = field.query else {
                    continue;
                };
                let slice = read_transcript(&session, &query).unwrap();
                assert!(
                    serde_json::to_vec(&slice).unwrap().len()
                        < crate::MAX_AUTOMATION_OBSERVATION_BYTES
                );
            }
        }
    }

    #[test]
    fn structured_text_leaves_are_bounded_and_paths_are_strict() {
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let mut item = ActivityItem::new(None, ActivityKind::Tool, "Tool", None, true);
        item.image_urls = vec!["日😀".repeat(2_000_000)];
        let id = item.id;
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 0,
            turn_id: None,
            activities: vec![item],
        });
        for invalid in [
            "image_urls",
            "image_urls/01",
            "image_urls/-1",
            "image_urls/18446744073709551616",
            "image_urls/0/unknown",
        ] {
            assert!(!valid_field_path(TranscriptCollection::Activities, invalid));
        }
        let result = read_transcript(
            &session,
            &TranscriptQuery::Text {
                collection: TranscriptCollection::Activities,
                item_id: id,
                field: "image_urls/0".into(),
                offset_bytes: 7_000_000,
                max_bytes: 4,
            },
        )
        .unwrap();
        let TranscriptPage::Text {
            text,
            total_bytes,
            next_offset_bytes,
            ..
        } = result.page
        else {
            panic!()
        };
        assert_eq!(text, "日");
        assert_eq!(total_bytes, 14_000_000);
        assert_eq!(next_offset_bytes, Some(7_000_003));
    }
}
