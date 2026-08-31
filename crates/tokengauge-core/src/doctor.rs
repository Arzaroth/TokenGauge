//! `--doctor`: what this machine looks like to TokenGauge.
//!
//! Lives in the core because the report is the same question on every
//! platform, and the binary that used to own it is Linux-only - so the users
//! most likely to need a diagnostic, on the OS with no waybar, no daemon and
//! no `--json`, were the ones who could not run one.
//!
//! [`doctor_lines`] decides *what* is wrong and [`render`] decides how to say
//! it. They used to be one function that computed and printed in the same
//! breath, which meant nothing could ask the doctor a question without also
//! producing a page of output - so nothing ever did, and a 500-line report had
//! no test behind it.
//!
//! A heading is a line in the list rather than a `println!` between two of
//! them, so the report stays one ordered document that a caller can walk. That
//! also retires the magic label string the sync section used to smuggle its own
//! heading through the same list.

use std::path::Path;

use crate::{TokenGaugeConfig, fetch_all_providers, load_config};

pub struct DoctorCheck {
    pub label: String,
    pub ok: bool,
    pub detail: String,
}

/// One line of the report. A heading is part of the list rather than something
/// printed between two lists, which is what lets the whole report be a value.
pub enum DoctorLine {
    Heading(&'static str),
    Check(DoctorCheck),
}

/// Print the report and return the exit code: non-zero when any check failed,
/// so a script can act on it.
///
/// `extras` is where a frontend adds the checks only it can make - the waybar
/// binary contributes bar wiring and fleet sync. A surface with none passes a
/// closure returning nothing rather than getting a report full of checks about
/// a bar it does not have.
pub fn handle_doctor(
    config_path: &Path,
    version: &str,
    extras: impl Fn(&TokenGaugeConfig) -> Vec<DoctorLine>,
) -> i32 {
    render(&doctor_lines(config_path, version, extras))
}

pub fn render(lines: &[DoctorLine]) -> i32 {
    let isatty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let (green, red, dim, reset) = if isatty {
        ("\x1b[32m", "\x1b[31m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "", "")
    };

    println!("TokenGauge doctor");
    for line in visible(lines) {
        match line {
            DoctorLine::Heading(title) => {
                println!("\n{title}");
                println!("{}", "─".repeat(title.chars().count()));
            }
            DoctorLine::Check(c) => {
                let icon = if c.ok {
                    format!("{green}✓{reset}")
                } else {
                    format!("{red}✗{reset}")
                };
                if c.detail.is_empty() {
                    println!("  {icon}  {}", c.label);
                } else {
                    println!("  {icon}  {}  {dim}- {}{reset}", c.label, c.detail);
                }
            }
        }
    }

    println!();
    let failed = failures(lines);
    if failed == 0 {
        println!("{green}All checks passed.{reset}");
        0
    } else {
        println!("{red}{failed} check(s) failed.{reset}");
        1
    }
}

/// The report minus any heading with no checks under it.
///
/// A section can legitimately come out empty - "Credentials" on a machine with
/// no provider enabled - and a heading with nothing beneath it reads as a
/// section that failed to run. Dropping it is the renderer's call, not the
/// checks': `doctor_lines` should not have to know whether a section it built
/// turned out to have anything in it.
pub fn visible(lines: &[DoctorLine]) -> Vec<&DoctorLine> {
    let mut out: Vec<&DoctorLine> = Vec::new();
    for line in lines {
        if matches!(line, DoctorLine::Heading(_))
            && matches!(out.last(), Some(DoctorLine::Heading(_)))
        {
            out.pop();
        }
        out.push(line);
    }
    if matches!(out.last(), Some(DoctorLine::Heading(_))) {
        out.pop();
    }
    out
}

/// How many checks failed. The exit code, and what a test asserts on.
pub fn failures(lines: &[DoctorLine]) -> usize {
    lines
        .iter()
        .filter(|line| matches!(line, DoctorLine::Check(c) if !c.ok))
        .count()
}

/// Everything the doctor found, in report order. No output: a caller that only
/// wants to know whether something is wrong should not have to print a page to
/// find out.
pub fn doctor_lines(
    config_path: &Path,
    // The version of the binary that invoked the doctor, not the core crate's
    // own. They move together under one workspace bump, but the report is about
    // the tool the user ran - and it is the frontends section that compares a
    // desktop frontend's version against it.
    version: &str,
    extras: impl Fn(&TokenGaugeConfig) -> Vec<DoctorLine>,
) -> Vec<DoctorLine> {
    let lines: std::cell::RefCell<Vec<DoctorLine>> = std::cell::RefCell::new(Vec::new());
    let section = |title: &'static str| lines.borrow_mut().push(DoctorLine::Heading(title));
    let record = |c: DoctorCheck| lines.borrow_mut().push(DoctorLine::Check(c));

    // Config
    section("Config");
    let config = if config_path.exists() {
        match load_config(Some(config_path.to_path_buf())) {
            Ok(c) => {
                record(DoctorCheck {
                    label: format!("config loads: {}", config_path.display()),
                    ok: true,
                    detail: String::new(),
                });
                Some(c)
            }
            Err(e) => {
                record(DoctorCheck {
                    label: format!("config loads: {}", config_path.display()),
                    ok: false,
                    detail: e.to_string(),
                });
                None
            }
        }
    } else {
        record(DoctorCheck {
            label: format!("config exists: {}", config_path.display()),
            ok: false,
            detail: "run any tokengauge invocation to write defaults".into(),
        });
        None
    };

    let cfg = config.unwrap_or_default();

    // Credentials (read by the native fetchers) - one line per enabled
    // provider, keyed off the auth sources each fetcher actually reads.
    section("Credentials");
    for provider in crate::PROVIDERS {
        if !cfg.providers.is_enabled(provider) {
            continue;
        }
        let status = crate::provider_auth_status(provider);
        record(DoctorCheck {
            label: format!("{provider} credentials"),
            ok: status.ok,
            detail: if status.ok || status.hint.is_empty() {
                status.detail
            } else {
                format!("{} - {}", status.detail, status.hint)
            },
        });
        // When creds are missing and a sign-in CLI exists, report whether it's
        // installed so the fix is actionable. (TokenGauge reads the credential
        // file/env at runtime, not the CLI, so this only matters for sign-in.)
        if !status.ok
            && let Some(cli) = crate::provider_cli_name(provider)
        {
            record(check_binary(
                cli,
                &format!("{provider} sign-in"),
                &format!("install the {cli} CLI to sign in"),
            ));
        }
    }

    // Unknown / removed config keys
    let unknown = cfg.unknown_config_keys();
    if !unknown.is_empty() {
        section("Removed config keys");
        for key in &unknown {
            record(DoctorCheck {
                label: format!("unknown config key `{key}`"),
                ok: false,
                detail: "unknown or removed key - delete it from your config".into(),
            });
        }
    }

    // Where cost figures come from, and whether the two sources agree.
    if cfg.ccusage_enabled {
        section("Cost source");
        let compare = cfg.cost_source != crate::CostSource::Ccusage
            && crate::ccusage_runner_description().is_some();
        let d = crate::diagnose_costs(
            cfg.cost_source,
            &cfg.cache_file,
            std::time::Duration::from_secs(cfg.ccusage_timeout_secs.max(1)),
            compare,
        );

        record(DoctorCheck {
            label: format!("cost source: {:?}", d.source).to_lowercase(),
            ok: true,
            detail: format!(
                "{} events read in {}ms from {} transcript root(s)",
                d.events,
                d.elapsed.as_millis(),
                d.roots.len()
            ),
        });
        // Only `native` actually requires a transcript tree. A ccusage-only
        // install has none by design, and `auto` covers the same case through
        // the fallback, so failing the check there would fail a healthy machine.
        let native_required = cfg.cost_source == crate::CostSource::Native;
        record(DoctorCheck {
            label: if d.roots.is_empty() {
                "no transcript roots".into()
            } else {
                "transcripts found".into()
            },
            ok: !d.roots.is_empty() || !native_required,
            detail: if d.roots.is_empty() {
                if native_required {
                    "cost_source = \"native\" needs ~/.claude/projects or ~/.codex/sessions".into()
                } else {
                    "none yet - cost figures come from ccusage until a CLI writes one".into()
                }
            } else {
                d.roots
                    .iter()
                    .map(|r| r.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        });
        // Which of the four fallbacks the table came from. A machine that has
        // never reached LiteLLM rates everything against the copy compiled in
        // on release day - correct, and invisible from the cost row, where it
        // shows up only as a model priced since then reading as unpriced.
        record(DoctorCheck {
            label: format!(
                "price table: {} models, {}",
                d.prices,
                d.price_source.label()
            ),
            ok: d.prices > 0 && d.price_source.is_current(),
            detail: if d.price_source.is_current() {
                "from LiteLLM, cached beside the snapshot".into()
            } else {
                format!(
                    "{}; a model priced since will read as unpriced below",
                    crate::cost::pricing::price_cache_path(&cfg.cache_file).display()
                )
            },
        });
        // An unpriced model must read as a gap, never as $0 spent.
        record(DoctorCheck {
            label: if d.unpriced.is_empty() {
                "every model in use is priced".into()
            } else {
                format!("{} model(s) have tokens but no price", d.unpriced.len())
            },
            ok: d.unpriced.is_empty(),
            detail: if d.unpriced.is_empty() {
                String::new()
            } else {
                format!(
                    "{} - spend is undercounted until the price table catches up",
                    d.unpriced.join(", ")
                )
            },
        });
        if compare {
            match d.worst_token_drift() {
                // Token counts come from the same transcripts and must agree;
                // anything else means one of the two parsers has drifted.
                Some((provider, drift)) => record(DoctorCheck {
                    label: "native and ccusage agree on token counts".into(),
                    ok: drift < 0.01,
                    detail: format!(
                        "worst gap {:.2}% ({provider}, month to date)",
                        drift * 100.0
                    ),
                }),
                None => record(DoctorCheck {
                    label: "native vs ccusage".into(),
                    ok: true,
                    detail: "nothing in common to compare this month".into(),
                }),
            }
        }
    }

    // External dependencies
    section("Dependencies");
    if cfg.ccusage_enabled {
        // Only a hard requirement when it is the chosen source. Otherwise it is
        // the fallback for a CLI we do not parse, and the second opinion the
        // cost check above diffs against.
        let required = cfg.cost_source == crate::CostSource::Ccusage;
        match crate::ccusage_runner_description() {
            Some(cmd) => record(DoctorCheck {
                label: "ccusage runner available".into(),
                ok: true,
                detail: cmd,
            }),
            None if required => record(DoctorCheck {
                label: "ccusage runner".into(),
                ok: false,
                detail: "cost_source = \"ccusage\" needs it: npm i -g ccusage / bun i -g ccusage, or switch cost_source to \"native\"".into(),
            }),
            None => record(DoctorCheck {
                label: "ccusage runner absent (optional)".into(),
                ok: true,
                detail: "costs are read natively; install ccusage only to cross-check them".into(),
            }),
        }
    } else {
        record(DoctorCheck {
            label: "ccusage disabled in config".into(),
            ok: true,
            detail: "no cost data".into(),
        });
    }
    // Both are Linux desktop tools. On Windows they are not missing, they are
    // not the mechanism, and a red cross next to one is a fault invented by
    // the report.
    #[cfg(unix)]
    {
        if cfg.notifications.enabled {
            record(check_binary(
                "notify-send",
                "threshold notifications",
                "install libnotify, or set notifications.enabled = false",
            ));
        }
        record(check_binary(
            "xdg-open",
            "open dashboard/status URLs",
            "install xdg-utils",
        ));
    }

    // Cache + state files
    section("Filesystem");
    record(DoctorCheck {
        label: format!("snapshot: {}", cfg.cache_file.display()),
        ok: true,
        detail: if cfg.cache_file.exists() {
            String::new()
        } else {
            "not written yet".into()
        },
    });
    // A path from another OS - `/tmp/tokengauge-usage.json` left in a Windows
    // config by an old installer - is rooted but carries no drive, so Windows
    // resolves it against whichever drive is current and the snapshot lands
    // somewhere nobody thinks to look. It reads as the file simply not
    // existing, which is the one thing it is not.
    if !cfg.cache_file.is_absolute() {
        let resolved =
            std::path::absolute(&cfg.cache_file).unwrap_or_else(|_| cfg.cache_file.clone());
        record(DoctorCheck {
            label: format!(
                "cache_file is not an absolute path: {}",
                cfg.cache_file.display()
            ),
            ok: false,
            detail: format!(
                "resolves to {} - delete the `cache_file` line to use the default",
                resolved.display()
            ),
        });
        // No writable check, and no directory created for it: the path has
        // just been called wrong, and creating it would scatter an empty
        // state directory wherever the process happened to start.
    } else {
        let cache_dir = cfg.cache_file.parent().unwrap_or(Path::new("."));
        let cache_ok = std::fs::create_dir_all(cache_dir).is_ok();
        record(DoctorCheck {
            label: format!("cache directory writable: {}", cache_dir.display()),
            ok: cache_ok,
            detail: if cache_ok {
                String::new()
            } else {
                "permission denied".into()
            },
        });
    }

    // Providers
    section("Providers");
    let enabled = cfg.providers.enabled_providers();
    let disabled: Vec<&str> = crate::PROVIDERS
        .iter()
        .copied()
        .filter(|p| !cfg.providers.is_enabled(p))
        .collect();
    record(DoctorCheck {
        label: "enabled".into(),
        ok: !enabled.is_empty(),
        detail: if enabled.is_empty() {
            "none - set e.g. [providers] claude = true".into()
        } else {
            enabled.join(", ")
        },
    });
    if !disabled.is_empty() {
        // Surface the rest of the catalog so disabled providers are discoverable.
        record(DoctorCheck {
            label: "available (disabled)".into(),
            ok: true,
            detail: disabled.join(", "),
        });
    }
    if !enabled.is_empty() {
        // The definitive check: a live fetch per enabled provider.
        let result = fetch_all_providers(&cfg);
        for payload in &result.payloads {
            // A stale payload is last-good cache served because the live fetch
            // failed - report it as not-ok so --doctor doesn't read as success.
            let detail = payload.source.clone().unwrap_or_default();
            record(DoctorCheck {
                label: format!("live fetch {}", payload.provider),
                ok: !payload.stale,
                detail: if payload.stale {
                    format!("{detail} (stale cache - live fetch failed)")
                } else {
                    detail
                },
            });
        }
        for err in &result.errors {
            record(DoctorCheck {
                label: format!("live fetch {}", err.provider),
                ok: false,
                detail: err.message.clone(),
            });
        }
    }

    // Whatever the calling surface knows and the core does not: the waybar
    // binary contributes bar wiring, the click-action launcher and fleet
    // sync here. Called at this point rather than appended, so moving the
    // report into the core did not reorder it.
    for line in extras(&cfg) {
        lines.borrow_mut().push(line);
    }

    section("Updates");
    record(DoctorCheck {
        label: format!("installed version: {version}"),
        ok: true,
        detail: String::new(),
    });
    match crate::read_update_status(&cfg.cache_file) {
        Some(status) if status.available => record(DoctorCheck {
            label: "update available".into(),
            ok: true,
            detail: format!(
                "{} available - run: tokengauge --update",
                status.latest.as_deref().unwrap_or("newer release")
            ),
        }),
        Some(status) => record(DoctorCheck {
            label: "up to date".into(),
            ok: true,
            detail: status
                .latest
                .map(|v| format!("latest: {v}"))
                .unwrap_or_default(),
        }),
        None => record(DoctorCheck {
            label: "no update check yet".into(),
            ok: true,
            detail: "run: tokengauge --check-update".into(),
        }),
    }

    // Plasma, GNOME and Quickshell. There is nothing to install on Windows, so
    // "none installed - install one" would be advice with no action behind it.
    #[cfg(unix)]
    {
        section("Desktop frontends");
        use crate::frontend;
        let binary = version;
        let present = frontend::installed();
        if present.is_empty() {
            record(DoctorCheck {
                label: "none installed".into(),
                ok: true,
                detail: "install one: tokengauge --install-frontend <plasma|gnome|omarchy>".into(),
            });
        }
        for f in present {
            match f.installed_version() {
                // A frontend is QML or JavaScript installed outside the binary
                // directory, so it only moves when it is reinstalled. Skew here
                // reads as a missing feature rather than a stale install.
                Some(v) if v == binary => record(DoctorCheck {
                    label: format!("{} v{v}", f.label),
                    ok: true,
                    detail: String::new(),
                }),
                Some(v) => record(DoctorCheck {
                    label: format!("{} is v{v}, binary is v{binary}", f.label),
                    ok: false,
                    detail: format!("tokengauge --install-frontend {}", f.id),
                }),
                None => record(DoctorCheck {
                    label: format!("{} version unreadable", f.label),
                    ok: false,
                    detail: format!("tokengauge --install-frontend {}", f.id),
                }),
            }
        }
    }

    lines.into_inner()
}

pub fn check_binary(name: &str, purpose: &str, hint: &str) -> DoctorCheck {
    // The core's own PATH walk rather than spawning `which`: one fewer process
    // per checked binary, and no dependency on `which` itself being installed -
    // which the doctor would have reported as the binary being missing.
    let found = crate::launch::which(name).is_some();
    DoctorCheck {
        label: format!("{name} on PATH ({purpose})"),
        ok: found,
        detail: if found { String::new() } else { hint.into() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The doctor reports on a machine whose config may be broken, so it must
    /// not write a default one on the way past - that would replace the fault
    /// with a working file and report success. This is the first thing anyone
    /// could ask it, and until the checks were separable from the printing,
    /// asking meant printing a page.
    #[test]
    fn a_config_that_will_not_parse_is_reported_and_not_replaced() {
        let dir = std::env::temp_dir().join(format!(
            "tg-doctor-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");
        std::fs::write(&path, "refresh_secs = \"not a number\"\n").expect("write");

        let lines = doctor_lines(&path, "0.0.0-test", |_| Vec::new());
        assert!(
            failures(&lines) > 0,
            "a config that will not parse is a fault"
        );
        assert!(
            lines.iter().any(|line| matches!(
                line,
                DoctorLine::Check(c) if !c.ok && c.label.starts_with("config loads")
            )),
            "the report has to name which check failed"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("still there"),
            "refresh_secs = \"not a number\"\n",
            "diagnosing a config must not rewrite it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fault that took five rounds of screenshots to find on a real
    /// machine: a config carrying `/tmp/tokengauge-usage.json` on Windows,
    /// where it is rooted but driveless and so resolves against whichever
    /// drive is current. The snapshot is written somewhere nobody looks and
    /// the panel reads as though it had never been written at all.
    #[test]
    fn a_cache_path_that_will_not_resolve_where_it_says_is_a_fault() {
        let dir = std::env::temp_dir().join(format!(
            "tg-doctor-relpath-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");
        // Relative on every platform, and unique so a parallel run cannot see
        // another test's leftovers.
        let rel = format!("tg-doctor-rel-{}", std::process::id());
        std::fs::write(
            &path,
            format!(
                // `ccusage_enabled = false` keeps the cost section from
                // writing its price table beside the bogus path, which would
                // create the directory this test is about not creating.
                "refresh_secs = 600\nccusage_enabled = false\ncache_file = \"{rel}/usage.json\"\n[providers]\n"
            ),
        )
        .expect("write");

        let lines = doctor_lines(&path, "0.0.0-test", |_| Vec::new());
        assert!(
            lines.iter().any(|line| matches!(
                line,
                DoctorLine::Check(c) if !c.ok && c.label.starts_with("cache_file is not an absolute path")
            )),
            "a relative cache_file has to be reported, not silently honoured"
        );
        assert!(
            !Path::new(&rel).exists(),
            "the doctor must not create the directory it just called wrong"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A heading is never printed above an empty list. "Credentials" comes out
    /// empty on a machine with no provider enabled, and a heading with nothing
    /// under it reads as a section that failed to run.
    #[test]
    fn the_report_is_headings_and_checks_in_order() {
        let dir = std::env::temp_dir().join(format!(
            "tg-doctor-shape-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            format!(
                // ccusage off so the Cost source section makes no subprocess
                // call - the test is about section order, not about what a
                // machine happens to have on PATH.
                //
                // Single-quoted TOML literal: a Windows temp path has
                // backslashes, and in a double-quoted string `\U`/`\T` are
                // invalid escapes that fail the parse - which would silently
                // fall back to the default config (providers enabled) and break
                // this test's premise that the Credentials section is empty.
                "refresh_secs = 600\nccusage_enabled = false\ncache_file = '{}'\n[providers]\n",
                dir.join("usage.json").display()
            ),
        )
        .expect("write");

        let lines = doctor_lines(&path, "0.0.0-test", |_| Vec::new());
        let shown = visible(&lines);
        assert!(matches!(shown.first(), Some(DoctorLine::Heading(_))));
        assert!(
            matches!(shown.last(), Some(DoctorLine::Check(_))),
            "the report ends on a heading with nothing under it"
        );
        for pair in shown.windows(2) {
            if let [DoctorLine::Heading(a), DoctorLine::Heading(b)] = pair {
                panic!("`{a}` has no checks under it, only `{b}`");
            }
        }
        // And a section that really is empty is not silently swallowed along
        // with its checks: only the bare heading goes.
        assert!(shown.len() < lines.len(), "nothing was dropped at all");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
