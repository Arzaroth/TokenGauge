//! Codex rollout transcripts.
//!
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`. A different shape from
//! Claude Code's, with three traps in it:
//!
//! 1. `total_token_usage` is **cumulative for the session**, climbing into the
//!    tens of millions. Its per-event delta is the billed unit.
//! 2. `last_token_usage` looks like that delta and is not: the same event is
//!    re-emitted with the cumulative unchanged, so summing it overcounts. On
//!    this machine one session sums to 89.0M against a true 82.7M.
//! 3. `cached_input_tokens` is a **subset** of `input_tokens`, unlike
//!    Anthropic's separate cache-read field. Billing them as siblings inflates
//!    every Codex figure by roughly the cache hit rate, which is most tokens.
//!
//! The model is not in the usage payload either: it lives in `turn_context`,
//! and it changes mid-session.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::Deserialize;

use super::{TokenCounts, UsageEvent, dedup_key, jsonl_files};
use crate::model_to_provider;

#[derive(Debug, Deserialize)]
struct Record {
    #[serde(default)]
    timestamp: String,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Option<Payload>,
}

#[derive(Debug, Deserialize)]
struct Payload {
    #[serde(default, rename = "type")]
    kind: String,
    /// `turn_context` and `session_meta` carry the model in flight.
    #[serde(default)]
    model: Option<String>,
    /// `session_meta` only.
    #[serde(default)]
    id: Option<String>,
    /// `token_count` only, and null on the rate-limit-only emissions.
    #[serde(default)]
    info: Option<TokenInfo>,
}

#[derive(Debug, Deserialize)]
struct TokenInfo {
    #[serde(default)]
    total_token_usage: Cumulative,
}

/// Running totals for the session. `reasoning_output_tokens` is a subset of
/// `output_tokens` and `cached_input_tokens` a subset of `input_tokens`, so
/// neither is added to anything.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(super) struct Cumulative {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

impl Cumulative {
    /// What was billed between two readings. Saturating because a session that
    /// restarts its counter must read as "nothing new", never as a negative.
    fn since(&self, prev: &Cumulative) -> TokenCounts {
        let input = self.input_tokens.saturating_sub(prev.input_tokens);
        let cached = self
            .cached_input_tokens
            .saturating_sub(prev.cached_input_tokens);
        TokenCounts {
            // Cached input is inside input: what is left is what was charged
            // at the fresh-input rate.
            input: input.saturating_sub(cached),
            output: self.output_tokens.saturating_sub(prev.output_tokens),
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: cached,
        }
    }
}

pub fn roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    // An empty or blank CODEX_HOME is unset, not a relative path. Taking it
    // literally pushes a bogus `sessions` root, which `retain` then drops -
    // leaving no roots, skipping the ~/.codex fallback, and reporting zero
    // Codex spend. `codex_home()` in codex.rs already reads it this way.
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.trim().is_empty()
    {
        roots.push(Path::new(home.trim()).join("sessions"));
    }
    if roots.is_empty()
        && let Some(home) = dirs::home_dir()
    {
        roots.push(home.join(".codex").join("sessions"));
    }
    roots.retain(|r| r.is_dir());
    roots
}

pub fn read_events(
    roots: &[PathBuf],
    since: NaiveDate,
    seen: &mut HashSet<u64>,
) -> Vec<UsageEvent> {
    let mut events = Vec::new();
    // Running totals per session, not per file: a session continued into a
    // second rollout picks its cumulative up where the first left off, and
    // starting from zero there would bill its entire history again as one
    // delta. `jsonl_files` sorts by path, and rollouts are named
    // `YYYY/MM/DD/rollout-<ISO timestamp>-<id>`, so that order is chronological
    // and the earlier file is always read first.
    let mut totals: HashMap<String, Cumulative> = HashMap::new();
    for root in roots {
        for file in jsonl_files(root, since) {
            read_file(&file, since, seen, &mut totals, &mut events);
        }
    }
    events
}

pub(super) fn read_file(
    path: &Path,
    since: NaiveDate,
    seen: &mut HashSet<u64>,
    totals: &mut HashMap<String, Cumulative>,
    events: &mut Vec<UsageEvent>,
) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };

    // `events` is the accumulator shared by every file in the read, so the
    // fix-up at the end must touch only what this call appended. Rescanning the
    // whole vector once per file is quadratic in the number of transcripts.
    let appended_from = events.len();
    // A rollout with no session_meta is its own bucket; there is nothing to
    // continue it from.
    let mut session_id = path.to_string_lossy().into_owned();
    let mut model: Option<String> = None;
    // Index into `events` of rows emitted before any `turn_context` announced a
    // model. A session states its model on the first turn, so these are filled
    // in from the first one seen rather than dropped.
    let mut pending: Vec<usize> = Vec::new();

    for line in contents.lines() {
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            continue;
        };
        let Some(payload) = record.payload.as_ref() else {
            continue;
        };

        if record.kind == "session_meta" {
            if let Some(id) = payload.id.as_deref().filter(|id| !id.is_empty()) {
                session_id = id.to_string();
            }
            if model.is_none()
                && let Some(m) = payload.model.as_deref()
            {
                model = Some(m.to_string());
            }
            continue;
        }
        if record.kind == "turn_context" {
            if let Some(m) = payload.model.as_deref().filter(|m| !m.is_empty()) {
                model = Some(m.to_string());
                for index in pending.drain(..) {
                    events[index].model = m.to_string();
                }
            }
            continue;
        }
        if payload.kind != "token_count" {
            continue;
        }
        let Some(info) = payload.info.as_ref() else {
            continue;
        };

        let cumulative = info.total_token_usage;
        let previous = totals.entry(session_id.clone()).or_default();
        let tokens = cumulative.since(previous);
        *previous = cumulative;
        if tokens.total() == 0 {
            continue;
        }

        let Ok(timestamp) = record.timestamp.parse::<DateTime<Utc>>() else {
            continue;
        };
        let date = timestamp.with_timezone(&Local).date_naive();
        if date < since {
            continue;
        }

        // A resumed session replayed into a second file would repeat the same
        // cumulative readings; the session and its running total identify one.
        if !seen.insert(dedup_key(
            &session_id,
            &format!("{}|{}", record.timestamp, cumulative.total_tokens),
        )) {
            continue;
        }

        let name = model.clone().unwrap_or_default();
        if model.is_none() {
            pending.push(events.len());
        }
        events.push(UsageEvent {
            provider: model_to_provider(&name).unwrap_or("codex"),
            model: name,
            date,
            at: timestamp,
            tokens,
        });
    }

    // A session that never named its model contributes tokens under no model
    // name at all; that is a row nothing can price, so drop it.
    for index in pending.into_iter().rev() {
        events.remove(index);
    }
    // The provider is settled by the model, which may have arrived late.
    for event in events[appended_from..].iter_mut() {
        if let Some(provider) = model_to_provider(&event.model) {
            event.provider = provider;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    fn read(lines: &str) -> Vec<UsageEvent> {
        // Unique per call: these tests run in parallel and two of them may
        // hand this helper the same fixture text.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tokengauge-codex-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("rollout.jsonl");
        std::fs::write(&path, lines).expect("write fixture");
        let mut seen = HashSet::new();
        let mut totals = HashMap::new();
        let mut events = Vec::new();
        read_file(&path, day(2026, 1, 1), &mut seen, &mut totals, &mut events);
        let _ = std::fs::remove_dir_all(&dir);
        events
    }

    fn token_count(ts: &str, input: u64, cached: u64, output: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output},"total_tokens":{}}},"last_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output},"total_tokens":{}}}}}}}}}"#,
            input + output,
            input + output
        )
    }

    const CONTEXT: &str = r#"{"timestamp":"2026-05-11T06:17:00.967Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#;

    #[test]
    fn cumulative_readings_are_billed_as_deltas() {
        let lines = format!(
            "{CONTEXT}\n{}\n{}\n",
            token_count("2026-05-11T06:18:00.000Z", 1000, 0, 100),
            token_count("2026-05-11T06:19:00.000Z", 2500, 0, 300),
        );
        let events = read(&lines);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].tokens.input, 1000);
        assert_eq!(events[0].tokens.output, 100);
        // The second reading is a running total, not a second bill.
        assert_eq!(events[1].tokens.input, 1500);
        assert_eq!(events[1].tokens.output, 200);
    }

    #[test]
    fn a_re_emitted_event_bills_nothing() {
        // The trap: `last_token_usage` repeats while the cumulative stands
        // still. Summing it would charge this session twice.
        let repeat = token_count("2026-05-11T06:18:00.000Z", 1000, 0, 100);
        let again = token_count("2026-05-11T06:18:30.000Z", 1000, 0, 100);
        let events = read(&format!("{CONTEXT}\n{repeat}\n{again}\n"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tokens.total(), 1100);
    }

    #[test]
    fn cached_input_is_charged_at_the_read_rate_not_twice() {
        let lines = format!(
            "{CONTEXT}\n{}\n",
            token_count("2026-05-11T06:18:00.000Z", 13459, 9600, 62)
        );
        let events = read(&lines);
        assert_eq!(events[0].tokens.cache_read, 9600);
        assert_eq!(events[0].tokens.input, 13459 - 9600);
        // Cached tokens are counted once, at the read rate: the total is the
        // reported input plus output, not input plus output plus the cache.
        assert_eq!(events[0].tokens.total(), 13459 + 62);
    }

    #[test]
    fn a_mid_session_model_switch_reattributes_later_turns() {
        let switch = r#"{"timestamp":"2026-05-11T06:20:00.000Z","type":"turn_context","payload":{"model":"gpt-5.3-codex"}}"#;
        let lines = format!(
            "{CONTEXT}\n{}\n{switch}\n{}\n",
            token_count("2026-05-11T06:18:00.000Z", 100, 0, 10),
            token_count("2026-05-11T06:21:00.000Z", 200, 0, 20),
        );
        let events = read(&lines);
        assert_eq!(events[0].model, "gpt-5.5");
        assert_eq!(events[1].model, "gpt-5.3-codex");
    }

    #[test]
    fn usage_before_the_first_turn_context_is_attributed_to_it() {
        let lines = format!(
            "{}\n{CONTEXT}\n",
            token_count("2026-05-11T06:18:00.000Z", 100, 0, 10)
        );
        let events = read(&lines);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model, "gpt-5.5");
        assert_eq!(events[0].provider, "codex");
    }

    #[test]
    fn a_session_continued_in_a_second_file_bills_only_the_difference() {
        // The cumulative keeps climbing across rollouts. Reading the second
        // file with a fresh counter would bill the session's whole history
        // again as one delta.
        let meta = r#"{"timestamp":"2026-05-11T06:00:00.000Z","type":"session_meta","payload":{"id":"sess-1"}}"#;
        let first = format!(
            "{meta}\n{CONTEXT}\n{}\n",
            token_count("2026-05-11T06:18:00.000Z", 1000, 0, 100)
        );
        let second = format!(
            "{meta}\n{CONTEXT}\n{}\n",
            token_count("2026-05-11T07:18:00.000Z", 2500, 0, 300)
        );

        let dir =
            std::env::temp_dir().join(format!("tokengauge-codex-split-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let a = dir.join("rollout-a.jsonl");
        let b = dir.join("rollout-b.jsonl");
        std::fs::write(&a, first).expect("write a");
        std::fs::write(&b, second).expect("write b");

        let mut seen = HashSet::new();
        let mut totals = HashMap::new();
        let mut events = Vec::new();
        read_file(&a, day(2026, 1, 1), &mut seen, &mut totals, &mut events);
        read_file(&b, day(2026, 1, 1), &mut seen, &mut totals, &mut events);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].tokens.input, 1000);
        assert_eq!(events[1].tokens.input, 1500);
        assert_eq!(events[1].tokens.output, 200);
    }

    #[test]
    fn separate_sessions_keep_separate_running_totals() {
        // Two unrelated sessions must not subtract each other's totals.
        let one = r#"{"timestamp":"2026-05-11T06:00:00.000Z","type":"session_meta","payload":{"id":"sess-1"}}"#;
        let two = r#"{"timestamp":"2026-05-11T06:00:00.000Z","type":"session_meta","payload":{"id":"sess-2"}}"#;
        let dir = std::env::temp_dir().join(format!("tokengauge-codex-two-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let a = dir.join("rollout-a.jsonl");
        let b = dir.join("rollout-b.jsonl");
        std::fs::write(
            &a,
            format!(
                "{one}\n{CONTEXT}\n{}\n",
                token_count("2026-05-11T06:18:00.000Z", 5000, 0, 500)
            ),
        )
        .expect("write a");
        std::fs::write(
            &b,
            format!(
                "{two}\n{CONTEXT}\n{}\n",
                token_count("2026-05-11T06:19:00.000Z", 100, 0, 10)
            ),
        )
        .expect("write b");

        let mut seen = HashSet::new();
        let mut totals = HashMap::new();
        let mut events = Vec::new();
        read_file(&a, day(2026, 1, 1), &mut seen, &mut totals, &mut events);
        read_file(&b, day(2026, 1, 1), &mut seen, &mut totals, &mut events);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(events.len(), 2);
        assert_eq!(events[1].tokens.input, 100);
    }

    #[test]
    fn a_rate_limit_only_emission_is_not_usage() {
        let lines = r#"
{"timestamp":"2026-04-14T08:14:35.831Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"primary":{"used_percent":1.0}}}}
"#;
        assert!(read(lines).is_empty());
    }
}
