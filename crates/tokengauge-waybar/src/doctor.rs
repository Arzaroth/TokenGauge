//! `--doctor`: what this machine looks like to TokenGauge.
//!
//! Every check is a [`DoctorCheck`] rather than a `println!`, so the section
//! that decides *what* is wrong is separate from the one that decides how to
//! say it - and so a check can be asserted in a test.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tokengauge_core::update;
use tokengauge_core::{fetch_all_providers, load_config};

use crate::*;

pub struct DoctorCheck {
    pub label: String,
    pub ok: bool,
    pub detail: String,
}

pub(crate) fn handle_doctor(config_path: &Path) -> i32 {
    let isatty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let (green, red, dim, reset) = if isatty {
        ("\x1b[32m", "\x1b[31m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "", "")
    };
    let section = |title: &str| {
        println!("\n{title}");
        println!("{}", "─".repeat(title.chars().count()));
    };
    let print_check = |c: &DoctorCheck| {
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
    };

    let checks: std::cell::RefCell<Vec<DoctorCheck>> = std::cell::RefCell::new(Vec::new());
    let record = |c: DoctorCheck| {
        print_check(&c);
        checks.borrow_mut().push(c);
    };

    println!("TokenGauge doctor");

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
    for provider in tokengauge_core::PROVIDERS {
        if !cfg.providers.is_enabled(provider) {
            continue;
        }
        let status = tokengauge_core::provider_auth_status(provider);
        record(DoctorCheck {
            label: format!("{provider} credentials"),
            ok: status.ok,
            detail: if status.ok {
                status.detail
            } else {
                format!("{} - {}", status.detail, status.hint)
            },
        });
        // When creds are missing and a sign-in CLI exists, report whether it's
        // installed so the fix is actionable. (TokenGauge reads the credential
        // file/env at runtime, not the CLI, so this only matters for sign-in.)
        if !status.ok
            && let Some(cli) = tokengauge_core::provider_cli_name(provider)
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
        let compare = cfg.cost_source != tokengauge_core::CostSource::Ccusage
            && tokengauge_core::ccusage_runner_description().is_some();
        let d = tokengauge_core::diagnose_costs(
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
        let native_required = cfg.cost_source == tokengauge_core::CostSource::Native;
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
                    tokengauge_core::cost::pricing::price_cache_path(&cfg.cache_file).display()
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
        let required = cfg.cost_source == tokengauge_core::CostSource::Ccusage;
        match tokengauge_core::ccusage_runner_description() {
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

    // Cache + state files
    section("Filesystem");
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

    // Providers
    section("Providers");
    let enabled = cfg.providers.enabled_providers();
    let disabled: Vec<&str> = tokengauge_core::PROVIDERS
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

    // Bar wiring. Waybar is one surface of several now, so its module is only
    // missing-and-wrong when nothing else is drawing the gauge; on a desktop
    // running the Plasma applet, the GNOME extension or the Omarchy widget,
    // having no waybar config is the normal state and not a fault.
    section("Bar wiring");
    let drawn_by: Vec<&str> = tokengauge_core::frontend::installed()
        .iter()
        .map(|f| f.label)
        .collect();
    let drawn_by_text = drawn_by.join(", ");
    let waybar_cfg = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".config/waybar/config.jsonc"))
        .unwrap_or_else(|| PathBuf::from("~/.config/waybar/config.jsonc"));
    if waybar_cfg.exists() {
        let contents = std::fs::read_to_string(&waybar_cfg).unwrap_or_default();
        let wired = contents.contains("custom/tokengauge");
        record(DoctorCheck {
            label: format!("module wired in {}", waybar_cfg.display()),
            ok: wired || !drawn_by.is_empty(),
            detail: match (wired, drawn_by.is_empty()) {
                (true, _) => String::new(),
                (false, true) => {
                    "run scripts/install.sh to add the custom/tokengauge module".into()
                }
                (false, false) => format!("not wired, and not needed: {drawn_by_text} draws it"),
            },
        });
    } else if drawn_by.is_empty() {
        record(DoctorCheck {
            label: "no bar wired up".into(),
            ok: false,
            detail: format!(
                "no {} and no desktop frontend installed - run scripts/install.sh, or tokengauge --install-frontend <plasma|gnome|omarchy>",
                waybar_cfg.display()
            ),
        });
    } else {
        record(DoctorCheck {
            label: format!("waybar not in use - {drawn_by_text} draws the gauge"),
            ok: true,
            detail: String::new(),
        });
    }

    // Click action prerequisites: the binary the user wants to spawn
    // on left-click must be on PATH.
    let click_cmd = resolve_click_command(&cfg);
    let (label, ok, detail) = if click_cmd.is_empty() {
        (
            "click action launcher resolved".into(),
            false,
            "no TUI launcher found; set [waybar].tui_command or install a terminal".into(),
        )
    } else {
        let first = click_cmd
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        let on_path = tokengauge_core::launch::which(&first).is_some() || first.starts_with('/');
        (
            format!(
                "click action: {:?} -> {}",
                cfg.waybar.click_action, click_cmd
            ),
            on_path,
            if on_path {
                String::new()
            } else {
                format!("'{first}' not found on $PATH")
            },
        )
    };
    record(DoctorCheck { label, ok, detail });

    for check in sync_cli::doctor_checks(&cfg) {
        if check.label == sync_cli::SECTION_MARKER {
            section("Fleet sync");
        } else {
            record(check);
        }
    }

    section("Updates");
    record(DoctorCheck {
        label: format!("installed version: {}", update::current_version()),
        ok: true,
        detail: String::new(),
    });
    match tokengauge_core::read_update_status(&cfg.cache_file) {
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

    section("Desktop frontends");
    {
        use tokengauge_core::frontend;
        let binary = update::current_version();
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

    println!();
    let failed = checks.borrow().iter().filter(|c| !c.ok).count();
    if failed == 0 {
        println!("{green}All checks passed.{reset}");
        0
    } else {
        println!("{red}{failed} check(s) failed.{reset}");
        1
    }
}

pub(crate) fn check_binary(name: &str, purpose: &str, hint: &str) -> DoctorCheck {
    let found = Command::new("which")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    DoctorCheck {
        label: format!("{name} on PATH ({purpose})"),
        ok: found,
        detail: if found { String::new() } else { hint.into() },
    }
}
