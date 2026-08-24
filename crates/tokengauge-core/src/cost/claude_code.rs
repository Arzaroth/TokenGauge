//! Claude Code transcripts.
//!
//! `~/.claude/projects/**/*.jsonl`, one JSON object per line. Assistant records
//! carry a `message.usage` block, which is the billed unit. Anything else on
//! the line is ignored.
//!
//! This tree is also where GLM and Kimi spend shows up when those plans are
//! driven through Claude Code, so the *model* decides the provider here, never
//! the directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::Deserialize;

use super::{TokenCounts, UsageEvent, dedup_key, jsonl_files};
use crate::model_to_provider;

#[derive(Debug, Deserialize)]
struct Record {
    #[serde(default)]
    timestamp: String,
    #[serde(default, rename = "requestId")]
    request_id: String,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    usage: Option<Usage>,
}

/// Only the fields that are billed. `iterations` is a per-step breakdown whose
/// totals are already the fields above it, and `output_tokens_details.
/// thinking_tokens` is a subset of `output_tokens`: adding either double counts.
#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation: Option<CacheCreation>,
}

#[derive(Debug, Deserialize)]
struct CacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

/// Roots to scan, in the order ccusage checks them. `CLAUDE_CONFIG_DIR` may
/// name several directories, comma or colon separated.
pub fn roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(configured) = std::env::var("CLAUDE_CONFIG_DIR") {
        for dir in configured
            .split([',', ':'])
            .filter(|d| !d.trim().is_empty())
        {
            roots.push(Path::new(dir.trim()).join("projects"));
        }
    }
    if roots.is_empty()
        && let Some(home) = dirs::home_dir()
    {
        roots.push(home.join(".claude").join("projects"));
        roots.push(home.join(".config").join("claude").join("projects"));
    }
    roots.retain(|r| r.is_dir());
    roots
}

/// A model name that is not a billed call. `<synthetic>` marks Claude Code's
/// own local messages; `{{model}}` is an untemplated artifact.
fn is_billable_model(model: &str) -> bool {
    !model.is_empty() && !model.starts_with('<') && !model.starts_with("{{")
}

/// Read every usage event dated `since` or later.
///
/// `seen` maps a dedup key to the index of the event it produced, so it is only
/// meaningful alongside the `events` vector it was built with: pass the same
/// pair for the whole read, and start both fresh for the next one. Two
/// different things make one message appear more than once:
///
/// - a resumed or branched session copies earlier turns into a new file
///   verbatim, so without dedup a long project re-counts its own history;
/// - a streaming message is written repeatedly as it completes, each record
///   restating the same request with more `output_tokens` than the last.
///
/// The second is why a duplicate upgrades the event instead of being dropped:
/// keeping the first record of a group loses whatever the message went on to
/// say. On this machine that was 46% of one model's daily output.
pub fn read_events(
    roots: &[PathBuf],
    since: NaiveDate,
    seen: &mut HashMap<u64, usize>,
) -> Vec<UsageEvent> {
    let mut events = Vec::new();
    for root in roots {
        for file in jsonl_files(root, since) {
            read_file(&file, since, seen, &mut events);
        }
    }
    events
}

pub(super) fn read_file(
    path: &Path,
    since: NaiveDate,
    seen: &mut HashMap<u64, usize>,
    events: &mut Vec<UsageEvent>,
) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    for line in contents.lines() {
        if !line.contains("\"usage\"") {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            continue;
        };
        let Some((key, event)) = event_from(&record, since) else {
            continue;
        };
        let Some(key) = key else {
            // Nothing to match it against, so it stands on its own.
            events.push(event);
            continue;
        };
        match seen.get(&key) {
            // The completed record of a streamed message supersedes the
            // partial ones: same request, strictly more of it. The bounds check
            // keeps a mismatched `seen`/`events` pair from panicking or
            // overwriting an unrelated event.
            Some(&index)
                if index < events.len() && event.tokens.total() > events[index].tokens.total() =>
            {
                events[index] = event;
            }
            Some(&index) if index < events.len() => {}
            _ => {
                seen.insert(key, events.len());
                events.push(event);
            }
        }
    }
}

/// The event a record describes, and the key that identifies its request.
fn event_from(record: &Record, since: NaiveDate) -> Option<(Option<u64>, UsageEvent)> {
    let message = record.message.as_ref()?;
    let usage = message.usage.as_ref()?;
    if !is_billable_model(&message.model) {
        return None;
    }
    let provider = model_to_provider(&message.model)?;

    let at = record.timestamp.parse::<DateTime<Utc>>().ok()?;
    let date = at.with_timezone(&Local).date_naive();
    if date < since {
        return None;
    }

    let key = (!message.id.is_empty() || !record.request_id.is_empty())
        .then(|| dedup_key(&message.id, &record.request_id));

    // The 5m/1h split is authoritative when present; older records carry only
    // the total, which is a 5m write.
    let (write_5m, write_1h) = match &usage.cache_creation {
        Some(c) if c.ephemeral_5m_input_tokens + c.ephemeral_1h_input_tokens > 0 => {
            (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens)
        }
        _ => (usage.cache_creation_input_tokens, 0),
    };

    Some((
        key,
        UsageEvent {
            provider,
            model: message.model.clone(),
            date,
            at,
            tokens: TokenCounts {
                input: usage.input_tokens,
                output: usage.output_tokens,
                cache_write_5m: write_5m,
                cache_write_1h: write_1h,
                cache_read: usage.cache_read_input_tokens,
            },
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    fn read(lines: &str, since: NaiveDate) -> Vec<UsageEvent> {
        // Unique per call: these tests run in parallel and two of them may
        // hand this helper the same fixture text.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tokengauge-claude-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("transcript.jsonl");
        std::fs::write(&path, lines).expect("write fixture");
        let mut seen = HashMap::new();
        let mut events = Vec::new();
        read_file(&path, since, &mut seen, &mut events);
        let _ = std::fs::remove_dir_all(&dir);
        events
    }

    const REAL: &str = r#"
{"type":"assistant","timestamp":"2026-08-20T22:41:09.626Z","requestId":"req_A","message":{"id":"msg_A","model":"claude-opus-5","usage":{"input_tokens":2,"cache_creation_input_tokens":41864,"cache_read_input_tokens":0,"output_tokens":254,"output_tokens_details":{"thinking_tokens":115},"cache_creation":{"ephemeral_1h_input_tokens":41864,"ephemeral_5m_input_tokens":0},"iterations":[{"input_tokens":2,"output_tokens":254}]}}}
"#;

    #[test]
    fn reads_the_billed_fields_and_splits_the_cache_write() {
        let events = read(REAL, day(2026, 8, 1));
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.provider, "claude");
        assert_eq!(e.model, "claude-opus-5");
        assert_eq!(e.tokens.cache_write_1h, 41864);
        assert_eq!(e.tokens.cache_write_5m, 0);
        // `iterations` restates the totals and `thinking_tokens` is inside
        // `output_tokens`; neither is added.
        assert_eq!(e.tokens.input, 2);
        assert_eq!(e.tokens.output, 254);
    }

    #[test]
    fn a_resumed_session_does_not_count_its_history_twice() {
        let doubled = format!("{REAL}{REAL}");
        let events = read(&doubled, day(2026, 8, 1));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tokens.output, 254);
    }

    #[test]
    fn the_completed_record_of_a_streamed_message_wins() {
        // Same request written twice as it streams. Keeping the first loses
        // everything the message went on to say; keeping both bills it twice.
        let partial = r#"{"timestamp":"2026-08-20T10:00:00.000Z","requestId":"req_S","message":{"id":"msg_S","model":"claude-fable-5","usage":{"input_tokens":2,"output_tokens":120,"cache_creation_input_tokens":500}}}"#;
        let complete = r#"{"timestamp":"2026-08-20T10:00:00.000Z","requestId":"req_S","message":{"id":"msg_S","model":"claude-fable-5","usage":{"input_tokens":2,"output_tokens":3830,"cache_creation_input_tokens":500}}}"#;
        let events = read(&format!("{partial}\n{complete}\n"), day(2026, 8, 1));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tokens.output, 3830);
        assert_eq!(events[0].tokens.cache_write_5m, 500);
    }

    #[test]
    fn a_partial_record_arriving_last_does_not_undo_the_complete_one() {
        let partial = r#"{"timestamp":"2026-08-20T10:00:00.000Z","requestId":"req_S","message":{"id":"msg_S","model":"claude-fable-5","usage":{"input_tokens":2,"output_tokens":120}}}"#;
        let complete = r#"{"timestamp":"2026-08-20T10:00:00.000Z","requestId":"req_S","message":{"id":"msg_S","model":"claude-fable-5","usage":{"input_tokens":2,"output_tokens":3830}}}"#;
        let events = read(&format!("{complete}\n{partial}\n"), day(2026, 8, 1));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tokens.output, 3830);
    }

    #[test]
    fn synthetic_and_template_models_are_not_calls() {
        let lines = r#"
{"timestamp":"2026-08-20T10:00:00.000Z","requestId":"r1","message":{"id":"m1","model":"<synthetic>","usage":{"input_tokens":5,"output_tokens":5}}}
{"timestamp":"2026-08-20T10:00:00.000Z","requestId":"r2","message":{"id":"m2","model":"{{model}}","usage":{"input_tokens":5,"output_tokens":5}}}
"#;
        assert!(read(lines, day(2026, 8, 1)).is_empty());
    }

    #[test]
    fn glm_through_claude_code_is_glm_spend() {
        let lines = r#"
{"timestamp":"2026-08-20T10:00:00.000Z","requestId":"r1","message":{"id":"m1","model":"glm-4.6","usage":{"input_tokens":5,"output_tokens":5}}}
"#;
        let events = read(lines, day(2026, 8, 1));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].provider, "glm");
    }

    #[test]
    fn a_legacy_record_without_the_split_is_a_five_minute_write() {
        let lines = r#"
{"timestamp":"2026-08-20T10:00:00.000Z","requestId":"r1","message":{"id":"m1","model":"claude-opus-5","usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":900}}}
"#;
        let events = read(lines, day(2026, 8, 1));
        assert_eq!(events[0].tokens.cache_write_5m, 900);
        assert_eq!(events[0].tokens.cache_write_1h, 0);
    }

    #[test]
    fn events_before_the_window_are_skipped() {
        assert!(read(REAL, day(2026, 8, 23)).is_empty());
    }

    #[test]
    fn a_day_is_the_local_one_not_the_utc_one() {
        // 22:41Z on the 20th is already the 21st anywhere east of UTC+2, and
        // "today" in the panel means the user's day.
        let events = read(REAL, day(2026, 8, 1));
        let utc = "2026-08-20T22:41:09.626Z"
            .parse::<DateTime<Utc>>()
            .expect("fixture timestamp");
        assert_eq!(events[0].date, utc.with_timezone(&Local).date_naive());
    }

    #[test]
    fn a_record_with_no_ids_is_kept_rather_than_deduped_away() {
        let lines = r#"
{"timestamp":"2026-08-20T10:00:00.000Z","message":{"model":"claude-opus-5","usage":{"input_tokens":1,"output_tokens":1}}}
{"timestamp":"2026-08-20T10:00:00.000Z","message":{"model":"claude-opus-5","usage":{"input_tokens":1,"output_tokens":1}}}
"#;
        assert_eq!(read(lines, day(2026, 8, 1)).len(), 2);
    }
}
