//! The binary, end to end, against providers that are not signed in.
//!
//! Everything else in this repository tests a function. This runs the shipped
//! executable the way a frontend runs it - read the config, decide whether the
//! snapshot on disk can answer, resolve the panel, print JSON - on a machine
//! with no credentials, no daemon and no network.
//!
//! That is not a limitation, it is the point. `cache_is_stale()` is the single
//! fetch-or-serve decision, and a snapshot seeded fresh means every one of
//! these runs serves rather than fetches: the panel that comes out is the one
//! `panel.rs` resolved, carried through the config, the cache reader, the
//! provider filter and the JSON writer. The one test that *does* want a fetch
//! backdates the file, and lands in the credential walk rather than on the
//! network, because a provider with no token says so before it asks anyone.
//!
//! The four QML and TypeScript frontends read the other side of this boundary;
//! `tests/qml` and `tests/gnome` drive them against a recording of what comes
//! out of here.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// A machine with a config, a snapshot, and nothing else of ours.
struct Machine {
    root: PathBuf,
}

impl Machine {
    /// `providers` is written into `[providers]` verbatim, so a test can enable
    /// a provider the seeded snapshot does not hold and vice versa.
    fn with(providers: &[(&str, bool)], primary: Option<&str>, window: &str) -> Machine {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("tg-e2e-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).expect("a state dir");

        let mut config = format!(
            "refresh_secs = 3600\n\
             cache_file = {:?}\n\
             ccusage_enabled = false\n\n\
             [providers]\n",
            root.join("state/tokengauge-usage.json"),
        );
        for (name, enabled) in providers {
            config.push_str(&format!("{name} = {enabled}\n"));
        }
        config.push_str(&format!("\n[waybar]\nwindow = \"{window}\"\n"));
        if let Some(primary) = primary {
            config.push_str(&format!("primary = \"{primary}\"\n"));
        }
        std::fs::write(root.join("config.toml"), config).expect("the config");

        Machine { root }
    }

    fn cache(&self) -> PathBuf {
        self.root.join("state/tokengauge-usage.json")
    }

    /// Write the recorded snapshot, with the instants it has to carry filled in.
    ///
    /// Three of its fields cannot be recorded: a window whose reset has passed
    /// makes the snapshot stale by the rollover rule however young it is, and a
    /// checked-in `resetsAt` is in the past the day after it is written. So the
    /// fixture pins the *format* - the one users have on disk, which this reads
    /// exactly as a released binary would - and the clock is substituted in.
    fn seed(&self) {
        let now = chrono::Utc::now();
        let iso = |dt: chrono::DateTime<chrono::Utc>| dt.to_rfc3339();
        let text = std::fs::read_to_string(fixtures().join("snapshot.json"))
            .expect("the recorded snapshot")
            .replace("{{SESSION_RESET}}", &iso(now + chrono::Duration::hours(2)))
            .replace("{{WEEKLY_RESET}}", &iso(now + chrono::Duration::days(3)))
            .replace("{{UPDATED_AT}}", &iso(now))
            .replace("{{UPDATED_AT_MS}}", &now.timestamp_millis().to_string());
        std::fs::write(self.cache(), text).expect("the seeded snapshot");
    }

    /// Run the shipped binary the way a frontend runs it.
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let config = self.root.join("config.toml");
        let out = Command::new(env!("CARGO_BIN_EXE_tokengauge"))
            .arg("--config")
            .arg(&config)
            .args(args)
            // An isolated home, or the machine running these tests answers with
            // its own credentials, its own snapshot and its own daemon socket.
            .env("HOME", &self.root)
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", &self.root)
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            // The keychain walk is compiled out on Linux and the file is not
            // there, but an inherited token would make "not signed in" a lie.
            .env_remove("TOKENGAUGE_CLAUDE_OAUTH_TOKEN")
            .output()
            .expect("the binary runs");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn json(&self) -> Value {
        let (code, stdout, stderr) = self.run(&["--json"]);
        assert_eq!(code, 0, "--json failed: {stderr}");
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("not JSON: {e}\n{stdout}"))
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A row by provider, matched without regard to case: the id in the config is
/// `claude` and the id on the row is `Claude`, because the row carries the
/// provider's own spelling of its name.
fn row<'a>(snapshot: &'a Value, provider: &str) -> &'a Value {
    snapshot["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|r| {
            r["provider"]
                .as_str()
                .is_some_and(|p| p.eq_ignore_ascii_case(provider))
        })
        .unwrap_or_else(|| panic!("no {provider} row in {}", providers_in(snapshot).join(", ")))
}

fn providers_in(snapshot: &Value) -> Vec<String> {
    snapshot["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| r["provider"].as_str().unwrap_or("").to_lowercase())
        .collect()
}

fn section<'a>(row: &'a Value, id: &str) -> &'a Value {
    row["panel"]
        .as_array()
        .expect("panel")
        .iter()
        .find(|s| s["id"] == id)
        .unwrap_or_else(|| panic!("no section {id}"))
}

fn labels(section: &Value) -> Vec<String> {
    section["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| r["label"].as_str().unwrap_or("").to_string())
        .collect()
}

/// A snapshot young enough to answer is answered with, not refetched.
///
/// Nothing on this machine is signed in, so a fetch would put a "not signed in"
/// error where the row is - which is exactly what the stale test below gets.
/// An empty `errors` with a full row is the proof that no provider was asked.
#[test]
fn a_fresh_snapshot_is_served_without_asking_a_provider() {
    let machine = Machine::with(&[("claude", true)], Some("claude"), "weekly");
    machine.seed();
    let before = std::fs::read_to_string(machine.cache()).expect("the seed");

    let snapshot = machine.json();

    assert_eq!(snapshot["errors"].as_array().expect("errors").len(), 0);
    assert_eq!(snapshot["rows"].as_array().expect("rows").len(), 1);
    assert_eq!(row(&snapshot, "claude")["stale"], false);
    assert_eq!(
        std::fs::read_to_string(machine.cache()).expect("the cache"),
        before,
        "serving a snapshot must not rewrite it"
    );
}

/// The panel a frontend draws, resolved by the binary rather than by the
/// frontend. Section order, kinds and every string come from `panel.rs`; this
/// is the only test that sees them the way a frontend does.
#[test]
fn the_panel_a_frontend_draws_comes_out_of_the_binary() {
    let machine = Machine::with(&[("claude", true)], Some("claude"), "weekly");
    machine.seed();
    let snapshot = machine.json();
    let claude = row(&snapshot, "claude");

    let sections: Vec<(&str, &str)> = claude["panel"]
        .as_array()
        .expect("panel")
        .iter()
        .map(|s| {
            (
                s["id"].as_str().unwrap_or(""),
                s["kind"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        sections,
        [("limits", "meters"), ("cost", "rows")],
        "the seeded snapshot has limits and cost and no store behind it, so \
         those are the sections the spec keeps"
    );

    let limits = section(claude, "limits");
    assert_eq!(labels(limits), ["Session", "Weekly (all)"]);
    assert_eq!(limits["rows"][0]["value"], "31%");
    assert_eq!(limits["rows"][0]["tone"], "good");
    assert_eq!(limits["rows"][0]["fraction"], 0.31);
    assert_eq!(limits["rows"][1]["value"], "68%");
    assert_eq!(limits["rows"][1]["tone"], "warn");
    // The countdown is measured at render time, not at the fetch, so the only
    // thing that can be asserted about it is that it counts down.
    assert!(
        limits["rows"][0]["footnote"]
            .as_str()
            .unwrap_or("")
            .starts_with("Resets in "),
        "{:?}",
        limits["rows"][0]["footnote"]
    );

    let cost = section(claude, "cost");
    assert_eq!(labels(cost), ["Today", "This month"]);
    assert_eq!(cost["rows"][0]["value"], "$12.50");
    assert_eq!(cost["rows"][0]["suffix"], "384.0K tokens");
}

/// The bar icon's hover summary can never name a window the panel under it
/// does not draw. `bar_tooltip` is built off `panel_spec` for that reason;
/// this holds the two against each other at the far end of the JSON.
#[test]
fn the_bar_summary_names_only_windows_the_panel_draws() {
    let machine = Machine::with(&[("claude", true)], Some("claude"), "weekly");
    machine.seed();
    let snapshot = machine.json();
    let claude = row(&snapshot, "claude");

    let tip = &claude["bar_tooltip"];
    assert_eq!(tip["title"], "Claude");
    let lines: Vec<String> = tip["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|l| l["label"].as_str().unwrap_or("").to_string())
        .collect();

    let limits = labels(section(claude, "limits"));
    assert_eq!(lines[..limits.len()], limits[..]);
    assert_eq!(
        lines.len(),
        limits.len() + 1,
        "every limit, then one money line"
    );
    assert_eq!(tip["lines"][0]["value"], "31%");
    assert_eq!(tip["lines"][0]["tone"], "good");
    assert_eq!(
        tip["lines"].as_array().unwrap().last().unwrap()["tone"],
        "normal"
    );
}

/// The headline number follows the configured window, and the pin decides
/// which row reports it. Three frontends each used to pick the window
/// themselves and carry their own copy of the tier boundaries.
#[test]
fn the_bar_follows_the_window_and_the_pin() {
    let weekly = Machine::with(
        &[("claude", true), ("codex", true)],
        Some("claude"),
        "weekly",
    );
    weekly.seed();
    let snapshot = weekly.json();
    assert_eq!(snapshot["window"], "weekly");
    assert_eq!(snapshot["primary"], "claude");
    assert_eq!(row(&snapshot, "claude")["bar"]["percent"], 68);
    assert_eq!(row(&snapshot, "claude")["bar"]["tone"], "warn");
    assert_eq!(row(&snapshot, "codex")["bar"]["percent"], 44);

    let daily = Machine::with(&[("claude", true)], Some("claude"), "daily");
    daily.seed();
    assert_eq!(row(&daily.json(), "claude")["bar"]["percent"], 31);
}

/// Repinning is config work, not a fetch, and every frontend has to hear about
/// it: it moves the revision file the watchers are parked on.
#[test]
fn repinning_moves_the_revision_file_without_asking_a_provider() {
    let machine = Machine::with(
        &[("claude", true), ("codex", true)],
        Some("claude"),
        "weekly",
    );
    machine.seed();
    let snapshot = machine.json();
    let revision = PathBuf::from(snapshot["revision_file"].as_str().expect("a revision file"));
    assert_eq!(
        revision.parent(),
        machine.cache().parent(),
        "every state file is derived from the snapshot's parent"
    );
    let before = std::fs::read_to_string(&revision).unwrap_or_default();

    let (code, _, stderr) = machine.run(&["--set-primary", "codex"]);
    assert_eq!(code, 0, "--set-primary failed: {stderr}");

    let after = machine.json();
    assert_eq!(after["primary"], "codex");
    assert_eq!(after["errors"].as_array().expect("errors").len(), 0);
    assert_ne!(
        std::fs::read_to_string(&revision).unwrap_or_default(),
        before,
        "a frontend watching the revision file would never have re-read"
    );
}

/// A provider switched off disappears from a snapshot that still holds it.
/// The cache is written by whichever set was enabled at fetch time, so a
/// toggle leaves it carrying rows the user just turned off; the filter runs on
/// every read rather than waiting for the next fetch to catch up.
#[test]
fn a_provider_switched_off_is_filtered_out_of_a_snapshot_that_still_holds_it() {
    let machine = Machine::with(
        &[("claude", true), ("codex", false)],
        Some("claude"),
        "weekly",
    );
    machine.seed();
    let snapshot = machine.json();

    assert_eq!(
        providers_in(&snapshot),
        ["claude"],
        "codex is in the file, not in the panel"
    );
    assert_eq!(snapshot["enabled"], serde_json::json!(["claude"]));
    assert_eq!(
        snapshot["errors"].as_array().expect("errors").len(),
        0,
        "filtering must not look like a refetch"
    );
}

/// A window that has reset since the snapshot was written makes it stale
/// however young it is, because the percentages beside that window describe a
/// window that no longer exists.
///
/// The fetch that follows gets as far as the credential walk and stops: a
/// provider with no token says so without asking anyone, which is what makes
/// this assertable offline. What comes back is the whole stale path in one
/// go - the last good figures served rather than an empty panel, and the
/// reason carried on the payload as a `status` section rather than as a bare
/// `stale` chip, because that chip is the same word whether the network
/// blipped once or a credential expired weeks ago.
#[test]
fn a_window_that_has_reset_sends_the_binary_back_to_the_provider() {
    let machine = Machine::with(&[("claude", true)], Some("claude"), "weekly");
    machine.seed();

    // Backdate the write and put the reset between then and now. Both halves
    // matter: the rule compares the reset against the write, not against now
    // alone, or a provider reporting an instant already past would be refetched
    // on every single render.
    let text = std::fs::read_to_string(machine.cache()).expect("the seed");
    let passed = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
    let reset_at = regex_free_replace_reset(&text, &passed);
    std::fs::write(machine.cache(), reset_at).expect("the backdated snapshot");
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    filetime_set(&machine.cache(), hour_ago);

    let snapshot = machine.json();
    let claude = row(&snapshot, "claude");

    assert_eq!(claude["stale"], true, "the fetch was not attempted");
    let reason = claude["stale_reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("not signed in"),
        "a machine with no token must say so rather than report a network \
         failure: {reason:?}"
    );
    assert_eq!(
        claude["weekly_used"], 68,
        "the last good figures are still served"
    );

    // The error the fallback absorbed is a section, not a chip, and it leads.
    let status = section(claude, "status");
    assert_eq!(status["kind"], "rows");
    assert_eq!(claude["panel"][0]["id"], "status");
    assert_eq!(status["rows"][0]["label"], "Stale");
    assert_eq!(status["rows"][0]["badge"], reason);
    assert_eq!(
        snapshot["errors"].as_array().expect("errors").len(),
        0,
        "a covered failure is on the payload, not beside it"
    );
}

/// Nothing here waits on GitHub. `update` is written by `--check-update` and
/// by the daemon, never by a render, so a panel drawn before either has run
/// says there is no news rather than going to find out.
#[test]
fn the_panel_never_waits_on_github() {
    let machine = Machine::with(&[("claude", true)], Some("claude"), "weekly");
    machine.seed();
    let started = std::time::Instant::now();
    let snapshot = machine.json();
    assert!(
        snapshot["update"].is_null(),
        "a render resolved an update status: {}",
        snapshot["update"]
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "a render took {:?} - something went to the network",
        started.elapsed()
    );
}

/// Replace the first `resetsAt` with an instant that has passed, leaving the
/// rest of the recording alone. A dependency for one substitution is not worth
/// it, and the fixture's shape is fixed enough to index into.
fn regex_free_replace_reset(text: &str, passed: &str) -> String {
    let key = "\"resetsAt\": \"";
    let start = text.find(key).expect("a resetsAt") + key.len();
    let end = start + text[start..].find('"').expect("its closing quote");
    format!("{}{passed}{}", &text[..start], &text[end..])
}

fn filetime_set(path: &Path, when: std::time::SystemTime) {
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("the cache file");
    file.set_times(std::fs::FileTimes::new().set_modified(when))
        .expect("backdating the write");
}
