//! One table that says everything TokenGauge knows about a provider.
//!
//! Nine facts used to live in nine `match` statements scattered across the
//! crate - the display label, the CLI its credentials come from, how to probe
//! those credentials, the glyph, the brand colour, the bundled logo, the
//! dashboard and status URLs, the three window labels, which config field
//! toggles it, whether a transcript reader can produce per-call events for it,
//! and how to fetch it. Nothing kept them in step: adding a provider meant
//! finding all nine, and forgetting one produced a provider that fetched fine
//! and rendered under its own bare id, or had no dashboard to middle-click to.
//!
//! Adding a provider is one row here plus its fetcher module. What cannot be a
//! row is [`ProvidersConfig`], because serde needs named fields to read
//! `[providers]` - so the row carries an accessor for its field instead, and a
//! test asserts every id in the table has one that works.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::Utc;

use crate::{ProviderPayload, ProvidersConfig, claude, codex, glm, grok, kimi};

/// Whether a provider's credentials are currently available, and where from.
pub struct AuthStatus {
    /// At least one accepted auth source is present.
    pub ok: bool,
    /// What was found (or what is missing).
    pub detail: String,
    /// How to satisfy it when missing (empty when `ok`).
    pub hint: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderUrls {
    pub dashboard: Option<&'static str>,
    pub status: Option<&'static str>,
}

pub struct ProviderIcon {
    pub glyph: &'static str,
    pub color_hex: &'static str,
}

/// Everything the rest of the crate needs to know about one provider.
pub struct ProviderMeta {
    /// The key in `[providers]`, in the snapshot, and on the wire.
    pub id: &'static str,
    /// How it is spelled to a user. Acronyms keep their case, which is the
    /// whole reason this is not `id.to_uppercase()`.
    pub label: &'static str,
    /// The CLI its credentials come from. `None` means an API key in the
    /// environment, and `--doctor` says so rather than naming a missing CLI.
    pub cli: Option<&'static str>,
    pub glyph: &'static str,
    pub color_hex: &'static str,
    /// Basename of the bundled brand SVG, when one ships.
    pub icon_slug: Option<&'static str>,
    pub urls: ProviderUrls,
    /// Labels for the session, weekly and tertiary windows. Every provider
    /// slices its limits differently and calls the slices different things.
    pub windows: (&'static str, &'static str, &'static str),
    /// True when a transcript reader produces per-call events for it. That is
    /// what fleet sync needs to bucket, and what `cost_source = "auto"` checks
    /// before falling back to ccusage.
    pub natively_read: bool,
    fetch: fn(Duration) -> Result<Vec<ProviderPayload>>,
    auth: fn() -> AuthStatus,
    /// `[providers]` is a serde struct with named fields, so the toggle cannot
    /// be a table lookup. This is the accessor for this provider's field.
    enabled_in: fn(&ProvidersConfig) -> Option<bool>,
}

pub const PROVIDER_META: &[ProviderMeta] = &[
    ProviderMeta {
        id: "codex",
        label: "Codex",
        cli: Some("codex"),
        glyph: "\u{f0b2b}",
        color_hex: "#74AA9C",
        icon_slug: Some("codex"),
        urls: ProviderUrls {
            dashboard: Some("https://platform.openai.com/usage"),
            status: Some("https://status.openai.com"),
        },
        windows: ("Session", "Weekly", "Tertiary"),
        natively_read: true,
        fetch: codex::fetch,
        auth: codex_auth,
        enabled_in: |c| c.codex,
    },
    ProviderMeta {
        id: "claude",
        label: "Claude",
        cli: Some("claude"),
        glyph: "\u{f0721}",
        color_hex: "#DE7356",
        icon_slug: Some("claude"),
        urls: ProviderUrls {
            dashboard: Some("https://claude.ai/settings/usage"),
            status: Some("https://status.anthropic.com"),
        },
        windows: ("Session", "Weekly (all)", "Weekly (Sonnet)"),
        natively_read: true,
        fetch: claude::fetch,
        auth: claude_auth,
        enabled_in: |c| c.claude,
    },
    ProviderMeta {
        id: "kimi",
        label: "Kimi",
        cli: Some("kimi"),
        glyph: "\u{f06a9}",
        color_hex: "#FE603C",
        icon_slug: Some("kimi"),
        urls: ProviderUrls {
            dashboard: Some("https://www.kimi.com/code/console"),
            status: None,
        },
        windows: ("Weekly", "Rate Limit", "Tertiary"),
        natively_read: true,
        fetch: kimi::fetch,
        auth: kimi_auth,
        enabled_in: |c| c.kimi,
    },
    ProviderMeta {
        id: "grok",
        label: "Grok",
        cli: Some("grok"),
        glyph: "\u{f06a9}",
        color_hex: "#000000",
        icon_slug: Some("grok"),
        urls: ProviderUrls {
            dashboard: Some("https://grok.com/?_s=usage"),
            status: Some("https://status.x.ai"),
        },
        windows: ("Weekly", "On-demand", "Tertiary"),
        natively_read: true,
        fetch: grok::fetch,
        auth: grok_auth,
        enabled_in: |c| c.grok,
    },
    ProviderMeta {
        id: "glm",
        label: "GLM",
        // An API key in the environment, so there is no CLI to name.
        cli: None,
        glyph: "\u{f06a9}",
        color_hex: "#E85A6A",
        icon_slug: Some("glm"),
        urls: ProviderUrls {
            dashboard: Some("https://zcode.z.ai/en"),
            status: None,
        },
        windows: ("Weekly", "30-day", "5-hour"),
        natively_read: false,
        fetch: glm::fetch,
        auth: glm_auth,
        enabled_in: |c| c.glm,
    },
];

/// The row for a provider, or `None` for a name from a config or a snapshot
/// that this build does not know.
pub fn provider_meta(id: &str) -> Option<&'static ProviderMeta> {
    let id = id.to_lowercase();
    PROVIDER_META.iter().find(|meta| meta.id == id)
}

/// Every provider id, in the order the table lists them - which is the order
/// they appear in the bar and in the settings pane.
pub const PROVIDERS: &[&str] = &["codex", "claude", "kimi", "grok", "glm"];

/// The providers a transcript reader can produce events for on its own.
///
/// Only these can take part in fleet sync, and only these are excluded from the
/// `auto` cost fallback: everything else legitimately reaches a cost row
/// through ccusage, so its absence from a native read says nothing.
pub fn natively_read() -> Vec<&'static str> {
    PROVIDER_META
        .iter()
        .filter(|meta| meta.natively_read)
        .map(|meta| meta.id)
        .collect()
}

/// The display label, or the input unchanged for a name this build does not
/// know - a snapshot written by a newer build still renders.
pub fn provider_label(name: &str) -> &str {
    match provider_meta(name) {
        Some(meta) => meta.label,
        None => name,
    }
}

/// The CLI a provider's credentials come from, if any. `None` means the
/// provider authenticates with an API key / env var and needs no CLI.
pub fn provider_cli_name(provider: &str) -> Option<&'static str> {
    provider_meta(provider)?.cli
}

pub fn provider_icon(label: &str) -> ProviderIcon {
    match provider_meta(label) {
        Some(meta) => ProviderIcon {
            glyph: meta.glyph,
            color_hex: meta.color_hex,
        },
        None => ProviderIcon {
            glyph: "\u{f06a9}",
            color_hex: crate::NEUTRAL_HEX,
        },
    }
}

/// Basename slug of the bundled brand SVG for a provider label, if one ships.
pub fn provider_icon_slug(label: &str) -> Option<&'static str> {
    provider_meta(label)?.icon_slug
}

pub fn provider_urls(provider: &str) -> ProviderUrls {
    match provider_meta(provider) {
        Some(meta) => meta.urls,
        None => ProviderUrls {
            dashboard: None,
            status: None,
        },
    }
}

/// Provider-specific labels for the three usage windows. Generic ones for a
/// provider this build does not know, so its gauges still have headings.
pub fn window_labels(provider: &str) -> (&'static str, &'static str, &'static str) {
    match provider_meta(provider) {
        Some(meta) => meta.windows,
        None => ("Session", "Weekly", "Tertiary"),
    }
}

pub fn fetch_single_provider(provider: &str, timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let meta = provider_meta(provider).ok_or_else(|| anyhow!("unknown provider {provider}"))?;
    (meta.fetch)(timeout)
}

/// Report a provider's credential presence without doing a network fetch.
/// Each probe mirrors the auth sources its fetcher actually reads.
pub fn provider_auth_status(provider: &str) -> AuthStatus {
    match provider_meta(provider) {
        Some(meta) => (meta.auth)(),
        None => AuthStatus {
            ok: false,
            detail: format!("unknown provider {provider}"),
            hint: "",
        },
    }
}

impl ProvidersConfig {
    /// Every provider switched on, in table order.
    pub fn enabled_providers(&self) -> Vec<&'static str> {
        PROVIDER_META
            .iter()
            .filter(|meta| (meta.enabled_in)(self).unwrap_or(false))
            .map(|meta| meta.id)
            .collect()
    }

    /// Whether one provider is switched on. Unknown names are off: a config
    /// naming a provider this build does not have must not render a row.
    pub fn is_enabled(&self, provider: &str) -> bool {
        provider_meta(provider).is_some_and(|meta| (meta.enabled_in)(self).unwrap_or(false))
    }
}

// ---------------------------------------------------------------------------
// Credential probes
// ---------------------------------------------------------------------------

pub fn claude_credentials_path() -> PathBuf {
    claude::credentials_path()
}

pub fn codex_auth_path() -> PathBuf {
    codex::auth_path()
}

pub fn kimi_credentials_path() -> PathBuf {
    kimi::credentials_path()
}

pub fn grok_auth_path() -> PathBuf {
    grok::auth_path()
}

fn env_var_present(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
}

fn file_auth_status(path: PathBuf, hint: &'static str) -> AuthStatus {
    if path.exists() {
        AuthStatus {
            ok: true,
            detail: path.display().to_string(),
            hint: "",
        }
    } else {
        AuthStatus {
            ok: false,
            detail: format!("{} not found", path.display()),
            hint,
        }
    }
}

fn claude_auth() -> AuthStatus {
    file_auth_status(claude_credentials_path(), "run `claude` to sign in")
}

fn codex_auth() -> AuthStatus {
    file_auth_status(codex_auth_path(), "run `codex` to sign in")
}

fn grok_auth() -> AuthStatus {
    match grok::credentials_valid(Utc::now()) {
        Ok(()) => AuthStatus {
            ok: true,
            detail: grok_auth_path().display().to_string(),
            hint: "",
        },
        Err(err) => AuthStatus {
            ok: false,
            detail: err.to_string(),
            hint: "run `grok login` to sign in",
        },
    }
}

fn kimi_auth() -> AuthStatus {
    // Mirrors kimi::resolve_auth, which prefers KIMI_CODE_API_KEY over the CLI
    // file and validates the file (parse + freshness) when it uses it.
    if env_var_present("KIMI_CODE_API_KEY") {
        return AuthStatus {
            ok: true,
            detail: "KIMI_CODE_API_KEY set".to_string(),
            hint: "",
        };
    }
    let path = kimi_credentials_path();
    match kimi::credentials_valid() {
        Ok(()) => AuthStatus {
            ok: true,
            detail: format!("{} (kimi CLI)", path.display()),
            hint: "",
        },
        Err(err) => AuthStatus {
            ok: false,
            detail: err.to_string(),
            hint: "sign in with `kimi` or set KIMI_CODE_API_KEY",
        },
    }
}

fn glm_auth() -> AuthStatus {
    match ["Z_AI_API_KEY", "ZAI_API_TOKEN"]
        .into_iter()
        .find(|v| env_var_present(v))
    {
        Some(var) => AuthStatus {
            ok: true,
            detail: format!("{var} set"),
            hint: "",
        },
        None => AuthStatus {
            ok: false,
            detail: "Z_AI_API_KEY unset".to_string(),
            hint: "set Z_AI_API_KEY (legacy ZAI_API_TOKEN also works)",
        },
    }
}

/// Directory the installer drops provider SVG logos into. Overridable with
/// `TOKENGAUGE_ICON_DIR` (e.g. point it at the repo `assets/providers` when
/// running a dev build).
pub fn provider_icon_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("TOKENGAUGE_ICON_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".local/share"));
    base.join("tokengauge").join("icons")
}

/// Path to a provider's bundled brand SVG, or None when no logo is installed
/// (the frontend then falls back to the glyph icon).
pub fn provider_icon_svg_path(label: &str) -> Option<PathBuf> {
    let slug = provider_icon_slug(label)?;
    let path = provider_icon_dir().join(format!("ProviderIcon-{slug}.svg"));
    path.exists().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // provider_label tests
    // ------------------------------------------------------------------------

    #[test]
    fn provider_label_known_providers() {
        assert_eq!(provider_label("claude"), "Claude");
        assert_eq!(provider_label("codex"), "Codex");
    }

    #[test]
    fn provider_label_unknown_returns_input() {
        assert_eq!(provider_label("unknown_provider"), "unknown_provider");
    }

    #[test]
    fn provider_icon_known_and_default() {
        assert_eq!(provider_icon("Claude").glyph, "\u{f0721}");
        assert_eq!(provider_icon("claude").color_hex, "#DE7356");
        assert_eq!(provider_icon("Codex").glyph, "\u{f0b2b}");
        assert_eq!(provider_icon("Unknown").glyph, "\u{f06a9}");
    }

    #[test]
    fn provider_cli_names() {
        assert_eq!(provider_cli_name("kimi"), Some("kimi"));
        assert_eq!(provider_cli_name("grok"), Some("grok"));
        assert_eq!(provider_cli_name("claude"), Some("claude"));
        // GLM authenticates with an API key - no CLI.
        assert_eq!(provider_cli_name("glm"), None);
        assert_eq!(provider_cli_name("nope"), None);
    }

    #[test]
    fn provider_auth_status_covers_all_providers() {
        // Never panics and always yields a hint when not satisfied.
        for provider in PROVIDERS {
            let status = provider_auth_status(provider);
            if !status.ok {
                assert!(!status.hint.is_empty(), "{provider} missing hint");
            }
        }
    }

    /// `PROVIDERS` is a const the frontends iterate, and the table is what
    /// everything else reads. Two lists of the same thing is exactly the shape
    /// this file exists to remove, so if they cannot be one thing they at least
    /// have to agree.
    #[test]
    fn the_id_list_and_the_table_are_the_same_providers_in_the_same_order() {
        let from_table: Vec<&str> = PROVIDER_META.iter().map(|meta| meta.id).collect();
        assert_eq!(from_table, PROVIDERS);
    }

    /// The failure this table replaces: a provider added to some of the nine
    /// matches and not the others fetched fine and then rendered under its bare
    /// id, or had no dashboard to middle-click to.
    #[test]
    fn every_provider_is_complete() {
        for meta in PROVIDER_META {
            assert!(!meta.id.is_empty(), "a row with no id");
            assert_ne!(
                meta.label, meta.id,
                "{}: the label is the id, so nothing set it",
                meta.id
            );
            assert!(
                meta.urls.dashboard.is_some(),
                "{}: no dashboard, so middle-click does nothing",
                meta.id
            );
            assert!(
                meta.icon_slug.is_some(),
                "{}: no brand mark, so every frontend falls back to the glyph",
                meta.id
            );
            assert!(
                !meta.glyph.is_empty() && meta.color_hex.starts_with('#'),
                "{}: incomplete icon",
                meta.id
            );
            let (session, weekly, tertiary) = meta.windows;
            assert!(
                !session.is_empty() && !weekly.is_empty() && !tertiary.is_empty(),
                "{}: a window with no label",
                meta.id
            );
        }
    }

    /// A row's `enabled_in` has to reach *its own* field. A copy-pasted row
    /// pointing at the one above it would silently tie two providers together.
    #[test]
    fn each_row_toggles_its_own_config_field() {
        for meta in PROVIDER_META {
            let mut config = ProvidersConfig::default();
            match meta.id {
                "codex" => config.codex = Some(true),
                "claude" => config.claude = Some(true),
                "kimi" => config.kimi = Some(true),
                "grok" => config.grok = Some(true),
                "glm" => config.glm = Some(true),
                other => panic!("{other} has no field in this test - add it with the row"),
            }
            assert_eq!(
                config.enabled_providers(),
                vec![meta.id],
                "{} switched on something else",
                meta.id
            );
            assert!(config.is_enabled(meta.id));
        }
    }

    #[test]
    fn an_unknown_provider_degrades_rather_than_disappearing() {
        // A snapshot from a newer build still renders: its rows keep their own
        // name, get the generic window headings and the fallback glyph.
        assert_eq!(provider_label("quasar"), "quasar");
        assert_eq!(window_labels("quasar"), ("Session", "Weekly", "Tertiary"));
        assert!(provider_urls("quasar").dashboard.is_none());
        assert!(provider_icon_slug("quasar").is_none());
        // But it is never switched on, and never fetched.
        assert!(!ProvidersConfig::default().is_enabled("quasar"));
        assert!(fetch_single_provider("quasar", Duration::from_secs(1)).is_err());
        assert!(!provider_auth_status("quasar").ok);
    }

    #[test]
    fn ids_resolve_whatever_case_they_arrive_in() {
        assert_eq!(provider_label("CLAUDE"), "Claude");
        assert_eq!(provider_label("Glm"), "GLM");
        assert_eq!(provider_cli_name("Codex"), Some("codex"));
    }

    /// Fleet sync buckets per-call events, and only a transcript reader
    /// produces those. A provider marked native with no reader behind it would
    /// publish nothing and read as a machine that had gone quiet.
    #[test]
    fn only_the_transcript_backed_providers_claim_to_be_native() {
        assert_eq!(natively_read(), vec!["codex", "claude", "kimi", "grok"]);
    }
}
