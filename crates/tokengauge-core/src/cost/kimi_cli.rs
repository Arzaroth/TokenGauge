//! Kimi Code transcripts.
//!
//! `~/.kimi-code/sessions/**/wire.jsonl` (and `~/.kimi`, the older home), one
//! JSON object per line. Two record shapes have shipped and both are still on
//! disk in any long-lived session tree, so both are read:
//!
//! - **Kimi Code**: `{"type": "usage.record", "usageScope": ..., "usage": {...}}`,
//!   the model named on the record itself.
//! - **older**: `{"message": {"type": "StatusUpdate", "payload": {"token_usage": ...}}}`,
//!   which names no model - that comes from `config.json` at the session root.
//!
//! The format is ccusage's (`rust/adapters/kimi`, MIT); this is an independent
//! reader of it, and the traps below are the reason it is not a naive sum.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::{Local, NaiveDate, TimeZone, Utc};
use serde::Deserialize;

use super::{TokenCounts, UsageEvent, dedup_key, jsonl_files};
use crate::model_to_provider;

/// What the subscription reports when no model was pinned. It is the plan, not
/// a model; `pricing` resolves it to whatever the plan currently serves.
const DEFAULT_MODEL: &str = "kimi-for-coding";

#[derive(Debug, Default, Deserialize)]
struct Record {
    #[serde(default, rename = "type")]
    kind: String,
    // Kimi Code
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<CodeUsage>,
    #[serde(default, rename = "usageScope")]
    usage_scope: Option<String>,
    /// Milliseconds.
    #[serde(default)]
    time: Option<i64>,
    // older
    #[serde(default)]
    message: Option<Message>,
    /// Seconds, fractional.
    #[serde(default)]
    timestamp: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeUsage {
    #[serde(default)]
    input_other: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    input_cache_creation: u64,
    #[serde(default)]
    input_cache_read: u64,
}

#[derive(Debug, Default, Deserialize)]
struct Message {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Option<Payload>,
}

#[derive(Debug, Default, Deserialize)]
struct Payload {
    #[serde(default)]
    token_usage: Option<TokenUsage>,
    #[serde(default)]
    message_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TokenUsage {
    #[serde(default)]
    input_other: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    input_cache_creation: u64,
    #[serde(default)]
    input_cache_read: u64,
}

#[derive(Debug, Default, Deserialize)]
struct Config {
    #[serde(default)]
    model: Option<String>,
}

/// `KIMI_DATA_DIR` may name several homes, comma separated; otherwise both
/// spellings of the default home are read, since a machine upgraded across the
/// rename has sessions under each.
pub fn roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(configured) = std::env::var("KIMI_DATA_DIR") {
        for dir in configured.split(',').filter(|d| !d.trim().is_empty()) {
            roots.push(Path::new(dir.trim()).join("sessions"));
        }
    }
    if roots.is_empty()
        && let Some(home) = dirs::home_dir()
    {
        roots.push(home.join(".kimi-code").join("sessions"));
        roots.push(home.join(".kimi").join("sessions"));
    }
    roots.retain(|r| r.is_dir());
    roots
}

/// A `wire.jsonl` sits at a known depth under the sessions root. Anything else
/// with that name is some other tool's file that happens to collide.
fn is_wire_file(root: &Path, path: &Path) -> bool {
    if path.file_name().and_then(|n| n.to_str()) != Some("wire.jsonl") {
        return false;
    }
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    // sessions/<group>/<session>/wire.jsonl, or the newer
    // sessions/<workspace>/<session>/agents/<agent>/wire.jsonl.
    matches!(relative.components().count(), 3 | 5)
}

/// The session root holding `config.json`, walked back up from a wire file.
fn session_root(root: &Path, path: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(root).ok()?;
    let depth = relative.components().count();
    let mut dir = path.parent()?;
    // Up past the wire file's own directory to the tree root: three levels for
    // the old layout, five for the agent-scoped one.
    for _ in 1..depth {
        dir = dir.parent()?;
    }
    dir.parent().map(Path::to_path_buf)
}

fn model_from_config(root: &Path, path: &Path) -> String {
    session_root(root, path)
        .and_then(|dir| std::fs::read_to_string(dir.join("config.json")).ok())
        .and_then(|raw| serde_json::from_str::<Config>(&raw).ok())
        .and_then(|config| config.model)
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

pub fn read_events(
    roots: &[PathBuf],
    since: NaiveDate,
    seen: &mut HashSet<u64>,
) -> Vec<UsageEvent> {
    let mut events = Vec::new();
    for root in roots {
        for file in jsonl_files(root, since) {
            if !is_wire_file(root, &file) {
                continue;
            }
            read_file(root, &file, since, seen, &mut events);
        }
    }
    events
}

fn read_file(
    root: &Path,
    path: &Path,
    since: NaiveDate,
    seen: &mut HashSet<u64>,
    events: &mut Vec<UsageEvent>,
) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let session = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    // Read lazily: the old shape is the only one that needs it, and a tree of
    // Kimi Code sessions would otherwise re-read the same file once per line.
    let mut configured_model: Option<String> = None;

    for line in raw.lines() {
        let line = line.trim();
        // Cheap reject before the parse. Both shapes spell it: `usage.record`
        // and `usageScope` on one, `token_usage` on the other.
        if line.is_empty() || !line.contains("usage") {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            continue;
        };

        let (tokens, at, model, id) = match record.kind.as_str() {
            "usage.record" => {
                // A session record restates the session's running total. Summing
                // it alongside the turns it is made of doubles the session.
                if record.usage_scope.as_deref() != Some("turn") {
                    continue;
                }
                let Some(usage) = record.usage.as_ref() else {
                    continue;
                };
                let Some(at) = record
                    .time
                    .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
                else {
                    continue;
                };
                let model = record
                    .model
                    .as_deref()
                    .map(|m| m.strip_prefix("kimi-code/").unwrap_or(m))
                    .filter(|m| !m.trim().is_empty())
                    .unwrap_or(DEFAULT_MODEL)
                    .to_string();
                let tokens = TokenCounts {
                    input: usage.input_other,
                    output: usage.output,
                    cache_write_5m: usage.input_cache_creation,
                    cache_write_1h: 0,
                    cache_read: usage.input_cache_read,
                };
                (tokens, at, model, String::new())
            }
            "metadata" => continue,
            _ => {
                let Some(message) = record.message.as_ref() else {
                    continue;
                };
                if message.kind != "StatusUpdate" {
                    continue;
                }
                let Some(usage) = message
                    .payload
                    .as_ref()
                    .and_then(|p| p.token_usage.as_ref())
                else {
                    continue;
                };
                let Some(at) = record
                    .timestamp
                    .filter(|s| s.is_finite())
                    .and_then(|s| Utc.timestamp_millis_opt((s * 1000.0) as i64).single())
                else {
                    continue;
                };
                let model = configured_model
                    .get_or_insert_with(|| model_from_config(root, path))
                    .clone();
                let tokens = TokenCounts {
                    input: usage.input_other,
                    output: usage.output,
                    cache_write_5m: usage.input_cache_creation,
                    cache_write_1h: 0,
                    cache_read: usage.input_cache_read,
                };
                let id = message
                    .payload
                    .as_ref()
                    .and_then(|p| p.message_id.clone())
                    .unwrap_or_default();
                (tokens, at, model, id)
            }
        };

        if tokens.total() == 0 {
            continue;
        }
        let date = at.with_timezone(&Local).date_naive();
        if date < since {
            continue;
        }

        // The record carries no id of its own in the newer shape, so the whole
        // reading identifies it: a wire file replayed into a resumed session
        // repeats the same instant, model and counts.
        let key = dedup_key(
            &session,
            &format!(
                "{id}|{}|{model}|{}|{}|{}|{}",
                at.timestamp_millis(),
                tokens.input,
                tokens.output,
                tokens.cache_write_5m,
                tokens.cache_read
            ),
        );
        if !seen.insert(key) {
            continue;
        }

        events.push(UsageEvent {
            provider: model_to_provider(&model).unwrap_or("kimi"),
            model,
            date,
            at,
            tokens,
            key: Some(key),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    /// A throwaway Kimi home. Unique per call: these tests run in parallel.
    fn home() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tokengauge-kimi-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Read the home and take it back down again.
    fn read(home: &Path) -> Vec<UsageEvent> {
        let mut seen = HashSet::new();
        let events = read_events(&[home.join("sessions")], day(2020, 1, 1), &mut seen);
        let _ = std::fs::remove_dir_all(home);
        events
    }

    fn wire(dir: &Path, relative: &str, body: &str) {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
        std::fs::write(&path, body).expect("write");
    }

    #[test]
    fn a_kimi_code_turn_record_is_one_billed_call() {
        let tmp = home();
        let sessions = tmp.join("sessions");
        wire(
            &sessions,
            "ws/sess-1/agents/main/wire.jsonl",
            r#"{"type":"usage.record","usageScope":"turn","model":"kimi-code/kimi-k2-thinking","time":1756000000000,"usage":{"inputOther":10,"output":20,"inputCacheCreation":30,"inputCacheRead":40}}
"#,
        );
        let events = read(&tmp);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].provider, "kimi");
        assert_eq!(events[0].model, "kimi-k2-thinking");
        assert_eq!(events[0].tokens.input, 10);
        assert_eq!(events[0].tokens.output, 20);
        assert_eq!(events[0].tokens.cache_write_5m, 30);
        assert_eq!(events[0].tokens.cache_read, 40);
    }

    /// The session-scoped record is the running total of the turns beside it.
    /// Counting both bills the session twice.
    #[test]
    fn a_session_scoped_record_is_not_a_second_call() {
        let tmp = home();
        let sessions = tmp.join("sessions");
        wire(
            &sessions,
            "ws/sess-1/wire.jsonl",
            r#"{"type":"usage.record","usageScope":"turn","model":"kimi-k2.6","time":1756000000000,"usage":{"inputOther":10,"output":20}}
{"type":"usage.record","usageScope":"session","model":"kimi-k2.6","time":1756000001000,"usage":{"inputOther":10,"output":20}}
"#,
        );
        let events = read(&tmp);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tokens.total(), 30);
    }

    /// The older shape names no model on the record; `config.json` at the tree
    /// root does.
    #[test]
    fn the_older_shape_takes_its_model_from_the_session_config() {
        let tmp = home();
        let sessions = tmp.join("sessions");
        std::fs::write(tmp.join("config.json"), r#"{"model":"kimi-k2-thinking"}"#)
            .expect("write config");
        wire(
            &sessions,
            "group/sess-1/wire.jsonl",
            r#"{"message":{"type":"StatusUpdate","payload":{"message_id":"m1","token_usage":{"input_other":5,"output":7,"input_cache_read":11}}},"timestamp":1756000000.5}
"#,
        );
        let events = read(&tmp);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model, "kimi-k2-thinking");
        assert_eq!(events[0].tokens.input, 5);
        assert_eq!(events[0].tokens.cache_read, 11);
    }

    #[test]
    fn a_config_without_a_model_falls_back_to_the_plan() {
        let tmp = home();
        let sessions = tmp.join("sessions");
        wire(
            &sessions,
            "group/sess-1/wire.jsonl",
            r#"{"message":{"type":"StatusUpdate","payload":{"token_usage":{"output":7}}},"timestamp":1756000000}
"#,
        );
        let events = read(&tmp);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model, DEFAULT_MODEL);
    }

    #[test]
    fn a_repeated_reading_is_the_same_call_twice() {
        let tmp = home();
        let sessions = tmp.join("sessions");
        let line = r#"{"type":"usage.record","usageScope":"turn","model":"kimi-k2.6","time":1756000000000,"usage":{"inputOther":10,"output":20}}"#;
        wire(
            &sessions,
            "ws/sess-1/wire.jsonl",
            &format!("{line}\n{line}\n"),
        );
        assert_eq!(read(&tmp).len(), 1);
    }

    /// A file named `wire.jsonl` outside the layout is some other tool's.
    #[test]
    fn only_a_wire_file_at_the_right_depth_is_read() {
        let tmp = home();
        let sessions = tmp.join("sessions");
        wire(
            &sessions,
            "wire.jsonl",
            r#"{"type":"usage.record","usageScope":"turn","model":"kimi-k2.6","time":1756000000000,"usage":{"output":9}}
"#,
        );
        assert!(read(&tmp).is_empty());
    }
}
