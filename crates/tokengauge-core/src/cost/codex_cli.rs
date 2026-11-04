//! Codex rollout transcripts.
//!
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, and the same tree under
//! `~/.codex/archived_sessions`. Codex compresses a finished rollout to
//! `.jsonl.zst` and deletes the plain file, and restores the plain file
//! before it drops the archive when a session resumes, so a rollout is read
//! once: the plain copy over the compressed one, and `sessions` over
//! `archived_sessions`, as ccusage does. A different shape from Claude
//! Code's, with five traps in it:
//!
//! 1. `total_token_usage` is **cumulative for the session**, climbing into the
//!    tens of millions. Its per-event delta is the billed unit.
//! 2. `last_token_usage` looks like that delta and is not: the same event is
//!    re-emitted with the cumulative unchanged, so summing it overcounts. On
//!    this machine one session sums to 89.0M against a true 82.7M.
//! 3. `cached_input_tokens` is a **subset** of `input_tokens`, unlike
//!    Anthropic's separate cache-read field. Billing them as siblings inflates
//!    every Codex figure by roughly the cache hit rate, which is most tokens.
//!    Some builds report the same subset under Anthropic's name instead, so
//!    the populated one is whichever of the two is larger.
//! 4. A reading that moves **backwards** is a stale snapshot re-emitted out of
//!    order. Its delta is zero either way, but latching it lowers the baseline,
//!    and the next reading then bills the whole span again.
//! 5. A one-shot `codex exec` writes no rollout envelope at all: the usage
//!    rides on the line itself, one row per call rather than a cumulative, and
//!    under whichever field names the API that served it used.
//! 6. A subagent rollout opens with its **parent's records copied in** as model
//!    context, `token_count` rows included. `session_meta` marks where the
//!    child's own history starts; before that mark the cumulative belongs to
//!    the parent, and the child opens a counter of its own at zero rather than
//!    continuing it.
//!
//! The model is not in the usage payload either: it lives in `turn_context`,
//! and it changes mid-session.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::Deserialize;

use super::pricing::ServiceTier;
use super::{Firsts, TokenCounts, UsageEvent, dedup_key, first_seen, transcript_files};
use crate::model_to_provider;

#[derive(Debug, Deserialize)]
struct Record {
    #[serde(default)]
    timestamp: String,
    #[serde(default, rename = "type")]
    kind: String,
    /// Position in the rollout. Only a subagent's copied prefix needs it.
    #[serde(default)]
    ordinal: Option<u64>,
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
    /// `session_meta` of a subagent rollout: the first ordinal that is the
    /// child's own. Absent on every ordinary session.
    #[serde(default)]
    subagent_history_start_ordinal: Option<u64>,
    /// `session_meta`: the seat the session ran on, which remuda files a
    /// Codex login under as `<user>__<account>`.
    #[serde(default)]
    creator_user_id: Option<String>,
    #[serde(default)]
    creator_account_id: Option<String>,
    /// `thread_settings_applied` only.
    #[serde(default)]
    thread_settings: Option<ThreadSettings>,
}

#[derive(Debug, Deserialize)]
struct ThreadSettings {
    #[serde(default)]
    service_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenInfo {
    #[serde(default)]
    total_token_usage: Cumulative,
    /// The response this reading closed. Never summed (trap 2); it only says
    /// whether a delta is that one response or several.
    #[serde(default)]
    last_token_usage: Option<Cumulative>,
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
    /// The same subset of `input_tokens` under Anthropic's name, which some
    /// Codex builds emit instead. Only one of the two is ever populated.
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

impl Cumulative {
    /// The cache-read subset, whichever field carried it.
    fn cached(&self) -> u64 {
        self.cached_input_tokens.max(self.cache_read_input_tokens)
    }

    /// A reading that moved backwards in any field restates an earlier state:
    /// a `token_count` written out of order, or a snapshot replayed. Billing it
    /// yields nothing either way, but it must not become the new baseline - the
    /// next reading would then bill everything since the state it restated.
    fn regressed_from(&self, prev: &Cumulative) -> bool {
        self.input_tokens < prev.input_tokens
            || self.output_tokens < prev.output_tokens
            || self.cached() < prev.cached()
    }

    /// What was billed between two readings. Saturating because a session that
    /// restarts its counter must read as "nothing new", never as a negative.
    fn since(&self, prev: &Cumulative) -> TokenCounts {
        let input = self.input_tokens.saturating_sub(prev.input_tokens);
        let cached = self.cached().saturating_sub(prev.cached());
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

/// A rollout line with no `payload`: a one-shot `codex exec` writes the call's
/// usage onto the line itself. Parsed separately from [`Record`] so a stray
/// field on a headless row can never cost us a line the rollout reader handles.
#[derive(Debug, Deserialize)]
struct BareRow {
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<BareUsage>,
    #[serde(default)]
    data: Option<UsageEnvelope>,
    #[serde(default)]
    response: Option<UsageEnvelope>,
    #[serde(default)]
    result: Option<UsageEnvelope>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct UsageEnvelope {
    #[serde(default)]
    usage: Option<BareUsage>,
}

/// One call's usage, named the way the API that served it named it.
///
/// Each spelling is its own field rather than a serde alias: to serde an alias
/// is the same field twice, so a row carrying both `input_tokens` and
/// `prompt_tokens` would fail to parse and be dropped whole - counting nothing,
/// silently, which is the failure this reader exists to end.
#[derive(Debug, Clone, Copy, Deserialize)]
struct BareUsage {
    input_tokens: Option<u64>,
    input: Option<u64>,
    prompt_tokens: Option<u64>,
    output_tokens: Option<u64>,
    output: Option<u64>,
    completion_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

impl BareRow {
    fn usage(&self) -> Option<BareUsage> {
        self.usage
            .or_else(|| self.data.and_then(|e| e.usage))
            .or_else(|| self.response.and_then(|e| e.usage))
            .or_else(|| self.result.and_then(|e| e.usage))
    }
}

impl BareUsage {
    fn input(&self) -> u64 {
        self.input_tokens
            .or(self.input)
            .or(self.prompt_tokens)
            .unwrap_or(0)
    }

    fn output(&self) -> u64 {
        self.output_tokens
            .or(self.output)
            .or(self.completion_tokens)
            .unwrap_or(0)
    }

    fn cached(&self) -> u64 {
        self.cached_tokens
            .or(self.cached_input_tokens)
            .or(self.cache_read_input_tokens)
            .unwrap_or(0)
    }

    /// Per call, not cumulative. The cached subset is inside the input here
    /// too, so it is charged once, at the read rate.
    fn tokens(&self) -> TokenCounts {
        let input = self.input();
        let cached = self.cached().min(input);
        TokenCounts {
            input: input - cached,
            output: self.output(),
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: cached,
        }
    }
}

/// A headless row names the model the way the serving API did -
/// `openai/gpt-5.2-codex`. The price table is keyed on the bare id, which
/// `price_candidates` walks forward from.
fn bare_model_name(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model).trim()
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
        roots.push(Path::new(home.trim()).join("archived_sessions"));
    }
    if roots.is_empty()
        && let Some(home) = dirs::home_dir()
    {
        roots.push(home.join(".codex").join("sessions"));
        roots.push(home.join(".codex").join("archived_sessions"));
    }
    roots.retain(|r| r.is_dir());
    roots
}

fn is_compressed(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "zst")
        && path
            .file_stem()
            .is_some_and(|stem| Path::new(stem).extension().is_some_and(|e| e == "jsonl"))
}

fn is_rollout(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl") || is_compressed(path)
}

/// The rollout a file holds, whichever form it is in.
fn plain_path(path: &Path) -> PathBuf {
    if is_compressed(path) {
        path.with_extension("")
    } else {
        path.to_path_buf()
    }
}

/// A rollout's text. A `.zst` may hold several frames back to back.
fn read_rollout(path: &Path) -> Option<String> {
    if !is_compressed(path) {
        return std::fs::read_to_string(path).ok();
    }
    let compressed = std::fs::read(path).ok()?;
    let mut input = compressed.as_slice();
    let mut text = Vec::new();
    while !input.is_empty() {
        let mut frame = ruzstd::decoding::StreamingDecoder::new(&mut input).ok()?;
        frame.read_to_end(&mut text).ok()?;
    }
    String::from_utf8(text).ok()
}

pub fn read_events(
    roots: &[PathBuf],
    since: NaiveDate,
    seen: &mut HashSet<u64>,
    firsts: &mut Firsts,
    gaps: &mut Vec<PathBuf>,
) -> Vec<UsageEvent> {
    let mut events = Vec::new();
    // Running totals per session, not per file: a session continued into a
    // second rollout picks its cumulative up where the first left off, and
    // starting from zero there would bill its entire history again as one
    // delta. Rollouts are named `rollout-<ISO timestamp>-<id>`, so sorting
    // every root's files by name reads the earlier file first, whether it was
    // archived or not. A rollout in two places is read from the first root,
    // and as `x.jsonl` before `x.jsonl.zst`.
    let mut totals: HashMap<String, Cumulative> = HashMap::new();
    // A rollout by its Codex home and its plain path under `sessions` or
    // `archived_sessions`, so each is read in one form only.
    let mut files: Vec<((PathBuf, PathBuf), usize, PathBuf)> = Vec::new();
    for (order, root) in roots.iter().enumerate() {
        let home = root.parent().unwrap_or(root);
        for file in transcript_files(root, since, gaps, is_rollout) {
            let relative = plain_path(file.strip_prefix(root).unwrap_or(&file));
            files.push(((home.to_path_buf(), relative), order, file));
        }
    }
    files.sort_by(|(a, a_order, a_file), (b, b_order, b_file)| {
        (a.1.file_name(), a_order, a_file).cmp(&(b.1.file_name(), b_order, b_file))
    });
    let mut read: HashSet<(PathBuf, PathBuf)> = HashSet::new();
    let mut unread: Vec<((PathBuf, PathBuf), PathBuf)> = Vec::new();
    for (rollout, _, file) in files {
        if read.contains(&rollout) {
            continue;
        }
        if read_file(&file, since, seen, &mut totals, firsts, &mut events) {
            read.insert(rollout);
        } else {
            unread.push((rollout, file));
        }
    }
    gaps.extend(
        unread
            .into_iter()
            .filter(|(rollout, _)| !read.contains(rollout))
            .map(|(_, file)| file),
    );
    events
}

pub(super) fn read_file(
    path: &Path,
    since: NaiveDate,
    seen: &mut HashSet<u64>,
    totals: &mut HashMap<String, Cumulative>,
    firsts: &mut Firsts,
    events: &mut Vec<UsageEvent>,
) -> bool {
    let Some(contents) = read_rollout(path) else {
        return false;
    };

    // `events` is the accumulator shared by every file in the read, so the
    // fix-up at the end must touch only what this call appended. Rescanning the
    // whole vector once per file is quadratic in the number of transcripts.
    let appended_from = events.len();
    // A rollout with no session_meta is its own bucket; there is nothing to
    // continue it from.
    // Named as the plain file, so a rollout compressed or archived between two
    // reads keeps the dedup keys built from it.
    let plain = plain_path(path);
    let mut session_id = plain
        .file_name()
        .unwrap_or(plain.as_os_str())
        .to_string_lossy()
        .into_owned();
    let mut model: Option<String> = None;
    let mut tier = ServiceTier::Standard;
    // Index into `events` of rows emitted before any `turn_context` announced a
    // model. A session states its model on the first turn, so these are filled
    // in from the first one seen rather than dropped.
    let mut pending: Vec<usize> = Vec::new();
    // A headless row need not carry a timestamp of its own; it belongs to the
    // last line that did, whatever kind of line that was - an exec rollout
    // opens with `turn_context` and can bill before any usage row is seen. Its ordinal within the file is what tells two
    // otherwise identical calls apart, and it has to be stable across runs
    // because fleet sync dedupes on the key built from it.
    let mut last_at: Option<DateTime<Utc>> = None;
    let mut bare_rows = 0usize;
    // Set only by a subagent rollout. See trap 6.
    let mut subagent_start: Option<u64> = None;
    let mut login: Option<String> = None;

    for line in contents.lines() {
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            continue;
        };
        if let Ok(at) = record.timestamp.parse::<DateTime<Utc>>() {
            last_at = Some(at);
        }
        let Some(payload) = record.payload.as_ref() else {
            let Ok(bare) = serde_json::from_str::<BareRow>(line) else {
                continue;
            };
            let Some(usage) = bare.usage() else {
                continue;
            };
            bare_rows += 1;
            let tokens = usage.tokens();
            if tokens.total() == 0 {
                continue;
            }
            // Absent is inherited; malformed is not. Filing a call under the
            // day of the row before it is a guess, and the rollout reader
            // drops an unparseable timestamp rather than guessing too.
            let at = if bare.timestamp.trim().is_empty() {
                last_at
            } else {
                bare.timestamp.parse::<DateTime<Utc>>().ok()
            };
            let Some(at) = at else {
                continue;
            };
            first_seen(firsts, &session_id, at);
            let date = at.with_timezone(&Local).date_naive();
            if date < since {
                continue;
            }
            let key = dedup_key(
                &session_id,
                &format!("exec|{bare_rows}|{}", at.to_rfc3339()),
            );
            if !seen.insert(key) {
                continue;
            }
            let name = match bare
                .model
                .as_deref()
                .map(bare_model_name)
                .filter(|m| !m.is_empty())
            {
                Some(named) => named.to_string(),
                None => {
                    if model.is_none() {
                        pending.push(events.len());
                    }
                    model.clone().unwrap_or_default()
                }
            };
            events.push(UsageEvent {
                provider: model_to_provider(&name).unwrap_or("codex"),
                model: name,
                date,
                at,
                tokens,
                key: Some(key),
                one_request: false,
                service_tier: ServiceTier::Standard,
                session: Some(session_id.clone()),
                starts_session: false,
                login: login.clone(),
                source: crate::cost::Source::Cli,
            });
            continue;
        };

        if record.kind == "session_meta" {
            if let Some(id) = payload.id.as_deref().filter(|id| !id.is_empty()) {
                session_id = id.to_string();
            }
            if let Some(start) = payload.subagent_history_start_ordinal {
                subagent_start = Some(start);
            }
            if let (Some(user), Some(account)) = (
                payload.creator_user_id.as_deref().filter(|u| !u.is_empty()),
                payload
                    .creator_account_id
                    .as_deref()
                    .filter(|a| !a.is_empty()),
            ) {
                login = Some(selvedge::usage::account_digest(&format!(
                    "{user}__{account}"
                )));
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
        // Fast mode, and a turn resumed on another tier. Settings that name no
        // tier leave the one before standing; Codex writes those for its
        // auto-review threads.
        if payload.kind == "thread_settings_applied" {
            if let Some(name) = payload
                .thread_settings
                .as_ref()
                .and_then(|settings| settings.service_tier.as_deref())
            {
                tier = ServiceTier::from_codex(name).unwrap_or_default();
            }
            continue;
        }
        if payload.kind != "token_count" {
            continue;
        }
        let Some(info) = payload.info.as_ref() else {
            continue;
        };

        // Inherited context, not this session's spend. Dropped outright rather
        // than latched as a baseline: the child counts from zero, so the
        // parent's cumulative sitting in `totals` would read as a regression
        // against every reading the child goes on to write, and the session
        // would bill nothing at all. A row with no ordinal cannot be shown to
        // be the child's, and reads as inherited for the same reason.
        if subagent_start.is_some_and(|start| record.ordinal.is_none_or(|at| at < start)) {
            continue;
        }

        let cumulative = info.total_token_usage;
        let previous = totals.entry(session_id.clone()).or_default();
        // Checked before the latch, never after: a stale reading that became
        // the baseline is what makes the next one bill the span twice.
        if cumulative.regressed_from(previous) {
            continue;
        }
        let tokens = cumulative.since(previous);
        // A delta spans several responses when a reading in between was
        // skipped; only one that is exactly the last response can be tiered.
        let one_request = info.last_token_usage.is_some_and(|last| {
            last.input_tokens
                == cumulative
                    .input_tokens
                    .saturating_sub(previous.input_tokens)
                && last.output_tokens
                    == cumulative
                        .output_tokens
                        .saturating_sub(previous.output_tokens)
        });
        *previous = cumulative;
        if tokens.total() == 0 {
            continue;
        }

        let Ok(timestamp) = record.timestamp.parse::<DateTime<Utc>>() else {
            continue;
        };
        first_seen(firsts, &session_id, timestamp);
        let date = timestamp.with_timezone(&Local).date_naive();
        if date < since {
            continue;
        }

        // A resumed session replayed into a second file would repeat the same
        // cumulative readings; the session and its running total identify one.
        let key = dedup_key(
            &session_id,
            &format!("{}|{}", record.timestamp, cumulative.total_tokens),
        );
        if !seen.insert(key) {
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
            key: Some(key),
            one_request,
            service_tier: tier,
            session: Some(session_id.clone()),
            starts_session: false,
            login: login.clone(),
            source: crate::cost::Source::Cli,
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
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    fn read(lines: &str) -> Vec<UsageEvent> {
        read_since(lines, day(2026, 1, 1))
    }

    fn read_since(lines: &str, since: NaiveDate) -> Vec<UsageEvent> {
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
        read_file(
            &path,
            since,
            &mut seen,
            &mut totals,
            &mut Firsts::new(),
            &mut events,
        );
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

    fn reading(ts: &str, total: (u64, u64), last: (u64, u64)) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{},"output_tokens":{}}},"last_token_usage":{{"input_tokens":{},"output_tokens":{}}}}}}}}}"#,
            total.0, total.1, last.0, last.1
        )
    }

    /// A delta is one response only when it is exactly the response the
    /// reading closed. Across a skipped reading it is two, and a long-context
    /// tier applied to the pair would bill both at the premium.
    #[test]
    fn a_delta_is_one_request_only_when_it_is_the_last_response() {
        let lines = [
            CONTEXT,
            &reading("2026-05-11T06:18:00.000Z", (1_000, 100), (1_000, 100)),
            &reading("2026-05-11T06:19:00.000Z", (3_000, 300), (2_000, 200)),
            // Moved backwards: skipped, and not latched.
            &reading("2026-05-11T06:20:00.000Z", (2_000, 200), (500, 50)),
            &reading("2026-05-11T06:21:00.000Z", (7_000, 700), (1_000, 100)),
        ]
        .join("\n");
        let events = read(&lines);
        let one: Vec<bool> = events.iter().map(|e| e.one_request).collect();
        assert_eq!(one, [true, true, false]);
    }

    #[test]
    fn a_codex_session_is_the_login_of_the_seat_it_names() {
        let meta = r#"{"timestamp":"2026-05-11T06:17:00.000Z","type":"session_meta","payload":{"id":"sess-1","creator_user_id":"user-1","creator_account_id":"acct-1"}}"#;
        let lines = [
            meta,
            CONTEXT,
            &token_count("2026-05-11T06:18:00.000Z", 100, 0, 10),
        ]
        .join("\n");
        let events = read(&lines);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session.as_deref(), Some("sess-1"));
        assert_eq!(
            events[0].login,
            Some(selvedge::usage::account_digest("user-1__acct-1"))
        );

        let anonymous = [
            CONTEXT,
            &token_count("2026-05-11T06:18:00.000Z", 100, 0, 10),
        ]
        .join("\n");
        assert_eq!(read(&anonymous)[0].login, None);
    }

    fn ordinal_token_count(ordinal: u64, ts: &str, input: u64, output: u64) -> String {
        let row = token_count(ts, input, 0, output);
        format!(r#"{{"ordinal":{ordinal},{}"#, &row[1..])
    }

    /// A subagent rollout opens with the parent's records copied in. Billing
    /// the copied prefix charges the child for context it inherited: here the
    /// parent's 900K/90K, which is all of the session's apparent spend.
    #[test]
    fn an_inherited_prefix_is_not_the_subagents_spend() {
        let meta = r#"{"ordinal":0,"timestamp":"2026-05-11T06:00:00.000Z","type":"session_meta","payload":{"id":"sub-1","subagent_history_start_ordinal":3}}"#;
        let lines = [
            meta,
            CONTEXT,
            &ordinal_token_count(1, "2026-05-11T06:10:00.000Z", 900_000, 90_000),
            &ordinal_token_count(2, "2026-05-11T06:11:00.000Z", 950_000, 95_000),
            &ordinal_token_count(3, "2026-05-11T06:18:00.000Z", 1_000, 100),
            &ordinal_token_count(4, "2026-05-11T06:19:00.000Z", 2_500, 300),
        ]
        .join("\n");

        let events = read(&lines);
        let input: u64 = events.iter().map(|e| e.tokens.input).sum();
        let output: u64 = events.iter().map(|e| e.tokens.output).sum();
        // The child's own two readings: 1000 then 2500 cumulative, so 2500
        // billed once. Not 2500 + the 950K it was handed.
        assert_eq!(input, 2_500, "inherited input billed to the child");
        assert_eq!(output, 300);
    }

    /// The boundary is authoritative even when the file stops before the child
    /// writes anything of its own, which is how a subagent that was cancelled
    /// early lands on disk.
    #[test]
    fn a_prefix_only_subagent_rollout_bills_nothing() {
        let meta = r#"{"ordinal":0,"timestamp":"2026-05-11T06:00:00.000Z","type":"session_meta","payload":{"id":"sub-2","subagent_history_start_ordinal":9}}"#;
        let lines = [
            meta,
            CONTEXT,
            &ordinal_token_count(1, "2026-05-11T06:10:00.000Z", 900_000, 90_000),
        ]
        .join("\n");
        assert!(read(&lines).is_empty());
    }

    /// An ordinary session carries no boundary, and its ordinals must not be
    /// read as a prefix. This is the regression that would silently zero every
    /// Codex figure.
    #[test]
    fn a_session_without_a_boundary_bills_every_row() {
        let meta = r#"{"ordinal":0,"timestamp":"2026-05-11T06:00:00.000Z","type":"session_meta","payload":{"id":"plain-1"}}"#;
        let lines = [
            meta,
            CONTEXT,
            &ordinal_token_count(1, "2026-05-11T06:18:00.000Z", 1_000, 100),
            &ordinal_token_count(2, "2026-05-11T06:19:00.000Z", 2_500, 300),
        ]
        .join("\n");
        let events = read(&lines);
        let input: u64 = events.iter().map(|e| e.tokens.input).sum();
        assert_eq!(input, 2_500);
    }

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

    fn compress(text: &str) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(
            text.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        )
    }

    fn rollout(id: &str, input: u64) -> String {
        format!(
            "{{\"timestamp\":\"2026-05-11T06:17:00.000Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\"}}}}\n{CONTEXT}\n{}\n",
            token_count("2026-05-11T06:18:00.000Z", input, 0, 10),
        )
    }

    /// A Codex home with `sessions` and `archived_sessions`, each file given
    /// as its path under the home and its bytes.
    fn home(name: &str, files: &[(&str, Vec<u8>)]) -> (PathBuf, Vec<PathBuf>) {
        let dir = std::env::temp_dir().join(format!(
            "tokengauge-codex-home-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, bytes) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let roots = ["sessions", "archived_sessions"]
            .iter()
            .map(|r| dir.join(r))
            .filter(|r| r.is_dir())
            .collect();
        (dir, roots)
    }

    fn inputs(roots: &[PathBuf]) -> Vec<u64> {
        let mut gaps = Vec::new();
        let mut inputs: Vec<u64> = read_events(
            roots,
            day(2026, 1, 1),
            &mut HashSet::new(),
            &mut Firsts::new(),
            &mut gaps,
        )
        .iter()
        .map(|e| e.tokens.input)
        .collect();
        assert!(gaps.is_empty(), "{gaps:?}");
        inputs.sort();
        inputs
    }

    #[test]
    fn a_compressed_rollout_reads_as_its_plain_text() {
        let text = rollout("zst", 1000);
        let (dir, roots) = home(
            "zst",
            &[("sessions/2026/05/11/rollout-a.jsonl.zst", compress(&text))],
        );
        assert_eq!(inputs(&roots), vec![1000]);

        let mut frames = compress(&text[..text.len() / 2]);
        frames.extend(compress(&text[text.len() / 2..]));
        assert_eq!(
            read_rollout_bytes(&dir, "frames.jsonl.zst", &frames).as_deref(),
            Some(text.as_str())
        );
        assert_eq!(
            read_rollout_bytes(&dir, "junk.jsonl.zst", b"not zstd"),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn read_rollout_bytes(dir: &Path, name: &str, bytes: &[u8]) -> Option<String> {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        read_rollout(&path)
    }

    /// Codex keeps both forms for a while, and ccusage reads one of them.
    #[test]
    fn a_rollout_is_read_once_whichever_forms_and_directories_hold_it() {
        let (dir, roots) = home(
            "once",
            &[
                ("sessions/both.jsonl", rollout("both", 1).into_bytes()),
                ("sessions/both.jsonl.zst", compress(&rollout("both-zst", 2))),
                ("sessions/compressed.jsonl.zst", compress(&rollout("c", 3))),
                ("sessions/other.zst", compress(&rollout("other", 4))),
                (
                    "archived_sessions/compressed.jsonl",
                    rollout("c-arch", 5).into_bytes(),
                ),
                (
                    "archived_sessions/both.jsonl.zst",
                    compress(&rollout("b-arch", 6)),
                ),
                (
                    "archived_sessions/archived.jsonl.zst",
                    compress(&rollout("a", 7)),
                ),
            ],
        );
        assert_eq!(inputs(&roots), vec![1, 3, 7]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rollout_without_a_session_keeps_its_id_once_compressed() {
        let text = format!(
            "{CONTEXT}\n{}\n",
            token_count("2026-05-11T06:18:00.000Z", 100, 0, 10)
        );
        let session = |file: &str, bytes: Vec<u8>| {
            let (dir, roots) = home("no-session", &[(file, bytes)]);
            let read = read_events(
                &roots,
                day(2026, 1, 1),
                &mut HashSet::new(),
                &mut Firsts::new(),
                &mut Vec::new(),
            );
            let _ = std::fs::remove_dir_all(&dir);
            read[0].session.clone().expect("a session")
        };
        let plain = session("sessions/r.jsonl", text.clone().into_bytes());
        let compressed = session("sessions/r.jsonl.zst", compress(&text));
        let archived = session("archived_sessions/r.jsonl", text.clone().into_bytes());
        assert!(plain.ends_with("r.jsonl"), "{plain}");
        assert_eq!(plain, compressed);
        assert_eq!(plain, archived);
    }

    /// A session resumed after it was archived continues from the archived
    /// rollout, whichever root is listed first.
    #[test]
    fn an_archived_rollout_is_read_before_the_session_resumed_from_it() {
        let part = |ts: &str, input: u64| {
            let meta = format!(
                r#"{{"timestamp":"{ts}","type":"session_meta","payload":{{"id":"resumed"}}}}"#
            );
            format!("{meta}\n{CONTEXT}\n{}\n", token_count(ts, input, 0, 0)).into_bytes()
        };
        let (dir, roots) = home(
            "resumed",
            &[
                (
                    "sessions/2026/05/11/rollout-2026-05-11T12-00-00-resumed.jsonl",
                    part("2026-05-11T12:00:00.000Z", 150),
                ),
                (
                    "archived_sessions/rollout-2026-05-09T12-00-00-resumed.jsonl",
                    part("2026-05-09T12:00:00.000Z", 100),
                ),
            ],
        );
        let mut by_day: Vec<(NaiveDate, u64)> = read_events(
            &roots,
            day(2026, 1, 1),
            &mut HashSet::new(),
            &mut Firsts::new(),
            &mut Vec::new(),
        )
        .iter()
        .map(|e| (e.date, e.tokens.input))
        .collect();
        by_day.sort();
        assert_eq!(by_day, vec![(day(2026, 5, 9), 100), (day(2026, 5, 11), 50)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rollout_whose_plain_copy_cannot_be_read_is_read_compressed() {
        let (dir, roots) = home(
            "unreadable",
            &[
                ("sessions/r.jsonl", b"\xff\xfe".to_vec()),
                ("sessions/r.jsonl.zst", compress(&rollout("r", 9))),
            ],
        );
        assert_eq!(inputs(&roots), vec![9]);
        let _ = std::fs::remove_dir_all(&dir);

        let (dir, roots) = home(
            "unreadable-both",
            &[
                ("sessions/r.jsonl", b"\xff\xfe".to_vec()),
                ("sessions/r.jsonl.zst", b"not zstd".to_vec()),
            ],
        );
        let mut gaps = Vec::new();
        let read = read_events(
            &roots,
            day(2026, 1, 1),
            &mut HashSet::new(),
            &mut Firsts::new(),
            &mut gaps,
        );
        assert!(read.is_empty());
        assert_eq!(gaps.len(), 2, "{gaps:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn settings(ts: &str, tier: Option<&str>) -> String {
        let tier = tier
            .map(|t| format!(r#","service_tier":"{t}""#))
            .unwrap_or_default();
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"thread_settings_applied","thread_settings":{{"model":"gpt-5.5"{tier}}}}}}}"#
        )
    }

    /// CodexBar #4814: Fast mode is recorded per thread, and a turn resumed
    /// on another tier switches it.
    #[test]
    fn a_call_is_tagged_with_the_tier_its_thread_ran_on() {
        let lines = [
            CONTEXT.to_string(),
            token_count("2026-05-11T06:18:00.000Z", 100, 0, 10),
            settings("2026-05-11T06:18:30.000Z", Some("priority")),
            token_count("2026-05-11T06:19:00.000Z", 200, 0, 20),
            settings("2026-05-11T06:19:30.000Z", None),
            token_count("2026-05-11T06:20:00.000Z", 300, 0, 30),
            settings("2026-05-11T06:20:30.000Z", Some("default")),
            token_count("2026-05-11T06:21:00.000Z", 400, 0, 40),
            settings("2026-05-11T06:21:30.000Z", Some("fast")),
            settings("2026-05-11T06:21:40.000Z", Some("hyperspeed")),
            token_count("2026-05-11T06:22:00.000Z", 500, 0, 50),
        ]
        .join("\n");
        let tiers: Vec<ServiceTier> = read(&lines).iter().map(|e| e.service_tier).collect();
        assert_eq!(
            tiers,
            vec![
                ServiceTier::Standard,
                ServiceTier::Priority,
                ServiceTier::Priority,
                ServiceTier::Standard,
                ServiceTier::Standard,
            ]
        );
    }

    /// The since filter has to run *after* the running total is advanced, or a
    /// session that started before the window absorbs all of its own history
    /// into the first reading inside it. Every other test here uses a `since`
    /// old enough to include the whole fixture, so nothing else would notice
    /// the two lines being swapped.
    #[test]
    fn history_before_the_window_is_not_billed_to_the_first_day_inside_it() {
        let lines = format!(
            "{CONTEXT}\n{}\n{}\n",
            // Well before the window: 900k tokens this session already spent.
            token_count("2026-05-01T06:18:00.000Z", 900_000, 0, 90_000),
            // The first reading inside it. Its delta is 1,000 in and 100 out.
            token_count("2026-05-11T06:19:00.000Z", 901_000, 0, 90_100),
        );
        let events = read_since(&lines, day(2026, 5, 10));

        assert_eq!(events.len(), 1, "only the in-window reading is billed");
        assert_eq!(events[0].tokens.input, 1_000);
        assert_eq!(events[0].tokens.output, 100);
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
        read_file(
            &a,
            day(2026, 1, 1),
            &mut seen,
            &mut totals,
            &mut Firsts::new(),
            &mut events,
        );
        read_file(
            &b,
            day(2026, 1, 1),
            &mut seen,
            &mut totals,
            &mut Firsts::new(),
            &mut events,
        );
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
        read_file(
            &a,
            day(2026, 1, 1),
            &mut seen,
            &mut totals,
            &mut Firsts::new(),
            &mut events,
        );
        read_file(
            &b,
            day(2026, 1, 1),
            &mut seen,
            &mut totals,
            &mut Firsts::new(),
            &mut events,
        );
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(events.len(), 2);
        assert_eq!(events[1].tokens.input, 100);
    }

    #[test]
    fn a_cache_read_named_field_is_the_same_subset() {
        // Trap 3's other half: some builds name the subset the way Anthropic
        // does. Read as a sibling it would price every cache read as fresh
        // input; ignored outright it does the same.
        let line = r#"{"timestamp":"2026-05-11T06:18:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":13459,"cache_read_input_tokens":9600,"output_tokens":62,"total_tokens":13521}}}}"#;
        let events = read(&format!("{CONTEXT}\n{line}\n"));
        assert_eq!(events[0].tokens.cache_read, 9600);
        assert_eq!(events[0].tokens.input, 13459 - 9600);
        assert_eq!(events[0].tokens.total(), 13459 + 62);
    }

    #[test]
    fn a_stale_reading_does_not_lower_the_baseline() {
        // Trap 4. The middle row restates a state already billed. Latching it
        // would make the last row bill 2,000 input instead of 1,500 - the
        // 1,000 of the first row charged a second time.
        let lines = format!(
            "{CONTEXT}\n{}\n{}\n{}\n",
            token_count("2026-05-11T06:18:00.000Z", 1000, 0, 100),
            token_count("2026-05-11T06:18:30.000Z", 500, 0, 50),
            token_count("2026-05-11T06:19:00.000Z", 2500, 0, 300),
        );
        let events = read(&lines);
        assert_eq!(events.len(), 2, "the stale reading bills nothing");
        assert_eq!(events[1].tokens.input, 1500);
        assert_eq!(events[1].tokens.output, 200);
        let input: u64 = events.iter().map(|e| e.tokens.input).sum();
        assert_eq!(input, 2500, "the session's own cumulative, billed once");
    }

    #[test]
    fn a_headless_exec_row_is_billed_per_call() {
        // Trap 5. No rollout envelope, no cumulative: each row is one call,
        // under the field names of whichever API served it. Two identical rows
        // are two calls, which is the opposite of the re-emission trap above.
        let exec = r#"{"timestamp":"2026-05-11T06:18:00.000Z","model":"openai/gpt-5.2-codex","usage":{"prompt_tokens":120,"completion_tokens":30,"cached_tokens":20}}"#;
        let nested =
            r#"{"timestamp":"2026-05-11T06:19:00.000Z","data":{"usage":{"input":10,"output":4}}}"#;
        let events = read(&format!("{CONTEXT}\n{exec}\n{exec}\n{nested}\n"));

        assert_eq!(events.len(), 3);
        assert_eq!(
            events[0].model, "gpt-5.2-codex",
            "the vendor path is dropped"
        );
        assert_eq!(events[0].tokens.input, 100);
        assert_eq!(events[0].tokens.cache_read, 20);
        assert_eq!(events[0].tokens.output, 30);
        assert_eq!(events[1].tokens.total(), events[0].tokens.total());
        // A row that names no model falls back to the turn in flight.
        assert_eq!(events[2].model, "gpt-5.5");
        assert_eq!(events[2].tokens.input, 10);
        assert_eq!(events[2].tokens.output, 4);
    }

    #[test]
    fn a_headless_row_spelling_one_count_twice_is_still_read() {
        // A serde alias is the same field twice, so a row emitting both
        // spellings for compatibility would fail to parse and be dropped
        // whole - counting nothing, with nothing to show it had.
        let both = r#"{"timestamp":"2026-05-11T06:18:00.000Z","usage":{"input_tokens":40,"prompt_tokens":40,"output_tokens":5,"completion_tokens":5}}"#;
        let events = read(&format!("{CONTEXT}\n{both}\n"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tokens.input, 40);
        assert_eq!(events[0].tokens.output, 5);
    }

    #[test]
    fn a_headless_row_inherits_the_day_of_any_line_that_carried_one() {
        // The anchor is the last line with a timestamp, not the last billed
        // row: an exec rollout opens with `turn_context`, so the first call
        // can be the one with no timestamp of its own.
        let bare = r#"{"result":{"usage":{"input_tokens":5,"output_tokens":1}}}"#;
        let events = read(&format!("{CONTEXT}\n{bare}\n"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tokens.input, 5);
    }

    #[test]
    fn a_headless_row_with_a_malformed_timestamp_is_dropped() {
        // Not the same as carrying none: a date that failed to parse says
        // nothing about which day the call belongs to, and the row before it
        // is not an answer.
        let stamped = r#"{"timestamp":"2026-05-11T06:18:00.000Z","usage":{"input_tokens":7,"output_tokens":3}}"#;
        let bad = r#"{"timestamp":"not-a-date","usage":{"input_tokens":5,"output_tokens":1}}"#;
        let events = read(&format!("{CONTEXT}\n{stamped}\n{bad}\n"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tokens.input, 7);
    }

    #[test]
    fn a_headless_row_without_a_timestamp_joins_the_last_one() {
        let stamped = r#"{"timestamp":"2026-05-11T06:18:00.000Z","response":{"usage":{"input_tokens":7,"output_tokens":3}}}"#;
        let bare = r#"{"result":{"usage":{"input_tokens":5,"output_tokens":1}}}"#;
        let events = read(&format!("{CONTEXT}\n{stamped}\n{bare}\n"));

        assert_eq!(events.len(), 2, "a timestamp-less row is not dropped");
        assert_eq!(events[1].date, events[0].date);
        assert_eq!(events[1].tokens.input, 5);
    }

    #[test]
    fn a_rate_limit_only_emission_is_not_usage() {
        let lines = r#"
{"timestamp":"2026-04-14T08:14:35.831Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"primary":{"used_percent":1.0}}}}
"#;
        assert!(read(lines).is_empty());
    }
}
