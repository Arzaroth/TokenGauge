//! The ccusage fallback, and the cross-check that keeps it earning its keep.
//!
//! ccusage reads 22 agent formats where the native readers parse two transcript
//! trees, so a Kimi or Grok plan driven from its own CLI still gets a cost row.
//! It is the fallback and the second opinion, not the source: `cost_source =
//! "auto"` reads natively and asks ccusage only about enabled providers the
//! readers found nothing for, so a Claude/Codex machine never spawns it.
//!
//! Everything here is subprocess and JSON shape. The one deadline at the top of
//! `fetch_ccusage_costs` covers every call it makes, retries included, because
//! three separate timeouts is three ways to hang a bar for a minute.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chrono::{Local, NaiveDate};
use serde::Deserialize;

use crate::*;
use chrono::Days;

/// Map a ccusage model name to a TokenGauge provider key.
/// Returns None if the model doesn't belong to a tracked provider.
pub fn model_to_provider(model: &str) -> Option<&'static str> {
    let lower = model.to_lowercase();
    if lower.starts_with("claude") {
        Some("claude")
    } else if lower.starts_with("gpt")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
        || lower.starts_with("codex")
        || lower.starts_with("openai")
    {
        Some("codex")
    } else if lower.starts_with("kimi") || lower.starts_with("moonshot") {
        Some("kimi")
    } else if lower.starts_with("grok") {
        Some("grok")
    } else if lower.starts_with("glm") {
        Some("glm")
    } else {
        None
    }
}

/// Map a ccusage agent name (the `--by-agent` split) to a provider key.
///
/// Only consulted when the model name is not conclusive, because the agent is
/// the coarser signal: a GLM or Kimi model driven through Claude Code is
/// reported under the `claude` agent, and it is the model that says whose
/// money it was.
fn agent_to_provider(agent: &str) -> Option<&'static str> {
    match agent.to_lowercase().as_str() {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "kimi" => Some("kimi"),
        "grok" => Some("grok"),
        _ => None,
    }
}

/// Length of the rolling cost history, in days.
pub(crate) const WEEKLY_HISTORY_DAYS: usize = 7;

#[derive(Debug, Clone, Default, Deserialize)]
struct CcusageDailyResponse {
    #[serde(default)]
    daily: Vec<CcusageDay>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageDay {
    #[serde(default)]
    agents: Vec<CcusageAgent>,
    #[serde(default)]
    model_breakdowns: Vec<CcusageModelBreakdown>,
    #[serde(default)]
    period: String,
}

/// One agent's slice of a day, from `--by-agent`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageAgent {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    model_breakdowns: Vec<CcusageModelBreakdown>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageModelBreakdown {
    model_name: String,
    #[serde(default)]
    cost: f64,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_tokens: u64,
    #[serde(default)]
    cache_read_tokens: u64,
}

/// Every breakdown in a day, paired with the provider it belongs to.
///
/// `agents` and `model_breakdowns` carry the same spend - the flat list is the
/// per-agent one merged - so exactly one of them is read, never both. ccusage
/// builds too old for `--by-agent` only emit the flat list, and there a model
/// name that maps nowhere (a Kimi or Grok run) is simply dropped, as before.
fn day_breakdowns(day: &CcusageDay) -> Vec<(&'static str, &CcusageModelBreakdown)> {
    if day.agents.is_empty() {
        return day
            .model_breakdowns
            .iter()
            .filter_map(|b| Some((model_to_provider(&b.model_name)?, b)))
            .collect();
    }
    day.agents
        .iter()
        .flat_map(|a| {
            a.model_breakdowns.iter().filter_map(|b| {
                let provider =
                    model_to_provider(&b.model_name).or_else(|| agent_to_provider(&a.agent))?;
                Some((provider, b))
            })
        })
        .collect()
}

fn ccusage_total_tokens(b: &CcusageModelBreakdown) -> u64 {
    b.input_tokens + b.output_tokens + b.cache_creation_tokens + b.cache_read_tokens
}

/// Running per-model totals, minus the model name (it is the map key).
#[derive(Default)]
struct ModelTotals {
    usd: f64,
    tokens: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
}

impl ModelTotals {
    fn add(&mut self, b: &CcusageModelBreakdown) {
        self.usd += b.cost;
        self.tokens += ccusage_total_tokens(b);
        self.input_tokens += b.input_tokens;
        self.output_tokens += b.output_tokens;
        self.cache_creation_tokens += b.cache_creation_tokens;
        self.cache_read_tokens += b.cache_read_tokens;
    }
}

struct AggregatedProvider {
    total_usd: f64,
    total_tokens: u64,
    models: HashMap<String, ModelTotals>,
}

impl AggregatedProvider {
    fn into_model_costs(self) -> (f64, u64, Vec<ModelCost>) {
        let mut models: Vec<ModelCost> = self
            .models
            .into_iter()
            .map(|(model, t)| ModelCost {
                model,
                usd: t.usd,
                tokens: t.tokens,
                input_tokens: t.input_tokens,
                output_tokens: t.output_tokens,
                cache_creation_tokens: t.cache_creation_tokens,
                cache_read_tokens: t.cache_read_tokens,
                by_device: Vec::new(),
            })
            .collect();
        models.sort_by(|a, b| {
            b.usd
                .partial_cmp(&a.usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        (self.total_usd, self.total_tokens, models)
    }
}

/// The last `n` calendar days ending at `today`, oldest first.
///
/// ccusage omits days with no usage entirely, so the window has to be built
/// from the calendar rather than from the response: an idle day is $0 spent,
/// not a day that did not happen, and dropping it silently shortens the series
/// and shifts every label in a chart drawn from it.
pub(crate) fn recent_periods(today: NaiveDate, n: usize) -> Vec<String> {
    (0..n)
        .rev()
        .filter_map(|offset| today.checked_sub_days(Days::new(offset as u64)))
        .map(|date| date.format("%Y-%m-%d").to_string())
        .collect()
}

/// Last `n` calendar days of cost and tokens per provider, oldest first. Every
/// provider's series covers the same dates, zero-filled where it spent nothing.
/// One day of one provider, while it is still being summed.
#[derive(Debug, Default)]
struct DayAccum {
    usd: f64,
    tokens: u64,
    /// model -> (usd, tokens)
    models: HashMap<String, (f64, u64)>,
}

fn last_n_days_by_provider(
    response: &CcusageDailyResponse,
    today: NaiveDate,
    n: usize,
) -> HashMap<String, Vec<DayCost>> {
    // provider -> period -> that day
    let mut per_day: HashMap<String, HashMap<String, DayAccum>> = HashMap::new();
    for day in &response.daily {
        if day.period.is_empty() {
            continue;
        }
        for (provider, b) in day_breakdowns(day) {
            let entry = per_day
                .entry(provider.to_string())
                .or_default()
                .entry(day.period.clone())
                .or_default();
            let tokens = ccusage_total_tokens(b);
            entry.usd += b.cost;
            entry.tokens += tokens;
            let model = entry.models.entry(b.model_name.clone()).or_insert((0.0, 0));
            model.0 += b.cost;
            model.1 += tokens;
        }
    }
    let periods = recent_periods(today, n);
    per_day
        .into_iter()
        .map(|(provider, days)| {
            let mut days = days;
            let series: Vec<DayCost> = periods
                .iter()
                .map(|p| {
                    let day = days.remove(p).unwrap_or_default();
                    DayCost {
                        date: p.clone(),
                        usd: day.usd,
                        tokens: day.tokens,
                        by_device: Vec::new(),
                        by_model: DayModelCost::top(
                            day.models
                                .into_iter()
                                .map(|(model, (usd, tokens))| DayModelCost { model, usd, tokens })
                                .collect(),
                        ),
                    }
                })
                .collect();
            (provider, series)
        })
        .collect()
}

/// Aggregate the days whose `period` falls within `since..=until` (inclusive,
/// `YYYY-MM-DD`, compared lexicographically - which is a date comparison in
/// that format). One ccusage call now covers today, the month and the rolling
/// week, and each figure slices the window it needs out of the same response.
fn aggregate_ccusage(
    response: &CcusageDailyResponse,
    since: &str,
    until: &str,
) -> HashMap<String, AggregatedProvider> {
    let mut totals: HashMap<String, AggregatedProvider> = HashMap::new();
    for day in &response.daily {
        if day.period.as_str() < since || day.period.as_str() > until {
            continue;
        }
        for (provider, b) in day_breakdowns(day) {
            let entry = totals
                .entry(provider.to_string())
                .or_insert_with(|| AggregatedProvider {
                    total_usd: 0.0,
                    total_tokens: 0,
                    models: HashMap::new(),
                });
            entry.total_usd += b.cost;
            entry.total_tokens += ccusage_total_tokens(b);
            entry.models.entry(b.model_name.clone()).or_default().add(b);
        }
    }
    totals
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageBlocksResponse {
    #[serde(default)]
    blocks: Vec<CcusageBlock>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageBlock {
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    burn_rate: Option<CcusageBurnRate>,
    #[serde(default)]
    projection: Option<CcusageProjection>,
    #[serde(default, rename = "costUSD")]
    cost_usd: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageBurnRate {
    cost_per_hour: f64,
    #[serde(default)]
    tokens_per_minute: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageProjection {
    #[serde(default)]
    remaining_minutes: u32,
    #[serde(default)]
    total_cost: f64,
}

/// Resolve which command launches ccusage on this host.
/// Order: direct `ccusage` (global npm/bun/AUR install) → `bunx ccusage` →
/// `npx --yes ccusage` (Node.js fallback). First one whose binary is on PATH
/// is used. Returns None if no runner is available.
fn resolve_ccusage_runner() -> Option<Vec<String>> {
    if binary_on_path("ccusage") {
        return Some(vec!["ccusage".into()]);
    }
    if binary_on_path("bunx") {
        return Some(vec!["bunx".into(), "ccusage".into()]);
    }
    if binary_on_path("npx") {
        return Some(vec!["npx".into(), "--yes".into(), "ccusage".into()]);
    }
    None
}

pub fn ccusage_runner_description() -> Option<String> {
    resolve_ccusage_runner().map(|parts| parts.join(" "))
}

fn binary_on_path(name: &str) -> bool {
    find_in_path(name).is_some()
}

/// Locate an executable named `name` on `PATH`, returning its full path.
///
/// On Windows the name is tried both as-is and with each extension from
/// `PATHEXT` (falling back to a sensible default set), so shims like
/// `npx.cmd`, `bunx.cmd` and `ccusage.exe` are found even when the caller
/// passes the bare stem.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let is_file = |p: &Path| std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false);

    for dir in std::env::split_paths(&path) {
        let direct = dir.join(name);
        if is_file(&direct) {
            return Some(direct);
        }

        #[cfg(windows)]
        {
            // Only append extensions when the name has none of its own.
            if Path::new(name).extension().is_none() {
                let pathext =
                    std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string());
                for cand in pathext_candidates(name, &pathext) {
                    let candidate = dir.join(cand);
                    if is_file(&candidate) {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    None
}

/// Expand an extensionless executable `name` into `name.<ext>` candidates from a
/// `PATHEXT` string (e.g. "npx" -> ["npx.EXE", "npx.CMD", ...]). Empty segments
/// are skipped. Extracted so the probing order can be unit-tested without
/// touching the process environment or filesystem.
#[cfg(windows)]
fn pathext_candidates(name: &str, pathext: &str) -> Vec<String> {
    pathext
        .split(';')
        .filter(|e| !e.is_empty())
        .map(|ext| format!("{name}.{}", ext.trim_start_matches('.')))
        .collect()
}

/// Build a `Command` for the resolved ccusage runner.
///
/// On Windows the runner is very often a batch shim (`npx.cmd`, `bunx.cmd`),
/// which `CreateProcess` cannot execute directly — Rust's `Command` only
/// appends `.exe`. Routing through `cmd /C` lets the shell resolve `.cmd`/`.bat`
/// (and plain `.exe`) via `PATHEXT`. On Unix we spawn the program directly.
fn ccusage_command(runner: &[String]) -> Command {
    #[cfg(windows)]
    {
        let mut command = Command::new("cmd");
        command.arg("/C");
        for part in runner {
            command.arg(part);
        }
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new(&runner[0]);
        for part in &runner[1..] {
            command.arg(part);
        }
        command
    }
}

fn run_ccusage_blocks(args: &[&str], deadline: Instant) -> Result<CcusageBlocksResponse> {
    let runner = resolve_ccusage_runner().ok_or_else(|| anyhow!("no ccusage runner on PATH"))?;
    let mut command = ccusage_command(&runner);
    command.args(args).arg("--json");
    let output =
        run_with_timeout(command, budget_left(deadline)?).context("ccusage blocks failed")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("ccusage blocks exit non-zero: {}", stderr.trim()));
    }
    serde_json::from_slice(&output.stdout).context("ccusage blocks output was not valid JSON")
}

struct ActiveBlockInfo {
    burn: Option<BurnRate>,
    session_usd: f64,
}

fn fetch_active_blocks(deadline: Instant) -> HashMap<String, ActiveBlockInfo> {
    let resp = match run_ccusage_blocks(&["blocks", "--active", "--offline"], deadline)
        .or_else(|_| run_ccusage_blocks(&["blocks", "--active"], deadline))
    {
        Ok(r) => r,
        Err(_) => return HashMap::new(),
    };
    let mut by_provider: HashMap<String, ActiveBlockInfo> = HashMap::new();
    for block in resp.blocks.into_iter().filter(|b| b.is_active) {
        let provider = block
            .models
            .iter()
            .find_map(|m| model_to_provider(m))
            .unwrap_or("claude")
            .to_string();
        let burn = match (block.burn_rate, block.projection) {
            (Some(rate), Some(proj)) => Some(BurnRate {
                cost_per_hour: rate.cost_per_hour,
                tokens_per_minute: rate.tokens_per_minute as u64,
                remaining_minutes: proj.remaining_minutes,
                projected_cost: proj.total_cost,
            }),
            _ => None,
        };
        by_provider.insert(
            provider,
            ActiveBlockInfo {
                burn,
                session_usd: block.cost_usd,
            },
        );
    }
    by_provider
}

/// Run `ccusage daily` once for everything, with the two flags that make it
/// cheap and precise.
///
/// `--offline` skips the LiteLLM pricing fetch each invocation otherwise pays
/// for - measurably ~700ms, for figures identical to the cent. `--by-agent`
/// splits the merged breakdown back out per CLI, which is what lets a Kimi or
/// Grok row exist at all. A ccusage too old for either flag rejects it and
/// exits non-zero, so the bare form is retried before the caller gives up and
/// shows no cost at all.
fn run_ccusage_daily(since: &str, deadline: Instant) -> Result<CcusageDailyResponse> {
    run_ccusage(
        &["daily", "--since", since, "--offline", "--by-agent"],
        deadline,
    )
    .or_else(|_| run_ccusage(&["daily", "--since", since], deadline))
}

/// What is left of the cost fetch's budget.
///
/// Every ccusage call is a retry away from a second one, and both the daily and
/// the blocks call can retry, so handing each attempt the full timeout lets a
/// slow or too-old ccusage run four of them back to back. That outlives
/// `refresh_budget_ms`, which sizes the refresh sentinel from a single timeout -
/// and once the sentinel expires a second refresh starts on top of the first.
/// One deadline for the whole fetch keeps it inside the budget.
///
/// Past the deadline this fails instead of clamping to a token slice: another
/// ccusage process started there would overrun the budget it was meant to
/// respect, and a sliver of a timeout only buys a subprocess spawn it cannot
/// finish inside.
fn budget_left(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left < MIN_CCUSAGE_SLICE {
        return Err(anyhow!("cost fetch budget exhausted"));
    }
    Ok(left)
}

/// Less than this left on the clock is not worth spawning for.
const MIN_CCUSAGE_SLICE: Duration = Duration::from_millis(500);

fn run_ccusage(args: &[&str], deadline: Instant) -> Result<CcusageDailyResponse> {
    let runner = resolve_ccusage_runner().ok_or_else(|| anyhow!("no ccusage runner on PATH"))?;
    let mut command = ccusage_command(&runner);
    command.args(args).arg("--json");
    let output = run_with_timeout(command, budget_left(deadline)?).context("ccusage failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("ccusage exited non-zero: {}", stderr.trim()));
    }

    serde_json::from_slice(&output.stdout).context("ccusage output was not valid JSON")
}

/// Fetch ccusage cost info. Returns a map from provider key to CostInfo.
/// Returns empty map on any failure (ccusage missing, no logs, parse error).
pub fn fetch_ccusage_costs(timeout: Duration) -> HashMap<String, CostInfo> {
    // One deadline for every call this makes, retries included.
    let deadline = Instant::now() + timeout;
    let today_date = Local::now().date_naive();
    let month_start_date = fmt::month_start(today_date);
    // The rolling 7-day window reaches back past the 1st for the first six
    // days of a month, so the query has to start at whichever of the two is
    // earlier. Asking only from the 1st would zero-fill the days before it -
    // which reads as "spent nothing" rather than "not asked for", understating
    // `weekly_usd` and inflating the today-vs-average baseline.
    let week_start_date = today_date
        .checked_sub_days(Days::new(WEEKLY_HISTORY_DAYS as u64 - 1))
        .unwrap_or(today_date);
    let since = month_start_date.min(week_start_date);

    // One call, sliced three ways below. Three calls each re-read every
    // transcript on disk to answer a narrower question than the one before it.
    let daily = match run_ccusage_daily(&since.format("%Y%m%d").to_string(), deadline) {
        Ok(r) => r,
        Err(_) => return HashMap::new(),
    };

    let today = today_date.format("%Y-%m-%d").to_string();
    let month_start = month_start_date.format("%Y-%m-%d").to_string();
    let mut today_agg = aggregate_ccusage(&daily, &today, &today);
    let mut monthly_agg = aggregate_ccusage(&daily, &month_start, &today);
    let mut weekly_history = last_n_days_by_provider(&daily, today_date, WEEKLY_HISTORY_DAYS);
    let mut active_blocks = fetch_active_blocks(deadline);

    let mut result = HashMap::new();
    // `weekly_history` reaches back past the 1st, so early in a month a
    // provider can have spend in the rolling week and none in the month. Left
    // out of this set it loses its row entirely, chart and all.
    let providers: std::collections::HashSet<String> = today_agg
        .keys()
        .chain(monthly_agg.keys())
        .chain(active_blocks.keys())
        .chain(weekly_history.keys())
        .cloned()
        .collect();
    for provider in providers {
        let (today_usd, today_tokens, today_models) = today_agg
            .remove(&provider)
            .map(|a| a.into_model_costs())
            .unwrap_or((0.0, 0, Vec::new()));
        let (monthly_usd, monthly_tokens, monthly_models) = monthly_agg
            .remove(&provider)
            .map(|a| a.into_model_costs())
            .unwrap_or((0.0, 0, Vec::new()));
        let (burn_rate, session_usd) = active_blocks
            .remove(&provider)
            .map(|a| (a.burn, a.session_usd))
            .unwrap_or((None, 0.0));
        let history = weekly_history.remove(&provider).unwrap_or_default();
        let weekly_cost_history: Vec<f64> = history.iter().map(|d| d.usd).collect();
        let weekly_usd = weekly_cost_history.iter().sum();
        result.insert(
            provider,
            CostInfo {
                today_usd,
                today_tokens,
                monthly_usd,
                monthly_tokens,
                today_models,
                monthly_models,
                burn_rate,
                session_usd,
                weekly_usd,
                weekly_cost_history,
                weekly_history: history,
                by_device: Vec::new(),
                sync_note: None,
            },
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    // ------------------------------------------------------------------------
    // Windows executable discovery / command construction
    // ------------------------------------------------------------------------

    #[cfg(windows)]
    #[test]
    fn pathext_candidates_appends_each_extension() {
        assert_eq!(
            pathext_candidates("npx", ".EXE;.CMD;.BAT"),
            vec![
                "npx.EXE".to_string(),
                "npx.CMD".to_string(),
                "npx.BAT".to_string()
            ]
        );
        // Empty PATHEXT segments are skipped.
        assert_eq!(
            pathext_candidates("foo", ".EXE;;"),
            vec!["foo.EXE".to_string()]
        );
    }

    #[cfg(windows)]
    #[test]
    fn ccusage_command_routes_through_cmd_preserving_args() {
        let runner = vec![
            "npx".to_string(),
            "--yes".to_string(),
            "ccusage".to_string(),
        ];
        let command = ccusage_command(&runner);
        assert_eq!(command.get_program().to_string_lossy(), "cmd");
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["/C", "npx", "--yes", "ccusage"]);
    }

    fn ccusage_fixture() -> CcusageDailyResponse {
        serde_json::from_str(
            r#"{
              "daily": [
                {
                  "period": "2026-08-18",
                  "modelBreakdowns": [
                    { "modelName": "claude-opus-5", "cost": 1.5,
                      "inputTokens": 10, "outputTokens": 20,
                      "cacheCreationTokens": 30, "cacheReadTokens": 40 }
                  ]
                },
                {
                  "period": "2026-08-20",
                  "modelBreakdowns": [
                    { "modelName": "claude-opus-5", "cost": 2.5,
                      "inputTokens": 1, "outputTokens": 2,
                      "cacheCreationTokens": 3, "cacheReadTokens": 4 },
                    { "modelName": "gpt-5-codex", "cost": 9.0,
                      "inputTokens": 5, "outputTokens": 5,
                      "cacheCreationTokens": 0, "cacheReadTokens": 0 }
                  ]
                }
              ]
            }"#,
        )
        .expect("parse ccusage fixture")
    }

    #[test]
    fn daily_history_carries_dates_and_tokens() {
        let history = last_n_days_by_provider(&ccusage_fixture(), day(2026, 8, 20), 4);
        let claude = history.get("claude").expect("claude history");

        // ccusage omits days with no usage, so the series is built from the
        // calendar: the 17th predates the fixture and the 19th is idle, and
        // both still get a zeroed entry rather than vanishing.
        assert_eq!(
            claude.iter().map(|d| d.date.as_str()).collect::<Vec<_>>(),
            vec!["2026-08-17", "2026-08-18", "2026-08-19", "2026-08-20"]
        );
        assert_eq!(
            claude.iter().map(|d| d.tokens).collect::<Vec<_>>(),
            vec![0, 100, 0, 10]
        );
        assert_eq!(claude[2].usd, 0.0);

        // Every provider covers the same window: codex only spent on the 20th.
        let codex = history.get("codex").expect("codex history");
        assert_eq!(
            codex.iter().map(|d| d.date.as_str()).collect::<Vec<_>>(),
            claude.iter().map(|d| d.date.as_str()).collect::<Vec<_>>()
        );
        assert_eq!(
            codex.iter().map(|d| d.tokens).collect::<Vec<_>>(),
            vec![0, 0, 0, 10]
        );

        // Each day names the models that spent it, and only its own: the 20th
        // is the one day both providers ran.
        assert_eq!(
            claude[3]
                .by_model
                .iter()
                .map(|m| (m.model.as_str(), m.tokens))
                .collect::<Vec<_>>(),
            vec![("claude-opus-5", 10)]
        );
        assert!(claude[2].by_model.is_empty());
        assert_eq!(
            codex[3]
                .by_model
                .iter()
                .map(|m| (m.model.as_str(), m.tokens))
                .collect::<Vec<_>>(),
            vec![("gpt-5-codex", 10)]
        );
    }

    #[test]
    fn daily_history_ends_on_today_and_ignores_later_entries() {
        // A day past the window - a machine whose clock ran ahead, or a stale
        // cache read after midnight - must not push the window forward.
        let history = last_n_days_by_provider(&ccusage_fixture(), day(2026, 8, 19), 2);
        let claude = history.get("claude").expect("claude history");
        assert_eq!(
            claude.iter().map(|d| d.date.as_str()).collect::<Vec<_>>(),
            vec!["2026-08-18", "2026-08-19"]
        );
        assert_eq!(
            claude.iter().map(|d| d.tokens).collect::<Vec<_>>(),
            vec![100, 0]
        );
    }

    /// `--by-agent` output: the same day, split per CLI. A `glm-4.6` run and a
    /// Kimi one both land under the `claude` agent because both are driven
    /// through Claude Code.
    fn by_agent_fixture() -> CcusageDailyResponse {
        serde_json::from_str(
            r#"{
              "daily": [
                {
                  "period": "2026-08-20",
                  "agents": [
                    {
                      "agent": "claude",
                      "modelBreakdowns": [
                        { "modelName": "claude-opus-5", "cost": 2.0,
                          "inputTokens": 1, "outputTokens": 1,
                          "cacheCreationTokens": 0, "cacheReadTokens": 0 },
                        { "modelName": "glm-4.6", "cost": 0.5,
                          "inputTokens": 2, "outputTokens": 2,
                          "cacheCreationTokens": 0, "cacheReadTokens": 0 }
                      ]
                    },
                    {
                      "agent": "grok",
                      "modelBreakdowns": [
                        { "modelName": "some-unlabelled-model", "cost": 1.0,
                          "inputTokens": 3, "outputTokens": 3,
                          "cacheCreationTokens": 0, "cacheReadTokens": 0 }
                      ]
                    }
                  ],
                  "modelBreakdowns": [
                    { "modelName": "claude-opus-5", "cost": 2.0,
                      "inputTokens": 1, "outputTokens": 1,
                      "cacheCreationTokens": 0, "cacheReadTokens": 0 },
                    { "modelName": "glm-4.6", "cost": 0.5,
                      "inputTokens": 2, "outputTokens": 2,
                      "cacheCreationTokens": 0, "cacheReadTokens": 0 },
                    { "modelName": "some-unlabelled-model", "cost": 1.0,
                      "inputTokens": 3, "outputTokens": 3,
                      "cacheCreationTokens": 0, "cacheReadTokens": 0 }
                  ]
                }
              ]
            }"#,
        )
        .expect("parse by-agent fixture")
    }

    #[test]
    fn by_agent_splits_providers_sharing_one_agent() {
        let agg = aggregate_ccusage(&by_agent_fixture(), "2026-08-20", "2026-08-20");

        // The model wins over the agent: GLM through Claude Code is GLM spend,
        // and before --by-agent it was dropped on the floor entirely.
        assert_eq!(agg.get("claude").expect("claude").total_usd, 2.0);
        assert_eq!(agg.get("glm").expect("glm").total_usd, 0.5);
        // The agent is the fallback when the model name maps nowhere.
        assert_eq!(agg.get("grok").expect("grok").total_usd, 1.0);
    }

    #[test]
    fn by_agent_and_flat_breakdowns_are_not_double_counted() {
        // ccusage emits both: `modelBreakdowns` is the per-agent data merged.
        let agg = aggregate_ccusage(&by_agent_fixture(), "2026-08-20", "2026-08-20");
        let total: f64 = agg.values().map(|a| a.total_usd).sum();
        assert_eq!(total, 3.5);

        let history = last_n_days_by_provider(&by_agent_fixture(), day(2026, 8, 20), 1);
        assert_eq!(history.get("claude").expect("claude")[0].usd, 2.0);
        assert_eq!(history.get("glm").expect("glm")[0].tokens, 4);
    }

    #[test]
    fn aggregate_window_slices_one_response() {
        // One ccusage call answers today, the month and the rolling week.
        let f = ccusage_fixture();
        let today = aggregate_ccusage(&f, "2026-08-20", "2026-08-20");
        assert_eq!(today.get("claude").expect("claude").total_usd, 2.5);
        assert_eq!(today.get("codex").expect("codex").total_usd, 9.0);

        let month = aggregate_ccusage(&f, "2026-08-01", "2026-08-20");
        assert_eq!(month.get("claude").expect("claude").total_usd, 4.0);

        // A day past `until` (a clock that ran ahead) stays out.
        let stale = aggregate_ccusage(&f, "2026-08-01", "2026-08-19");
        assert_eq!(stale.get("claude").expect("claude").total_usd, 1.5);
        assert!(!stale.contains_key("codex"));
    }

    #[test]
    fn a_provider_with_only_last_months_spend_keeps_its_week() {
        // 2026-08-03: the rolling week reaches back to 2026-07-28, so spend on
        // the 31st is in the week and not in the month. The provider still has
        // a row, and it still has its chart.
        let response: CcusageDailyResponse = serde_json::from_str(
            r#"{"daily":[{"period":"2026-07-31","modelBreakdowns":[
                 {"modelName":"gpt-5-codex","cost":3.0,"inputTokens":5,
                  "outputTokens":5,"cacheCreationTokens":0,"cacheReadTokens":0}]}]}"#,
        )
        .expect("fixture parses");

        let today = day(2026, 8, 3);
        let history = last_n_days_by_provider(&response, today, WEEKLY_HISTORY_DAYS);
        let month = aggregate_ccusage(&response, "2026-08-01", "2026-08-03");

        assert!(month.is_empty(), "nothing was spent this month");
        let codex = history.get("codex").expect("codex has a week");
        assert_eq!(codex.iter().map(|d| d.usd).sum::<f64>(), 3.0);

        // The set the result is built from has to include it, or the row and
        // its history are dropped on the floor.
        let providers: std::collections::HashSet<&String> =
            month.keys().chain(history.keys()).collect();
        assert!(providers.contains(&"codex".to_string()));
    }

    #[test]
    fn recent_periods_spans_a_month_boundary() {
        assert_eq!(
            recent_periods(day(2026, 3, 2), 3),
            vec![
                "2026-02-28".to_string(),
                "2026-03-01".to_string(),
                "2026-03-02".to_string(),
            ]
        );
    }

    #[test]
    fn model_costs_carry_the_token_split() {
        let mut agg = aggregate_ccusage(&ccusage_fixture(), "2026-08-01", "2026-08-31");
        let (usd, tokens, models) = agg.remove("claude").expect("claude").into_model_costs();

        assert_eq!(usd, 4.0);
        assert_eq!(tokens, 110);
        assert_eq!(models.len(), 1);

        let opus = &models[0];
        assert_eq!(opus.model, "claude-opus-5");
        assert_eq!(opus.tokens, 110);
        assert_eq!(opus.input_tokens, 11);
        assert_eq!(opus.output_tokens, 22);
        assert_eq!(opus.cache_creation_tokens, 33);
        assert_eq!(opus.cache_read_tokens, 44);
        assert_eq!(
            opus.input_tokens
                + opus.output_tokens
                + opus.cache_creation_tokens
                + opus.cache_read_tokens,
            opus.tokens
        );
    }
}
