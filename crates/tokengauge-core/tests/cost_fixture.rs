//! The cost readers, checked against ccusage's verdict without needing ccusage.
//!
//! `tests/fixtures/cost-home` is a fake home directory of real transcripts
//! reduced to the fields that are billed - no prompts, no output, no paths.
//! `cost-golden.json` is what ccusage read from that same tree, captured once by
//! `scripts/make-cost-fixture.py`, so this runs anywhere Rust does: no Node, no
//! network, no developer's home directory.
//!
//! Token counts are the invariant. They come from the files, so the two readings
//! must agree exactly. Money is deliberately not compared: it depends on
//! whichever price table each side happened to fetch. Days are not compared
//! either, because bucketing them is the reader's job and depends on its
//! timezone.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use chrono::NaiveDate;
use tokengauge_core::cost::{Roots, read_events_from};

fn fixture(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cost-home")
        .join(relative)
}

fn golden() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cost-golden.json");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("golden is valid JSON")
}

/// Everything the fixture holds, keyed by provider then model.
fn read_fixture() -> HashMap<String, HashMap<String, u64>> {
    let events = read_events_from(
        &Roots {
            claude: vec![fixture(".claude/projects")],
            codex: vec![fixture(".codex/sessions")],
            ..Roots::default()
        },
        NaiveDate::from_ymd_opt(2025, 1, 1).expect("valid date"),
    );
    assert!(
        !events.is_empty(),
        "the fixture tree produced no events at all"
    );

    let mut totals: HashMap<String, HashMap<String, u64>> = HashMap::new();
    for event in &events {
        *totals
            .entry(event.provider.to_string())
            .or_default()
            .entry(event.model.clone())
            .or_default() += event.tokens.total();
    }
    totals
}

#[test]
fn readers_agree_with_ccusage_on_every_model() {
    let ours = read_fixture();
    let golden = golden();
    let providers = golden["providers"]
        .as_object()
        .expect("golden has providers");

    // Key sets first, both ways. Checking only the golden's keys would let the
    // readers invent a provider or a model and still pass.
    let want_providers: BTreeSet<&str> = providers.keys().map(String::as_str).collect();
    let got_providers: BTreeSet<&str> = ours.keys().map(String::as_str).collect();
    assert_eq!(
        got_providers, want_providers,
        "provider set differs from ccusage's"
    );

    for (provider, expected) in providers {
        let mine = ours
            .get(provider)
            .unwrap_or_else(|| panic!("read nothing for {provider}"));
        let expected_models = expected["models"].as_object().expect("models map");

        let want: BTreeSet<&str> = expected_models.keys().map(String::as_str).collect();
        let got: BTreeSet<&str> = mine.keys().map(String::as_str).collect();
        assert_eq!(got, want, "{provider}: model set differs from ccusage's");

        for (model, tokens) in expected_models {
            let want = tokens.as_u64().expect("token count");
            let got = mine.get(model).copied().unwrap_or(0);
            assert_eq!(
                got, want,
                "{provider}/{model}: read {got} tokens, ccusage read {want}"
            );
        }
    }
}

#[test]
fn readers_agree_with_ccusage_on_the_totals() {
    let ours = read_fixture();
    let golden = golden();
    let providers = golden["providers"]
        .as_object()
        .expect("golden has providers");

    for (provider, expected) in providers {
        let want = expected["total_tokens"].as_u64().expect("total");
        let got: u64 = ours.get(provider).map(|m| m.values().sum()).unwrap_or(0);
        assert_eq!(
            got, want,
            "{provider}: read {got} tokens in total, ccusage read {want}"
        );
    }
}

/// The fixture carries `<synthetic>` records, which Claude Code writes for its
/// own local messages. They are not billed calls, and adding them to the tree
/// left the golden total unchanged - ccusage ignores them too - so the totals
/// above only agree if the reader drops them as well.
#[test]
fn synthetic_models_are_not_billed() {
    let ours = read_fixture();
    for models in ours.values() {
        for model in models.keys() {
            assert!(
                !model.starts_with('<'),
                "{model} is not a real model and should not be billed"
            );
        }
    }
}

/// Everything the fixture tree itself says about the traps, recounted here
/// rather than read off the golden's `traps` field. The generator wrote that
/// field, so trusting it means a generator whose own counting broke - or a
/// hand-edited golden - leaves this suite green over a fixture that no longer
/// covers anything.
struct TrapCounts {
    growing_streamed_groups: u64,
    synthetic_records: u64,
}

fn count_traps() -> TrapCounts {
    // Output tokens per (message.id, requestId) group, in file order. A group
    // that grows is the whole point of the fixture: first-wins and max-wins
    // agree everywhere else, and disagree by 46% of some models' output here.
    let mut groups: HashMap<(String, String), Vec<u64>> = HashMap::new();
    let mut synthetic_records = 0;

    let mut stack = vec![fixture(".claude/projects")];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                files.push(path);
            }
        }
    }
    assert!(!files.is_empty(), "the fixture tree holds no transcripts");

    for path in &files {
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for line in raw.lines() {
            let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let message = &record["message"];
            if message["model"].as_str() == Some("<synthetic>") {
                synthetic_records += 1;
            }
            let (Some(id), Some(request)) = (
                message["id"].as_str().filter(|s| !s.is_empty()),
                record["requestId"].as_str().filter(|s| !s.is_empty()),
            ) else {
                continue;
            };
            let Some(output) = message["usage"]["output_tokens"].as_u64() else {
                continue;
            };
            groups
                .entry((id.to_string(), request.to_string()))
                .or_default()
                .push(output);
        }
    }

    TrapCounts {
        growing_streamed_groups: groups
            .values()
            .filter(|outputs| outputs.len() > 1 && outputs.iter().max() > outputs.first())
            .count() as u64,
        synthetic_records,
    }
}

/// Guards the fixture itself: if a regeneration drops the streamed duplicates,
/// the suite would keep passing while no longer covering the bug that made the
/// readers disagree with ccusage in the first place.
#[test]
fn the_fixture_still_covers_the_traps() {
    let golden = golden();
    let claude = &golden["providers"]["claude"]["models"];
    let codex = &golden["providers"]["codex"]["models"];
    assert!(
        claude.as_object().expect("claude models").len() >= 3,
        "fixture should span several Claude models"
    );
    assert!(
        codex.as_object().expect("codex models").len() >= 2,
        "fixture should span several Codex models"
    );

    let counted = count_traps();

    // The growing-output duplicates are the trap. Without them the suite would
    // pass just as happily against a reader that keeps the first record of a
    // streamed group, which is the bug this whole fixture exists to catch.
    assert!(
        counted.growing_streamed_groups >= 50,
        "fixture no longer covers growing streamed duplicates: {} groups",
        counted.growing_streamed_groups
    );
    assert!(
        counted.synthetic_records > 0,
        "fixture no longer carries a <synthetic> record"
    );

    // And the golden agrees with the tree beside it. A mismatch means one of
    // the two was regenerated without the other, or edited by hand.
    assert_eq!(
        golden["traps"]["growing_streamed_groups"].as_u64(),
        Some(counted.growing_streamed_groups),
        "the golden's trap count and the fixture tree disagree"
    );
    assert_eq!(
        golden["traps"]["synthetic_records"].as_u64(),
        Some(counted.synthetic_records),
        "the golden's synthetic count and the fixture tree disagree"
    );
}
