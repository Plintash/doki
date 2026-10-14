//! Read-only transcript projection. SQLite slices before rows cross into Rust;
//! neither whole session JSON nor oversized message/tool text crosses the wire.
//! JSON1 still scans a session's stored JSON for activity/turn lookup. This is
//! deliberately daemon-side work, not a UI hydration or a persistent new index.
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;
use uuid::Uuid;
use waku_protocol::{
    TRANSCRIPT_PREVIEW_BYTES, TranscriptCollection, TranscriptError, TranscriptItem,
    TranscriptItemIdentity, TranscriptPage, TranscriptQuery, TranscriptResult, TranscriptSource,
    field_names, field_preview, is_structured_field, structured_field, text_page,
};

/// Always persisted truth: callers with loaded app detail use the live reader.
pub(crate) fn read_transcript(
    path: &Path,
    conversation_id: Uuid,
    query: &TranscriptQuery,
) -> Result<TranscriptResult> {
    query.validate()?;
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let transaction = connection.transaction()?;
    let exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
        [conversation_id.to_string()],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(TranscriptError::new(
            "conversation_not_found",
            "the requested conversation has no persisted history",
        )
        .into());
    }
    let collection = query.collection();
    let projection = Projection::new(collection);
    let page = match query {
        TranscriptQuery::Index { offset, limit, .. } => {
            let total: usize = transaction.query_row(
                &format!("{} SELECT count(*) FROM items", projection.with),
                [conversation_id.to_string()],
                |row| row.get(0),
            )?;
            if *offset > total {
                return Err(
                    TranscriptError::invalid("index offset exceeds collection length").into(),
                );
            }
            let mut columns = String::new();
            for name in field_names(collection) {
                let expression = projection.field(name);
                if is_structured_field(name) {
                    columns.push_str(&format!(
                        ", {} , {}",
                        projection.present(name),
                        projection.count(name)
                    ));
                } else {
                    columns.push_str(&format!(", length(CAST({expression} AS BLOB)), substr(CAST({expression} AS BLOB), 1, {TRANSCRIPT_PREVIEW_BYTES})"));
                }
            }
            let identity = if collection == TranscriptCollection::Activities {
                "legacy"
            } else {
                "0"
            };
            let sql = format!(
                "{} SELECT id, position, substr(CAST({} AS BLOB), 1, 8193) {columns}, {identity} FROM items ORDER BY position LIMIT ?2 OFFSET ?3",
                projection.with, projection.metadata
            );
            let mut statement = transaction.prepare(&sql)?;
            let mut rows = statement.query(params![
                conversation_id.to_string(),
                *limit as i64,
                *offset as i64
            ])?;
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                let id = Uuid::parse_str(&row.get::<_, String>(0)?)
                    .context("invalid persisted transcript item UUID")?;
                let position: usize = row.get(1)?;
                let metadata: Vec<u8> = row.get(2)?;
                if metadata.len() > 8192 {
                    bail!("persisted transcript metadata exceeds the scalar metadata bound");
                }
                let metadata: Value = serde_json::from_slice(&metadata)?;
                // No arbitrary provider text is allowed into index metadata.
                if !metadata.as_object().is_some_and(|object| {
                    object.values().all(|value| {
                        value.is_null()
                            || value.is_boolean()
                            || value.is_number()
                            || value.as_str().is_some_and(|text| text.len() <= 128)
                    })
                }) {
                    bail!("persisted transcript metadata must contain only bounded scalar values");
                }
                let mut fields = Vec::new();
                for (index, name) in field_names(collection).iter().enumerate() {
                    if is_structured_field(name) {
                        if row.get::<_, bool>(3 + index * 2)? {
                            fields.push(structured_field(
                                collection,
                                id,
                                name,
                                row.get(4 + index * 2)?,
                            ));
                        }
                    } else if let Some(total) = row.get::<_, Option<usize>>(3 + index * 2)? {
                        // SQLite substr() may return SQL NULL for an empty
                        // BLOB. The non-null byte length distinguishes an
                        // existing empty string from an absent optional field.
                        let bytes: Option<Vec<u8>> = row.get(4 + index * 2)?;
                        fields.push(field_preview(
                            collection,
                            id,
                            name,
                            total,
                            bytes.unwrap_or_default(),
                        )?);
                    }
                }
                let identity = if row.get::<_, bool>(3 + field_names(collection).len() * 2)? {
                    TranscriptItemIdentity::LegacyDerived
                } else {
                    TranscriptItemIdentity::Native
                };
                items.push(TranscriptItem {
                    id,
                    identity,
                    position,
                    metadata,
                    fields,
                });
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
        TranscriptQuery::Text {
            item_id,
            field,
            offset_bytes,
            max_bytes,
            ..
        } => {
            let expression = projection.field(field);
            let sql = format!(
                "{} SELECT length(CAST({expression} AS BLOB)), substr(CAST({expression} AS BLOB), ?3, ?4) FROM items WHERE id = ?2",
                projection.with
            );
            let result = transaction
                .query_row(
                    &sql,
                    params![
                        conversation_id.to_string(),
                        item_id.to_string(),
                        (*offset_bytes as i64)
                            .checked_add(1)
                            .ok_or_else(|| TranscriptError::invalid("offsetBytes is too large"))?,
                        *max_bytes as i64
                    ],
                    |row| {
                        Ok((
                            row.get::<_, Option<usize>>(0)?,
                            row.get::<_, Option<Vec<u8>>>(1)?,
                        ))
                    },
                )
                .optional()?;
            let Some((total, bytes)) = result else {
                return Err(TranscriptError::new(
                    "item_not_found",
                    "the requested item is not in this conversation and collection; re-index if the source changed or legacy history was hydrated/migrated",
                )
                .into());
            };
            let Some(total) = total else {
                return Err(TranscriptError::new(
                    "field_absent",
                    "this item has no value for the requested field",
                )
                .into());
            };
            text_page(
                collection,
                *item_id,
                field,
                *offset_bytes,
                total,
                bytes.unwrap_or_default(),
            )?
        }
    };
    // Dropping the read transaction leaves catalog, hydration, and runtime state
    // untouched. Each subsequent page makes a fresh per-call observation.
    Ok(TranscriptResult {
        conversation_id,
        observed_at_ms: waku_protocol::model::unix_time_millis(),
        source: TranscriptSource::Persisted,
        detail_loaded: false,
        page,
    })
}

struct Projection {
    collection: TranscriptCollection,
    with: &'static str,
    metadata: &'static str,
}
impl Projection {
    fn new(collection: TranscriptCollection) -> Self {
        let (with, metadata) = match collection {
            TranscriptCollection::Messages => (
                "WITH items AS (SELECT id, position, turn_id, role, content, display_content, attachments, annotations, created_at, streaming FROM messages WHERE session_id = ?1)",
                "json_object('turnId', turn_id, 'role', role, 'createdAt', created_at, 'streaming', json(CASE WHEN streaming = 0 THEN 'false' ELSE 'true' END), 'pending', json('false'))",
            ),
            TranscriptCollection::Activities => (
                // Old reasoning blocks predate activity IDs. Their deterministic
                // UUIDs are stable within this conversation until normal app
                // hydration migrates/saves the legacy block as an activity.
                "WITH blocks AS (SELECT b.key AS block_position, b.value AS block FROM session_details s, json_each(s.data, '$.transcript_blocks') b WHERE s.session_id = ?1), raw_items AS (SELECT b.block_position, a.key AS activity_position, a.value AS data, json_extract(b.block, '$.turn_id') AS turn_id, json_extract(b.block, '$.after_message') AS after_message, 0 AS legacy FROM blocks b, json_each(b.block, '$.content.data') a WHERE json_extract(b.block, '$.content.kind') = 'activities' UNION ALL SELECT b.block_position, 0, json_object('id', '00000000-0000-5000-a000-' || printf('%012x', b.block_position), 'kind', 'reasoning', 'title', 'Reasoning', 'complete', json('true'), 'failed', json('false'), 'reasoning', json_extract(b.block, '$.content.data')), json_extract(b.block, '$.turn_id'), json_extract(b.block, '$.after_message'), 1 FROM blocks b WHERE json_extract(b.block, '$.content.kind') = 'reasoning'), items AS (SELECT row_number() OVER (ORDER BY block_position, activity_position) - 1 AS position, json_extract(data, '$.id') AS id, data, turn_id, after_message, legacy FROM raw_items)",
                "json_object('turnId', turn_id, 'afterMessage', after_message, 'kind', json_extract(data, '$.kind'), 'complete', json(CASE WHEN json_extract(data, '$.complete') THEN 'true' ELSE 'false' END), 'failed', json(CASE WHEN json_extract(data, '$.failed') THEN 'true' ELSE 'false' END))",
            ),
            TranscriptCollection::Turns => (
                "WITH items AS (SELECT t.key AS position, json_extract(t.value, '$.id') AS id, t.value AS data FROM session_details s, json_each(s.data, '$.turns') t WHERE s.session_id = ?1)",
                "json_object('turnCount', json_extract(data, '$.turn_count'), 'status', json_extract(data, '$.status'), 'providerTurnStarted', json(CASE WHEN json_extract(data, '$.provider_turn_started') THEN 'true' ELSE 'false' END), 'startedAt', json_extract(data, '$.started_at'), 'completedAt', json_extract(data, '$.completed_at'))",
            ),
        };
        Self {
            collection,
            with,
            metadata,
        }
    }
    fn present(&self, name: &str) -> String {
        match name {
            "attachments" | "annotations" | "image_urls" | "file_changes" => "1".into(),
            _ => format!("coalesce(json_type(data, '$.{name}') != 'null', 0)"),
        }
    }
    fn count(&self, name: &str) -> String {
        match name {
            "attachments" | "annotations" => format!("json_array_length({name})"),
            "image_urls" | "file_changes" => {
                format!("coalesce(json_array_length(data, '$.{name}'), 0)")
            }
            "checkpoint" => "coalesce(json_array_length(data, '$.checkpoint.files'), 0)".into(),
            _ => "NULL".into(),
        }
    }
    fn field(&self, name: &str) -> String {
        let parts: Vec<_> = name.split('/').collect();
        let root = parts[0];
        // Whole structured values are used only by descriptor count/presence;
        // text requests are validated against the fixed leaf-path templates.
        assert!(field_names(self.collection).contains(&root));
        if parts.len() == 1 {
            if self.collection == TranscriptCollection::Messages {
                return match name {
                    "undelivered_reason" => "NULL".into(),
                    _ => name.into(),
                };
            }
            return format!("json_extract(data, '$.{name}')");
        }
        let (source, segments) = if self.collection == TranscriptCollection::Messages {
            (root, &parts[1..])
        } else {
            ("data", parts.as_slice())
        };
        let path = |segments: &[&str]| {
            let mut path = "$".to_owned();
            for segment in segments {
                if segment.bytes().all(|byte| byte.is_ascii_digit()) {
                    path.push_str(&format!("[{segment}]"));
                } else {
                    path.push('.');
                    path.push_str(segment);
                }
            }
            path
        };
        if parts.last() != Some(&"metadata") {
            return format!("json_extract({source}, '{}')", path(segments));
        }
        let parent_path = path(&segments[..segments.len() - 1]);
        let extract = |field: &str| format!("json_extract({source}, '{parent_path}.{field}')");
        let boolean = |field: &str| {
            format!(
                "json(CASE WHEN {} THEN 'true' ELSE 'false' END)",
                extract(field)
            )
        };
        let array_len = |field: &str| {
            format!("coalesce(json_array_length({source}, '{parent_path}.{field}'), 0)")
        };
        let object = match root {
            "attachments" => format!(
                "json_object('isDir',{},'isImage',{})",
                boolean("is_dir"),
                boolean("is_image")
            ),
            "annotations" if parts.len() == 3 => format!(
                "json_object('id',{},'type','messageSpan','messageId',{},'spanCount',{})",
                extract("id"),
                extract("target.message_id"),
                array_len("target.spans")
            ),
            "annotations" => format!(
                "json_object('ordinal',{},'start',{},'end',{})",
                extract("ordinal"),
                extract("span.start"),
                extract("span.end")
            ),
            "file_changes" => format!(
                "json_object('additions',{},'deletions',{},'status',{})",
                extract("additions"),
                extract("deletions"),
                extract("status")
            ),
            "reasoning" => format!(
                "json_object('startedAtMs',{},'finishedAtMs',{})",
                extract("started_at_ms"),
                extract("finished_at_ms")
            ),
            "checkpoint" if parts.len() == 2 => format!(
                "json_object('turnCount',{},'status',{},'fileCount',{},'additions',{},'deletions',{},'createdAt',{})",
                extract("turn_count"),
                extract("status"),
                array_len("files"),
                extract("additions"),
                extract("deletions"),
                extract("created_at")
            ),
            "checkpoint" => format!(
                "json_object('additions',{},'deletions',{})",
                extract("additions"),
                extract("deletions")
            ),
            _ => unreachable!("metadata path validated before SQL projection"),
        };
        format!(
            "CASE WHEN coalesce(json_type({source}, '{parent_path}') != 'null', 0) THEN {object} ELSE NULL END"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use waku_protocol::model::{
        ActivityItem, ActivityKind, AgentSession, Message, MessageRole, ProviderKind,
        TranscriptBlock,
    };
    use waku_protocol::{TranscriptEncoding, field_encoding};

    fn fixture() -> (std::path::PathBuf, Uuid, Uuid, String) {
        let path =
            std::env::temp_dir().join(format!("waku-bounded-transcript-{}.db", Uuid::new_v4()));
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE sessions(id TEXT); CREATE TABLE session_details(session_id TEXT, data TEXT); CREATE TABLE messages(id TEXT, session_id TEXT, position INTEGER, turn_id TEXT, role TEXT, content TEXT, display_content TEXT, attachments TEXT, annotations TEXT, created_at INTEGER, streaming INTEGER);").unwrap();
        let mut session = AgentSession::new(Uuid::new_v4(), ProviderKind::Codex);
        let text = "日😀\n\"\\".repeat(150_000);
        let message = Message::new(MessageRole::Assistant, &text);
        let mut activity =
            ActivityItem::new(None, ActivityKind::Tool, "A large result", None, true);
        activity.output = Some(text.clone());
        let activity_id = activity.id;
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 1,
            turn_id: None,
            activities: vec![activity],
        });
        connection
            .execute("INSERT INTO sessions VALUES(?1)", [session.id.to_string()])
            .unwrap();
        connection
            .execute(
                "INSERT INTO session_details VALUES(?1,?2)",
                params![
                    session.id.to_string(),
                    serde_json::to_string(&session).unwrap()
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages VALUES(?1,?2,0,NULL,'assistant',?3,NULL,'[]','[]',1,0)",
                params![message.id.to_string(), session.id.to_string(), text],
            )
            .unwrap();
        (path, session.id, activity_id, text)
    }

    #[test]
    fn persisted_large_activity_round_trips_without_hydration_or_database_writes() {
        let (path, conversation_id, item_id, text) = fixture();
        let original = std::fs::read(&path).unwrap();
        let mut reconstructed = String::new();
        let mut offset = 0;
        loop {
            let result = read_transcript(
                &path,
                conversation_id,
                &TranscriptQuery::Text {
                    collection: TranscriptCollection::Activities,
                    item_id,
                    field: "output".into(),
                    offset_bytes: offset,
                    max_bytes: 65536,
                },
            )
            .unwrap();
            assert_eq!(result.source, TranscriptSource::Persisted);
            assert!(!result.detail_loaded);
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
            assert!(total_bytes > 1_000_000);
            reconstructed.push_str(&text);
            if let Some(next) = next_offset_bytes {
                assert!(next > offset);
                offset = next;
            } else {
                break;
            }
        }
        assert_eq!(reconstructed, text);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn persisted_index_is_bounded_and_matches_live_field_shapes() {
        let (path, conversation_id, item_id, _) = fixture();
        for collection in [
            TranscriptCollection::Messages,
            TranscriptCollection::Activities,
            TranscriptCollection::Turns,
        ] {
            let result = read_transcript(
                &path,
                conversation_id,
                &TranscriptQuery::Index {
                    collection,
                    offset: 0,
                    limit: 50,
                },
            )
            .unwrap();
            assert!(serde_json::to_vec(&result).unwrap().len() < 8192);
            let TranscriptPage::Index { items, .. } = result.page else {
                panic!()
            };
            assert!(items.iter().flat_map(|item| &item.fields).all(|field| {
                field
                    .preview
                    .as_ref()
                    .is_none_or(|preview| preview.len() <= 256)
            }));
        }
        for (id, field, offset) in [
            (Uuid::new_v4(), "output", 0),
            (item_id, "output", 1),
            (item_id, "detail", 0),
            (item_id, "output", usize::MAX),
        ] {
            assert!(
                read_transcript(
                    &path,
                    conversation_id,
                    &TranscriptQuery::Text {
                        collection: TranscriptCollection::Activities,
                        item_id: id,
                        field: field.into(),
                        offset_bytes: offset,
                        max_bytes: 4
                    }
                )
                .is_err()
            );
        }
        let connection = Connection::open(&path).unwrap();
        let legacy = json!({"transcript_blocks":[{"after_message":0,"content":{"kind":"reasoning","data":{"content":"legacy thought","started_at_ms":1,"finished_at_ms":2}}}],"turns":[]});
        connection
            .execute("UPDATE session_details SET data=?1", [legacy.to_string()])
            .unwrap();
        let query = TranscriptQuery::Index {
            collection: TranscriptCollection::Activities,
            offset: 0,
            limit: 50,
        };
        let first = read_transcript(&path, conversation_id, &query).unwrap();
        let second = read_transcript(&path, conversation_id, &query).unwrap();
        assert_eq!(
            serde_json::to_value(first.page).unwrap(),
            serde_json::to_value(second.page).unwrap()
        );
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn persisted_empty_text_is_present_and_null_optional_text_is_absent() {
        let (path, conversation_id, item_id, _) = fixture();
        let connection = Connection::open(&path).unwrap();
        connection.execute("UPDATE session_details SET data=json_set(data, '$.transcript_blocks[0].content.data[0].detail', '', '$.transcript_blocks[0].content.data[0].arguments', NULL)", []).unwrap();
        connection
            .execute("UPDATE messages SET content = ''", [])
            .unwrap();
        for (collection, name) in [
            (TranscriptCollection::Activities, "detail"),
            (TranscriptCollection::Messages, "content"),
        ] {
            let result = read_transcript(
                &path,
                conversation_id,
                &TranscriptQuery::Index {
                    collection,
                    offset: 0,
                    limit: 1,
                },
            )
            .unwrap();
            let TranscriptPage::Index { items, .. } = result.page else {
                panic!()
            };
            let field = items[0]
                .fields
                .iter()
                .find(|field| field.field == name)
                .unwrap();
            assert_eq!(field.total_bytes, Some(0));
            assert_eq!(field.preview.as_deref(), Some(""));
            assert!(!field.preview_truncated);
            let query = field.query.as_ref().unwrap();
            let TranscriptPage::Text {
                text,
                total_bytes,
                next_offset_bytes,
                ..
            } = read_transcript(&path, conversation_id, query).unwrap().page
            else {
                panic!()
            };
            assert!(text.is_empty());
            assert_eq!(total_bytes, 0);
            assert!(next_offset_bytes.is_none());
        }
        let query = TranscriptQuery::Text {
            collection: TranscriptCollection::Activities,
            item_id,
            field: "arguments".into(),
            offset_bytes: 0,
            max_bytes: 4,
        };
        assert_eq!(
            read_transcript(&path, conversation_id, &query)
                .unwrap_err()
                .downcast::<TranscriptError>()
                .unwrap()
                .code,
            "field_absent"
        );
        drop(connection);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn actual_store_migrations_pages_and_field_slices_match_live_data() {
        use crate::persistence::StateStore;
        use waku_protocol::model::{Project, ReasoningBlock};
        let root = std::env::temp_dir().join(format!("waku-transcript-store-{}", Uuid::new_v4()));
        let path = root.join("app.db");
        let store = StateStore::daemon(path.clone());
        let mut state = store.load().unwrap();
        let project = Project::from_path(root.join("project"));
        let mut session = AgentSession::new(project.id, ProviderKind::Codex);
        let turn_id = session.begin_turn("A prompt with 日 and \"quoted\" text");
        session.messages[0].display_content = Some("Display 日😀".into());
        session.messages[0]
            .attachments
            .push(waku_protocol::model::MessageAttachment {
                path: root.join("file.txt"),
                mention: "@file".into(),
                name: "File 日😀".into(),
                is_dir: false,
                is_image: false,
                blob_reference: Some("waku-attachment:example.txt".into()),
            });
        session.messages[0]
            .annotations
            .push(waku_protocol::model::MessageAnnotation {
                id: Uuid::new_v4(),
                comment: Some("Comment 日😀".into()),
                target: waku_protocol::model::AnnotationTarget::MessageSpan {
                    message_id: Uuid::new_v4(),
                    spans: vec![waku_protocol::model::AnnotationSpan {
                        ordinal: 0,
                        span: waku_protocol::model::TextSpan { start: 1, end: 2 },
                        quote: "span 日😀".into(),
                    }],
                    quote: "quoted 日😀".into(),
                    block: "Block 日😀".into(),
                },
            });
        session.turns[0].checkpoint = Some(waku_protocol::model::Checkpoint {
            turn_count: 1,
            git_ref: "refs/checkpoint/example".into(),
            status: waku_protocol::model::CheckpointStatus::Ready,
            files: vec![waku_protocol::model::CheckpointFile {
                path: "a/日.txt".into(),
                additions: 1,
                deletions: 2,
            }],
            additions: 1,
            deletions: 2,
            created_at: 3,
        });
        session.push_message(MessageRole::Assistant, "answer\0\n\\日");
        session.turns[0].provider_resume_at = Some("provider-turn-1".into());
        let mut activity = ActivityItem::new(
            Some("source-1".into()),
            ActivityKind::Tool,
            "Tool title",
            Some("detail😀".into()),
            true,
        );
        activity.tool_name = Some("test_tool".into());
        activity.mcp_server = Some("test_server".into());
        activity.arguments = Some("{\"input\":\"日😀\"}".into());
        activity.output = Some("Tool output 日😀\0\n\\".into());
        activity
            .file_changes
            .push(waku_protocol::model::ActivityFileChange {
                path: "src/日.rs".into(),
                additions: Some(1),
                deletions: None,
                status: Some(waku_protocol::model::ActivityFileChangeStatus::Modified),
                diff: Some("+diff 日😀".into()),
            });
        {
            use base64::Engine as _;
            activity.image_urls = vec![format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(vec![1_u8; 9000])
            )];
        }
        activity.reasoning = Some(ReasoningBlock {
            content: "Thought 日😀".into(),
            started_at_ms: 1,
            finished_at_ms: 2,
        });
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 1,
            turn_id: Some(turn_id),
            activities: vec![activity],
        });
        session.transcript_blocks.push(TranscriptBlock {
            after_message: 2,
            turn_id: None,
            activities: vec![ActivityItem::new(
                None,
                ActivityKind::Tool,
                "Second block",
                Some(String::new()),
                false,
            )],
        });
        let conversation_id = session.id;
        state.projects.push(project);
        state.push_session(session);
        store.save(&mut state).unwrap();
        let live = &state.sessions[0];
        assert!(
            live.transcript_blocks[0].activities[0].image_urls[0]
                .starts_with(crate::blob_store::BLOB_SCHEME)
        );
        let unloaded = store.load().unwrap();
        assert!(!unloaded.sessions[0].detail_loaded);
        for collection in [
            TranscriptCollection::Messages,
            TranscriptCollection::Activities,
            TranscriptCollection::Turns,
        ] {
            let mut offset = 0;
            loop {
                let query = TranscriptQuery::Index {
                    collection,
                    offset,
                    limit: 1,
                };
                let expected = waku_protocol::read_transcript(live, &query).unwrap();
                let actual = read_transcript(&path, conversation_id, &query).unwrap();
                assert_eq!(
                    serde_json::to_value(&actual.page).unwrap(),
                    serde_json::to_value(&expected.page).unwrap()
                );
                let TranscriptPage::Index {
                    items, next_offset, ..
                } = actual.page
                else {
                    panic!()
                };
                for item in items {
                    for field in item.fields {
                        let paths = if field.query.is_some() {
                            vec![field.field]
                        } else {
                            field
                                .text_fields
                                .into_iter()
                                .map(|path| path.replace("{index}", "0").replace("{span}", "0"))
                                .collect()
                        };
                        for field in paths {
                            let collect = |persisted: bool| -> Result<String> {
                                let mut offset_bytes = 0;
                                let mut text = String::new();
                                loop {
                                    let query = TranscriptQuery::Text {
                                        collection,
                                        item_id: item.id,
                                        field: field.clone(),
                                        offset_bytes,
                                        max_bytes: 4,
                                    };
                                    let result = if persisted {
                                        read_transcript(&path, conversation_id, &query)?
                                    } else {
                                        waku_protocol::read_transcript(live, &query)?
                                    };
                                    let TranscriptPage::Text {
                                        text: chunk,
                                        next_offset_bytes,
                                        total_bytes,
                                        ..
                                    } = result.page
                                    else {
                                        panic!()
                                    };
                                    assert!(chunk.len() <= 4);
                                    text.push_str(&chunk);
                                    let Some(next) = next_offset_bytes else {
                                        assert_eq!(text.len(), total_bytes);
                                        break;
                                    };
                                    assert!(next > offset_bytes);
                                    offset_bytes = next;
                                }
                                Ok(text)
                            };
                            let expected = collect(false);
                            let actual = collect(true);
                            match (expected, actual) {
                                (Ok(expected), Ok(actual))
                                    if field_encoding(&field) == TranscriptEncoding::Json =>
                                {
                                    assert_eq!(
                                        serde_json::from_str::<Value>(&actual).unwrap(),
                                        serde_json::from_str::<Value>(&expected).unwrap()
                                    )
                                }
                                (Ok(expected), Ok(actual)) => assert_eq!(actual, expected),
                                (Err(expected), Err(actual)) => {
                                    assert_eq!(
                                        expected.downcast_ref::<TranscriptError>().unwrap().code,
                                        "field_absent"
                                    );
                                    assert_eq!(
                                        actual.downcast_ref::<TranscriptError>().unwrap().code,
                                        "field_absent"
                                    );
                                }
                                result => panic!("live/persisted mismatch for {field}: {result:?}"),
                            }
                        }
                    }
                }
                let Some(next) = next_offset else {
                    break;
                };
                assert!(next > offset);
                offset = next;
            }
        }
        assert!(!unloaded.sessions[0].detail_loaded);
        assert!(unloaded.sessions[0].messages.is_empty());
        drop(unloaded);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
