//! Plain `tokengauge`: every window each provider reports, as a table for a
//! terminal, then where the fastest burner lands and what the month cost.

use std::collections::BTreeSet;
use std::io::{self, Write};

use tokengauge_core::fmt::format_duration;
use tokengauge_core::panel::Tone;
use tokengauge_core::term::Ink;
use tokengauge_core::{ProviderFetchError, ProviderRow, UsagePace, window_labels};

const BAR_CELLS: usize = 20;

/// One window's line.
struct Line {
    who: String,
    window: String,
    used: u8,
    reset: String,
    pace: Option<UsagePace>,
    stale: bool,
}

/// A window's length the way a person says it.
fn span(minutes: u32) -> String {
    match minutes {
        300 => "5h".into(),
        1440 => "daily".into(),
        10080 => "weekly".into(),
        m if (40_320..=44_640).contains(&m) => "monthly".into(),
        m => format_duration(i64::from(m), 1),
    }
}

/// "in 16d 3h 11m" to "in 16d 3h": two units are enough to plan around. A
/// window with no reset gets a blank cell rather than a placeholder.
fn short_reset(reset: &str) -> String {
    let mut words = reset.split_whitespace();
    match words.next() {
        Some("in") => {
            let units: Vec<&str> = words.take(2).collect();
            format!("in {}", units.join(" "))
        }
        Some(word) if word.chars().any(char::is_alphanumeric) => reset.to_owned(),
        _ => String::new(),
    }
}

fn lines_of(who: &str, row: &ProviderRow, out: &mut Vec<Line>) {
    let (session, weekly, tertiary) = window_labels(&row.provider);
    let named =
        |minutes: Option<u32>, label: &str| minutes.map_or_else(|| label.to_lowercase(), span);
    let fixed = [
        (
            named(row.session_window_minutes, session),
            row.session_used,
            &row.session_reset,
            row.session_pace,
        ),
        (
            named(row.weekly_window_minutes, weekly),
            row.weekly_used,
            &row.weekly_reset,
            row.weekly_pace,
        ),
        (
            tertiary.to_lowercase(),
            row.tertiary_used,
            &row.tertiary_reset,
            None,
        ),
    ];
    let extra = row
        .extra_windows
        .iter()
        .filter(|e| !e.placeholder)
        .map(|e| (e.title.to_lowercase(), e.used, &e.reset, e.pace));
    for (window, used, reset, pace) in fixed.into_iter().chain(extra) {
        if let Some(used) = used {
            out.push(Line {
                who: who.to_owned(),
                window,
                used,
                reset: short_reset(reset),
                pace,
                stale: row.stale,
            });
        }
    }
}

/// Each provider's windows, one line per stored login where it has several.
fn lines(rows: &[ProviderRow]) -> Vec<Line> {
    let mut out = Vec::new();
    for row in rows {
        let provider = row.provider.to_lowercase();
        if row.credentials.is_empty() {
            lines_of(&provider, row, &mut out);
            continue;
        }
        for login in &row.credentials {
            let who = match login.credential.as_ref().and_then(|c| c.name.as_deref()) {
                Some(name) => format!("{provider}/{name}"),
                None => provider.clone(),
            };
            lines_of(&who, login, &mut out);
        }
    }
    out
}

fn bar(used: u8, ink: &Ink) -> String {
    let filled = (usize::from(used.min(100)) * BAR_CELLS).div_ceil(100);
    format!(
        "{}{}{}{}{}",
        ink.tone(Tone::for_percent(used)),
        "█".repeat(filled),
        ink.dim,
        "░".repeat(BAR_CELLS - filled),
        ink.reset
    )
}

/// The window heading furthest past where it stands: run out before its reset,
/// or land highest at it.
fn pace_line(lines: &[Line]) -> Option<String> {
    let which = |l: &Line| format!("{} {}", l.who, l.window);
    let runs_out = |l: &Line| {
        l.pace
            .filter(|p| !p.will_last_to_reset && l.used < 100)
            .and_then(|p| p.eta_seconds)
    };
    if let Some((line, eta)) = lines
        .iter()
        .filter_map(|l| runs_out(l).map(|eta| (l, eta)))
        .min_by_key(|(_, eta)| *eta)
    {
        return Some(format!(
            "{} runs out in {}",
            which(line),
            format_duration(eta / 60, 2)
        ));
    }
    if let Some(spent) = lines.iter().find(|l| l.used >= 100) {
        return Some(if spent.reset.is_empty() {
            format!("{} is spent", which(spent))
        } else {
            format!("{} is spent until it resets {}", which(spent), spent.reset)
        });
    }
    let (line, pace) = lines
        .iter()
        .filter_map(|l| l.pace.map(|p| (l, p)))
        .filter(|(l, p)| p.projected_percent > f64::from(l.used) + 0.5)
        .max_by(|a, b| a.1.projected_percent.total_cmp(&b.1.projected_percent))?;
    Some(format!(
        "{} lands near {}%",
        which(line),
        pace.projected_percent.round().min(100.0)
    ))
}

/// The month's cost, counted once per provider, and how many machines it
/// covers when fleet sync says.
fn month_line(rows: &[ProviderRow]) -> Option<(f64, usize)> {
    let mut seen = BTreeSet::new();
    let mut machines = BTreeSet::new();
    let mut total = 0.0;
    for row in rows {
        let Some(cost) = row.cost.as_ref() else {
            continue;
        };
        if !seen.insert(row.provider.as_str()) {
            continue;
        }
        total += cost.monthly_usd;
        machines.extend(cost.by_device.iter().map(|d| d.device_id.as_str()));
    }
    (!seen.is_empty()).then_some((total, machines.len()))
}

pub fn render(
    rows: &[ProviderRow],
    errors: &[ProviderFetchError],
    ink: &Ink,
    out: &mut impl Write,
) -> io::Result<()> {
    let lines = lines(rows);
    let Ink {
        dim, accent, reset, ..
    } = *ink;
    let month = month_line(rows);
    if lines.is_empty() && errors.is_empty() && month.is_none() {
        return writeln!(
            out,
            "no usage yet: enable a provider in the config, or run `tokengauge --doctor`"
        );
    }
    if !lines.is_empty() {
        let who = lines
            .iter()
            .map(|l| l.who.chars().count())
            .max()
            .unwrap_or(0)
            .max("provider".len());
        let window = lines
            .iter()
            .map(|l| l.window.chars().count())
            .max()
            .unwrap_or(0)
            .max("window".len());
        let used = BAR_CELLS + 6;
        writeln!(
            out,
            "{dim}{:who$}  {:window$}  {:used$}  resets{reset}",
            "provider", "window", "used"
        )?;
        for l in &lines {
            let cached = if l.stale {
                format!("  {dim}(last figures, the fetch failed){reset}")
            } else {
                String::new()
            };
            let line = format!(
                "{:who$}  {:window$}  {} {:>4}%  {}{cached}",
                l.who,
                l.window,
                bar(l.used, ink),
                l.used,
                l.reset
            );
            writeln!(out, "{}", line.trim_end())?;
        }
    }
    let pace = pace_line(&lines);
    if pace.is_some() || month.is_some() {
        writeln!(out)?;
    }
    if let Some(pace) = pace {
        writeln!(out, "{dim}at reset{reset}    {pace}")?;
    }
    if let Some((usd, machines)) = month {
        let across = if machines > 1 {
            format!(" across {machines} machines")
        } else {
            String::new()
        };
        writeln!(
            out,
            "{dim}this month{reset}  {accent}${usd:.2}{reset}{across}"
        )?;
    }
    for e in errors {
        writeln!(out, "{}{}: {}{reset}", ink.bad, e.provider, e.message)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::tests::sample_row;
    use tokengauge_core::sync::DeviceCost;
    use tokengauge_core::{CostInfo, CredentialInfo, PaceStage};

    fn printed(rows: &[ProviderRow], errors: &[ProviderFetchError]) -> String {
        let mut out = Vec::new();
        render(rows, errors, &Ink::new(false), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn pace(projected: f64, eta: Option<i64>) -> UsagePace {
        UsagePace {
            stage: PaceStage::Ahead,
            delta_percent: 10.0,
            projected_percent: projected,
            eta_seconds: eta,
            will_last_to_reset: eta.is_none(),
            early: false,
        }
    }

    fn cost(usd: f64, devices: &[&str]) -> CostInfo {
        CostInfo {
            monthly_usd: usd,
            by_device: devices
                .iter()
                .map(|id| DeviceCost {
                    device_id: (*id).into(),
                    label: (*id).into(),
                    tokens: 0,
                    usd: 0.0,
                    updated_at_ms: 0,
                    partial: false,
                    is_local: false,
                })
                .collect(),
            ..CostInfo::default()
        }
    }

    #[test]
    fn each_window_is_a_line_with_a_bar_and_its_reset() {
        let mut claude = sample_row("Claude");
        claude.session_pace = Some(pace(81.0, None));
        claude.cost = Some(cost(30.0, &["a", "b"]));
        let mut codex = sample_row("Codex");
        codex.weekly_used = None;
        codex.session_used = Some(14);
        codex.session_reset = "in 4h 10m 12s".into();
        codex.cost = Some(cost(11.2, &["a", "c"]));

        let text = printed(&[claude, codex], &[]);
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(lines[0], format!("provider  window  {:26}  resets", "used"));
        assert_eq!(
            lines[1],
            format!(
                "claude    5h      {}{}   67%  in 2h 34m",
                "█".repeat(14),
                "░".repeat(6)
            )
        );
        assert!(lines[2].starts_with("claude    weekly  ████░"), "{text}");
        assert!(lines[3].ends_with("14%  in 4h 10m"), "{text}");
        assert_eq!(lines[5], "at reset    claude 5h lands near 81%");
        assert_eq!(lines[6], "this month  $41.20 across 3 machines");
        assert!(!text.contains('\x1b'), "{text:?}");
    }

    #[test]
    fn a_login_each_and_the_one_running_out_first() {
        let mut work = sample_row("Claude");
        work.credential = Some(CredentialInfo {
            name: Some("work".into()),
            ..CredentialInfo::default()
        });
        work.weekly_pace = Some(pace(140.0, Some(5400)));
        let mut perso = sample_row("Claude");
        perso.credential = Some(CredentialInfo {
            name: Some("perso".into()),
            ..CredentialInfo::default()
        });
        let mut grouped = work.clone();
        grouped.credentials = vec![work, perso];

        let text = printed(&[grouped], &[]);

        assert!(text.contains("claude/work   5h"), "{text}");
        assert!(text.contains("claude/perso  weekly"), "{text}");
        assert!(
            text.contains("at reset    claude/work weekly runs out in 1h 30m"),
            "{text}"
        );
        assert!(
            !text.contains("this month"),
            "no cost, no month line: {text}"
        );
    }

    #[test]
    fn nothing_to_show_says_where_to_look() {
        assert!(printed(&[], &[]).starts_with("no usage yet"));
        let failed = ProviderFetchError::new("codex".into(), "token expired");
        assert_eq!(printed(&[], &[failed]), "codex: token expired\n");
    }

    #[test]
    fn spans_and_resets_read_short() {
        assert_eq!(span(300), "5h");
        assert_eq!(span(10080), "weekly");
        assert_eq!(span(43200), "monthly");
        assert_eq!(span(120), "2h");
        assert_eq!(short_reset("in 16d 3h 11m"), "in 16d 3h");
        assert_eq!(short_reset("\u{2014}"), "");
        assert_eq!(short_reset("resets at 18:00"), "resets at 18:00");
    }
}
