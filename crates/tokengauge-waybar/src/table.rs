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
    let (line, pace) = lines
        .iter()
        .filter_map(|l| l.pace.map(|p| (l, p)))
        .filter(|(l, p)| p.projected_percent > f64::from(l.used) + 0.5)
        .max_by(|a, b| a.1.projected_percent.total_cmp(&b.1.projected_percent))?;
    let which = format!("{} {}", line.who, line.window);
    Some(
        match pace.eta_seconds.filter(|_| !pace.will_last_to_reset) {
            Some(eta) => format!("{which} runs out in {}", format_duration(eta / 60, 2)),
            None => format!(
                "{which} lands near {}%",
                pace.projected_percent.round().min(100.0)
            ),
        },
    )
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
    if lines.is_empty() && errors.is_empty() {
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
            let line = format!(
                "{:who$}  {:window$}  {} {:>4}%  {}",
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
    let month = month_line(rows);
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
