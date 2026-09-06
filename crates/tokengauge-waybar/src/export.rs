//! The history store as rows, for a spreadsheet or a script.
//!
//! The panel and the history pane both answer questions TokenGauge picked in
//! advance. This answers the ones it did not: every bucket the store holds, one
//! row per day, provider, model and device, with the token split it was rated
//! from beside the money that rating produced.
//!
//! Rated at each month's own prices like every other history figure, so an
//! export and the chart agree, and so re-running it next year does not quietly
//! restate what last June cost.

use std::io::{self, Write};

use anyhow::{Context, Result};
use chrono::NaiveDate;
use tokengauge_core::TokenGaugeConfig;
use tokengauge_core::cost::pricing;

use crate::ExportFormat;

/// The column order, and the header row.
const COLUMNS: &[&str] = &[
    "date",
    "provider",
    "model",
    "device_id",
    "device",
    "input",
    "output",
    "cache_write_5m",
    "cache_write_1h",
    "cache_read",
    "total_tokens",
    "usd",
];

pub(crate) fn run(
    config: &TokenGaugeConfig,
    format: ExportFormat,
    since: Option<&str>,
) -> Result<()> {
    let since = since
        .map(|text| {
            text.parse::<NaiveDate>()
                .with_context(|| format!("--since wants a date as YYYY-MM-DD, not `{text}`"))
        })
        .transpose()?;

    let (store, error) = tokengauge_core::sync::store::load(&config.cache_file);
    // A store that would not parse exports an empty file, which downstream
    // reads as "nothing was spent". Say so on stderr and fail, rather than let
    // a spreadsheet be built on it.
    if let Some(error) = error {
        anyhow::bail!("{error}");
    }

    let prices = pricing::load(
        &config.cache_file,
        std::time::Duration::from_secs(config.ccusage_timeout_secs),
        false,
    );
    let now = chrono::Local::now();
    let rows = store.export_rows(since, *now.offset(), &prices, pricing::archive());

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    match format {
        ExportFormat::Csv => write_csv(&mut out, &rows),
        ExportFormat::Json => write_json(&mut out, &rows),
    }
}

fn write_csv(out: &mut impl Write, rows: &[tokengauge_core::sync::ExportRow]) -> Result<()> {
    writeln!(out, "{}", COLUMNS.join(","))?;
    for row in rows {
        writeln!(
            out,
            "{},{},{},{},{},{},{},{},{},{},{},{}",
            row.date,
            csv_field(&row.provider),
            csv_field(&row.model),
            csv_field(&row.device_id),
            csv_field(&row.device),
            row.tokens.input,
            row.tokens.output,
            row.tokens.cache_write_5m,
            row.tokens.cache_write_1h,
            row.tokens.cache_read,
            row.total_tokens,
            // An unpriced model leaves the cell empty rather than writing
            // `0.000000`: summing the column then reads as a gap, which is what
            // it is, instead of as a day that cost nothing.
            row.usd.map(|usd| format!("{usd:.6}")).unwrap_or_default(),
        )?;
    }
    out.flush()?;
    Ok(())
}

/// A device label is user-set and a model id is upstream's, so neither is
/// guaranteed to be free of commas or quotes.
///
/// A leading `=`, `+`, `-` or `@` is also a formula to Excel and LibreOffice,
/// and quoting does not stop them: a spreadsheet is the whole point of the CSV,
/// so a field that starts with one is prefixed with an apostrophe, which those
/// two read as "this is text" and drop again on display. `--export json` is
/// untouched for anything parsing the data rather than opening it.
fn csv_field(value: &str) -> String {
    let formula = value.starts_with(['=', '+', '-', '@', '\t', '\r']);
    // A bare CR ends a record for a strict RFC 4180 reader just as LF does, so
    // it has to be quoted wherever it sits, not only when it leads.
    let quoted = value.contains([',', '"', '\n', '\r']) || formula;
    if !quoted {
        return value.to_string();
    }
    let escaped = value.replace('"', "\"\"");
    if formula {
        format!("\"'{escaped}\"")
    } else {
        format!("\"{escaped}\"")
    }
}

fn write_json(out: &mut impl Write, rows: &[tokengauge_core::sync::ExportRow]) -> Result<()> {
    let values: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "date": row.date.to_string(),
                "provider": row.provider,
                "model": row.model,
                "device_id": row.device_id,
                "device": row.device,
                "input": row.tokens.input,
                "output": row.tokens.output,
                "cache_write_5m": row.tokens.cache_write_5m,
                "cache_write_1h": row.tokens.cache_write_1h,
                "cache_read": row.tokens.cache_read,
                "total_tokens": row.total_tokens,
                "usd": row.usd,
            })
        })
        .collect();
    serde_json::to_writer_pretty(&mut *out, &values)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(model: &str, device: &str) -> tokengauge_core::sync::ExportRow {
        tokengauge_core::sync::ExportRow {
            date: "2026-08-25".parse().expect("date"),
            provider: "claude".into(),
            model: model.into(),
            device_id: "abc123".into(),
            device: device.into(),
            tokens: tokengauge_core::cost::TokenCounts {
                input: 1,
                output: 2,
                cache_write_5m: 3,
                cache_write_1h: 4,
                cache_read: 5,
            },
            total_tokens: 15,
            usd: Some(1.5),
        }
    }

    #[test]
    fn the_header_matches_the_columns_written() {
        let mut out = Vec::new();
        write_csv(&mut out, &[row("m", "desk")]).expect("csv");
        let text = String::from_utf8(out).expect("utf8");
        let mut lines = text.lines();
        let header: Vec<&str> = lines.next().expect("header").split(',').collect();
        let body: Vec<&str> = lines.next().expect("a row").split(',').collect();
        assert_eq!(header, COLUMNS);
        assert_eq!(
            header.len(),
            body.len(),
            "a row has to carry one field per column"
        );
    }

    #[test]
    fn an_unpriced_model_leaves_the_money_cell_empty() {
        // Never `0.000000`: a row claiming a million tokens cost nothing is
        // worse than a row admitting it does not know.
        let mut unpriced = row("mystery-model", "desk");
        unpriced.usd = None;
        let mut out = Vec::new();
        write_csv(&mut out, &[unpriced]).expect("csv");
        let text = String::from_utf8(out).expect("utf8");
        let body = text.lines().nth(1).expect("a row");
        assert!(body.ends_with(",15,"), "the usd cell is empty: {body}");
        assert!(!body.contains("0.000000"), "{body}");
    }

    #[test]
    fn a_field_that_would_be_a_formula_is_neutralised() {
        // A device label is typed by the user and reaches a spreadsheet.
        let mut out = Vec::new();
        write_csv(&mut out, &[row("=cmd|'/c calc'!A1", "@SUM(1)")]).expect("csv");
        let text = String::from_utf8(out).expect("utf8");
        let body = text.lines().nth(1).expect("a row");
        assert!(body.contains("\"'=cmd"), "{body}");
        assert!(body.contains("\"'@SUM(1)\""), "{body}");
        assert!(
            !body.contains(",=cmd") && !body.contains(",@SUM"),
            "a bare formula reached the file: {body}"
        );
    }

    #[test]
    fn a_bare_carriage_return_does_not_end_the_record() {
        // A strict RFC 4180 reader ends a record on CR as readily as on LF, so
        // it has to be quoted wherever it sits and not only when it leads.
        assert_eq!(csv_field("one\rtwo"), "\"one\rtwo\"");
        assert_eq!(csv_field("one\ntwo"), "\"one\ntwo\"");
        assert_eq!(csv_field("ordinary"), "ordinary");
    }

    /// The JSON form is for something parsing the data rather than opening it,
    /// so it carries the same columns and says "no price" as `null` - never as
    /// the zero the CSV leaves as an empty cell.
    #[test]
    fn the_json_form_carries_the_same_columns_and_a_null_for_an_unpriced_row() {
        let mut unpriced = row("mystery-model", "desk");
        unpriced.usd = None;

        let mut out = Vec::new();
        write_json(&mut out, &[row("m", "desk"), unpriced]).expect("json");
        let parsed: Vec<serde_json::Value> =
            serde_json::from_slice(&out).expect("the export has to be valid JSON");

        assert_eq!(parsed.len(), 2);
        let keys: Vec<&str> = parsed[0]
            .as_object()
            .expect("an object per row")
            .keys()
            .map(String::as_str)
            .collect();
        let mut expected = COLUMNS.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected, "the two forms describe the same row");
        assert_eq!(parsed[0]["usd"], serde_json::json!(1.5));
        assert_eq!(parsed[1]["usd"], serde_json::Value::Null);
        assert_eq!(parsed[1]["total_tokens"], serde_json::json!(15));
    }

    fn config(tag: &str) -> TokenGaugeConfig {
        let dir = std::env::temp_dir().join(format!(
            "tg-export-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        TokenGaugeConfig {
            cache_file: dir.join("usage.json"),
            ..Default::default()
        }
    }

    /// Both refusals happen before a single row is written, because a partial
    /// export on stdout is one a script cannot tell from a complete one.
    #[test]
    fn an_export_that_cannot_be_trusted_is_refused_rather_than_written() {
        let cfg = config("since");
        let since = run(&cfg, ExportFormat::Csv, Some("last tuesday")).expect_err("not a date");
        assert!(since.to_string().contains("YYYY-MM-DD"), "{since}");

        // A store that will not parse would otherwise export an empty file,
        // which downstream reads as "nothing was spent".
        let cfg = config("corrupt");
        let store = tokengauge_core::sync::store::store_path(&cfg.cache_file);
        std::fs::create_dir_all(store.parent().expect("state dir")).expect("state dir");
        std::fs::write(&store, "{ this is not the store }").expect("write");
        assert!(
            run(&cfg, ExportFormat::Json, None).is_err(),
            "an unreadable store is not an empty one"
        );

        let _ = std::fs::remove_dir_all(cfg.cache_file.parent().expect("temp dir"));
    }

    #[test]
    fn a_label_with_a_comma_does_not_become_two_columns() {
        // Device labels are typed by the user and model ids come from upstream.
        let mut out = Vec::new();
        write_csv(&mut out, &[row("a,b", "my \"box\", at home")]).expect("csv");
        let text = String::from_utf8(out).expect("utf8");
        let body = text.lines().nth(1).expect("a row");
        assert!(body.contains("\"a,b\""), "{body}");
        assert!(body.contains("\"my \"\"box\"\", at home\""), "{body}");
    }
}
