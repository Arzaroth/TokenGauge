//! Grok CLI transcripts.
//!
//! `~/.grok/sessions/**/updates.jsonl`, one JSON object per line, with a
//! `summary.json` beside it naming the session and the model it was last set
//! to. Only a `turn_completed` update carries usage; everything else on the
//! line is the agent talking to its client.
//!
//! The format is ccusage's (`rust/adapters/grok`, MIT), which is the only
//! specification of it there is - the CLI documents its headless output and not
//! what it writes. Unlike the Claude and Codex readers, this one has never been
//! run against a populated tree on a developer machine, so it is written to
//! produce nothing rather than something wrong when the shape is not what it
//! expects.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::{Local, NaiveDate, TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;

use super::{TokenCounts, UsageEvent, dedup_key, jsonl_files};
use crate::model_to_provider;

#[derive(Debug, Default, Deserialize)]
struct Record {
    /// Unix seconds on the envelope.
    #[serde(default)]
    timestamp: Option<Value>,
    #[serde(default)]
    params: Option<Params>,
}

#[derive(Debug, Default, Deserialize)]
struct Params {
    #[serde(default, rename = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    update: Option<Update>,
    #[serde(default, rename = "_meta")]
    meta: Option<Meta>,
}

#[derive(Debug, Default, Deserialize)]
struct Update {
    #[serde(default, rename = "sessionUpdate")]
    session_update: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Default, Deserialize)]
struct Meta {
    #[serde(default, rename = "eventId")]
    event_id: Option<String>,
    #[serde(default, rename = "agentTimestampMs")]
    agent_timestamp_ms: Option<Value>,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cached_read_tokens: u64,
    #[serde(default)]
    cache_creation_tokens: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cached_read_tokens: u64,
    #[serde(default)]
    cache_creation_tokens: u64,
    /// Model id to its own share of the turn. A turn that switched models has
    /// several; the fields above are then their sum, and reading both would
    /// bill the turn twice.
    #[serde(default)]
    model_usage: Option<std::collections::HashMap<String, ModelUsage>>,
}

#[derive(Debug, Default, Deserialize)]
struct Summary {
    #[serde(default)]
    info: Option<SummaryInfo>,
    #[serde(default)]
    current_model_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SummaryInfo {
    #[serde(default)]
    id: Option<String>,
}

pub fn roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(home) = std::env::var("GROK_HOME")
        && !home.trim().is_empty()
    {
        roots.push(Path::new(home.trim()).join("sessions"));
    }
    if roots.is_empty()
        && let Some(home) = dirs::home_dir()
    {
        roots.push(home.join(".grok").join("sessions"));
    }
    roots.retain(|r| r.is_dir());
    roots
}

/// `cachedReadTokens` and `cacheCreationTokens` are both *inside*
/// `inputTokens`, the way Codex reports its cache reads and unlike Anthropic,
/// which reports them beside. Adding them would bill the cached part of every
/// turn twice, at the uncached rate.
fn split_input(input: u64, cached_read: u64, cache_creation: u64) -> (u64, u64, u64) {
    let cache_read = cached_read.min(input);
    let rest = input - cache_read;
    let cache_creation = cache_creation.min(rest);
    (rest - cache_creation, cache_read, cache_creation)
}

fn epoch_millis(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|n| n as i64))
        .filter(|ms| *ms > 0)
}

pub fn read_events(
    roots: &[PathBuf],
    since: NaiveDate,
    seen: &mut HashSet<u64>,
) -> Vec<UsageEvent> {
    let mut events = Vec::new();
    for root in roots {
        for file in jsonl_files(root, since) {
            if file.file_name().and_then(|n| n.to_str()) != Some("updates.jsonl") {
                continue;
            }
            read_file(&file, since, seen, &mut events);
        }
    }
    events
}

/// The session id and the model to fall back on, from the `summary.json` beside
/// the updates.
fn session_meta(path: &Path) -> (Option<String>, Option<String>) {
    let summary = std::fs::read_to_string(path.with_file_name("summary.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Summary>(&raw).ok());
    let Some(summary) = summary else {
        return (None, None);
    };
    (
        summary.info.and_then(|info| info.id),
        summary.current_model_id,
    )
}

fn read_file(path: &Path, since: NaiveDate, seen: &mut HashSet<u64>, events: &mut Vec<UsageEvent>) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let (summary_session, default_model) = session_meta(path);
    let dir_session = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    for line in raw.lines() {
        // Only a completed turn carries usage, and it is by far the rarer line.
        if !line.contains("turn_completed") {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            continue;
        };
        let Some(params) = record.params.as_ref() else {
            continue;
        };
        let Some(update) = params.update.as_ref() else {
            continue;
        };
        if update.session_update.as_deref() != Some("turn_completed") {
            continue;
        }
        let Some(usage) = update.usage.as_ref() else {
            continue;
        };

        let at = params
            .meta
            .as_ref()
            .and_then(|m| m.agent_timestamp_ms.as_ref())
            .and_then(epoch_millis)
            .or_else(|| {
                // Unix seconds on the envelope, milliseconds in the meta.
                record
                    .timestamp
                    .as_ref()
                    .and_then(epoch_millis)
                    .map(|s| s.saturating_mul(1000))
            })
            .and_then(|ms| Utc.timestamp_millis_opt(ms).single());
        let Some(at) = at else {
            continue;
        };
        let date = at.with_timezone(&Local).date_naive();
        if date < since {
            continue;
        }

        let session = params
            .session_id
            .clone()
            .or_else(|| summary_session.clone())
            .unwrap_or_else(|| dir_session.to_string());
        let event_id = params
            .meta
            .as_ref()
            .and_then(|m| m.event_id.as_deref())
            .unwrap_or_default();

        for (model, counts) in model_rows(usage, default_model.as_deref()) {
            let (input, cache_read, cache_write) = split_input(
                counts.input_tokens,
                counts.cached_read_tokens,
                counts.cache_creation_tokens,
            );
            let tokens = TokenCounts {
                input,
                output: counts.output_tokens,
                cache_write_5m: cache_write,
                cache_write_1h: 0,
                cache_read,
            };
            if tokens.total() == 0 {
                continue;
            }
            let key = dedup_key(
                &session,
                &format!(
                    "{event_id}|{}|{model}|{}",
                    at.timestamp_millis(),
                    tokens.total()
                ),
            );
            if !seen.insert(key) {
                continue;
            }
            events.push(UsageEvent {
                provider: model_to_provider(&model).unwrap_or("grok"),
                model,
                date,
                at,
                tokens,
                key: Some(key),
            });
        }
    }
}

/// One row per model the turn used, or the turn's own totals under the model
/// the session was set to.
///
/// A turn that names no model anywhere is dropped rather than filed under
/// "unknown": nothing can price that row, and the by-model panel would carry a
/// bucket a user cannot act on. The Claude and Codex readers already do this.
fn model_rows(usage: &Usage, default_model: Option<&str>) -> Vec<(String, ModelUsage)> {
    if let Some(map) = usage.model_usage.as_ref()
        && !map.is_empty()
    {
        let mut rows: Vec<(String, ModelUsage)> =
            map.iter().map(|(model, u)| (model.clone(), *u)).collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        return rows;
    }
    let Some(model) = default_model.filter(|m| !m.trim().is_empty()) else {
        return Vec::new();
    };
    vec![(
        model.to_string(),
        ModelUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_read_tokens: usage.cached_read_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
        },
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    fn home() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tokengauge-grok-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Lay down one session and read it, taking the tree back down again.
    fn read_session(updates: &str, summary: Option<&str>) -> Vec<UsageEvent> {
        let root = home();
        let session = root.join("sessions").join("project").join("sess-1");
        std::fs::create_dir_all(&session).expect("mkdir");
        std::fs::write(session.join("updates.jsonl"), updates).expect("write");
        if let Some(summary) = summary {
            std::fs::write(session.join("summary.json"), summary).expect("write");
        }
        let mut seen = HashSet::new();
        let events = read_events(&[root.join("sessions")], day(2020, 1, 1), &mut seen);
        let _ = std::fs::remove_dir_all(&root);
        events
    }

    fn turn(usage: &str) -> String {
        format!(
            r#"{{"timestamp":1756000000,"params":{{"sessionId":"s1","_meta":{{"eventId":"e1","agentTimestampMs":1756000000000}},"update":{{"sessionUpdate":"turn_completed","usage":{usage}}}}}}}"#
        )
    }

    #[test]
    fn a_completed_turn_is_split_per_model() {
        let events = read_session(
            &format!(
                "{}\n",
                turn(
                    r#"{"inputTokens":100,"outputTokens":50,"modelUsage":{"grok-4.5-build":{"inputTokens":100,"outputTokens":50}}}"#
                )
            ),
            None,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].provider, "grok");
        assert_eq!(events[0].model, "grok-4.5-build");
        assert_eq!(events[0].tokens.input, 100);
        assert_eq!(events[0].tokens.output, 50);
    }

    /// The cached read is part of the input count, not an extra bucket beside
    /// it. Counting it twice bills the cached part at the uncached rate.
    #[test]
    fn a_cached_read_is_taken_out_of_the_input_count() {
        let events = read_session(
            &format!(
                "{}\n",
                turn(
                    r#"{"modelUsage":{"grok-4":{"inputTokens":1000,"cachedReadTokens":600,"cacheCreationTokens":100,"outputTokens":20}}}"#
                )
            ),
            None,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tokens.input, 300);
        assert_eq!(events[0].tokens.cache_read, 600);
        assert_eq!(events[0].tokens.cache_write_5m, 100);
        // Still the thousand it started with, only classified.
        assert_eq!(events[0].tokens.total(), 1020);
    }

    /// The top-level counts are the sum of `modelUsage`, so a reader taking
    /// both bills every turn twice.
    #[test]
    fn the_turn_total_is_not_counted_beside_its_model_split() {
        let events = read_session(
            &format!(
                "{}\n",
                turn(
                    r#"{"inputTokens":300,"outputTokens":30,"modelUsage":{"grok-4":{"inputTokens":100,"outputTokens":10},"grok-4-fast":{"inputTokens":200,"outputTokens":20}}}"#
                )
            ),
            None,
        );
        let total: u64 = events.iter().map(|e| e.tokens.total()).sum();
        assert_eq!(events.len(), 2);
        assert_eq!(total, 330);
    }

    #[test]
    fn a_turn_without_a_model_split_uses_the_session_model() {
        let events = read_session(
            &format!("{}\n", turn(r#"{"inputTokens":10,"outputTokens":5}"#)),
            Some(r#"{"info":{"id":"sess-1"},"current_model_id":"grok-4"}"#),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model, "grok-4");
        assert_eq!(events[0].tokens.total(), 15);
    }

    /// Nothing names the model, so nothing can price the row.
    #[test]
    fn a_turn_no_one_names_a_model_for_is_dropped() {
        let events = read_session(
            &format!("{}\n", turn(r#"{"inputTokens":10,"outputTokens":5}"#)),
            None,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn a_line_that_is_not_a_completed_turn_carries_no_usage() {
        let events = read_session(
            r#"{"params":{"update":{"sessionUpdate":"agent_message_chunk","usage":{"inputTokens":900}}}}
{"params":{"update":{"sessionUpdate":"turn_completed"}}}
"#,
            Some(r#"{"current_model_id":"grok-4"}"#),
        );
        assert!(events.is_empty());
    }

    #[test]
    fn the_same_turn_read_twice_is_one_call() {
        let line = turn(r#"{"modelUsage":{"grok-4":{"inputTokens":10,"outputTokens":5}}}"#);
        let events = read_session(&format!("{line}\n{line}\n"), None);
        assert_eq!(events.len(), 1);
    }
}
