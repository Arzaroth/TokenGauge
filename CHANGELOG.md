# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Fleet sync: one panel covering every machine you code on.** Enable `[sync]` and the cost, token and burn figures cover the whole fleet instead of one machine, with a new "Tokens by device" section showing where the spend came from. Every frontend gets it at once, because the merged figures land where the local ones did and the new section reuses an existing row kind.
- Sync moves one encrypted object per machine through storage you already have: a folder your sync tool handles (Syncthing, Dropbox, Nextcloud, a NAS) today, S3-compatible buckets next. There is no service to run and no account to make. `--sync-init` creates the fleet key and prints it; `--sync-join <key>` adds the next machine.
- What crosses the wire is token counts bucketed by UTC hour, provider and model - never dollars, so a machine with a stale price table cannot skew the fleet total, and never prompts, paths or credentials. Objects are sealed with XChaCha20-Poly1305 and named with a keyed digest, so whoever holds the folder cannot read your usage or even count your machines.
- The COST section grows a "Sync" row that leads with problems: a transport that is down, an object it could not use, or a fleet that has gone stale. Configured-but-not-working under-reports silently, and a total that is quietly too low is worse than one that is visibly missing.
- `[sync.providers]` turns sync off per provider. Use it if you sync `~/.claude/projects` between machines yourself - both machines would otherwise read the same transcripts and double the total. TokenGauge detects that case and says so, but the fix is to leave that provider out.
- Providers whose costs come from ccusage (a Kimi or Grok plan driven from its own CLI) cannot sync yet: there are no per-call events behind them to bucket.
- **A sync screen in the TUI**, reached with `S` or from anywhere with `tokengauge --sync-setup`, which opens it in a terminal for you. Turn sync on, name this machine, point it at a folder, generate or paste a fleet key, and run a round-trip test, all in one place. There is no per-desktop settings pane by design: this screen handles a fleet key, and five implementations of a secret input is five chances to leak one.
- **An S3-compatible bucket as the alternative to a folder**: S3, Cloudflare R2, Backblaze B2's S3 endpoint, MinIO and Garage. Credentials come from `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, never written into the config by the setup screen. Requests are SigV4-signed directly over the HTTP client TokenGauge already links, rather than pulling an AWS SDK and an async runtime in to sign three verbs.
- Every frontend can reach the setup screen: a button in the Plasma settings pane, one in the GNOME popup header, `y` in the Omarchy widget's settings, and a Windows tray menu item.
- `--sync-status` prints what the last cycle did, with `--json` for the raw object. `--sync-test` writes a probe, reads it back and removes it, so you can check the transport and the key before trusting the figures. `--sync-forget <device>` drops a machine you no longer use.

## [0.23.0] - 2026-08-25

### Changed

- **The binary is now `tokengauge`.** It was called `tokengauge-waybar` because that is what it started as; it has since become the shared backend the Plasma applet, the GNOME extension, the Omarchy widget and the tray all shell out to, and it owns the daemon, the snapshot, `--doctor`, `--update` and `--install-frontend`. `--version` and the usage text name it correctly too.
- **`tokengauge-waybar` keeps working.** The installers put a symlink beside the real binary, `--update` refreshes that symlink, and release archives carry a real copy under the old name as well - the updater performing your upgrade is the *old* binary, and it only knows to look for the old name. An existing waybar config, systemd unit or frontend setting needs no edit.
- Frontend binary settings are relabelled "TokenGauge binary" but still default to `tokengauge-waybar`, which is the only name guaranteed present after an upgrade driven by a 0.22.x updater. The defaults flip a release later, once the duplicate copy in the archive can go.
- `cargo test` in CI now runs with `--all-features`, so the self-update module's tests run rather than being silently skipped.

## [0.22.0] - 2026-08-24

### Added

- **Costs are read natively, from the transcripts the CLIs already write.** `ccusage` is no longer required: TokenGauge parses `~/.claude/projects` and `~/.codex/sessions` itself and rates the tokens against LiteLLM's price table, cached beside the snapshot with a vendored copy compiled in so a cold or offline machine still shows a figure. Node and Bun stop being dependencies of the cost row. Checked against ccusage over eight months of real transcripts: 11,103,712,193 Claude tokens and 146,483,834 Codex tokens, identical on both sides, and the month-to-date money agrees to the cent.
- **The burn rate is anchored to the provider's own session window.** ccusage has to infer a five-hour block by flooring the hour of the first activity, because from outside that is the best available. TokenGauge already knows when the window resets and how long it runs - the gauge directly above that row is drawn from it - so session spend, $/hr and the projection are measured against the real window. On a live session the inferred block was 40 minutes out and counted less than half the session's spend.
- `cost_source` config key: `auto`, `native`, or `ccusage`. Defaults to `auto`, which reads natively and asks ccusage only about **enabled providers the native readers found nothing for** - ccusage reads 22 agent formats where TokenGauge parses two transcript trees, so a Kimi or Grok plan driven from its own CLI keeps its cost row. A Claude/Codex machine never spawns the subprocess.
- **The ccusage cross-check now runs in CI**, with no Node and no network. `crates/tokengauge-core/tests/fixtures/cost-home` is a fake home directory of real transcripts reduced to the fields that are billed - prompts, output, file paths, branch and project names never reach it, identifiers are deterministic stand-ins and timestamps are shifted to a fixed epoch - and `cost-golden.json` records what ccusage read from that same tree. `scripts/make-cost-fixture.py` regenerates both, and refuses to write a fixture that has stopped covering the traps.
- `--doctor` gained a **Cost source** section: how many events were read and how fast, which transcript roots were found, how many models the price table covers, any model with tokens and no price, and - when ccusage is installed - whether the two agree on token counts. An unpriced model is reported as a gap rather than shown as $0 spent.
- **Cost and token detail for Kimi, Grok and GLM.** Those three providers have had native usage limits for releases now, and no cost row at all: spend was attributed by guessing at the model name, and a name that did not start with `claude` or `gpt` was dropped on the floor. TokenGauge now reads ccusage's per-agent split (`--by-agent`) and falls back to the agent when a model name says nothing. A GLM or Kimi model driven *through* Claude Code is attributed to GLM or Kimi rather than to Claude, because the model is the finer signal of whose money it was.
- **Codex signs in with a personal access token.** A `personal_access_token` in `auth.json` - what `codex login` writes for managed and workspace accounts - was read as "not logged in". It now authenticates the usage call, with the account it belongs to resolved from OpenAI's whoami endpoint so a workspace member sees their own numbers, and the plan name it reports fills the header when the usage response omits one. OAuth still wins when both are present; nothing changes for an ordinary `codex` login.
- Codex team, EDU and enterprise workspaces show the administrator-defined pool. Those accounts report no rate windows at all and put the monthly cap under `spend_control`, which nothing read, so the provider rendered with no gauge.

### Changed

- **A Codex 30-day window is labelled Monthly**, on every panel. Free plans report a single 30-day window, and it landed in the slot the panels label "Session": a month of headroom read as a day's, resetting four weeks out. It now gets its own row and leaves the session gauge empty rather than lying about it.
- Kimi's extra rate windows are named for how long they run - `5-hour limit`, `1-minute limit` - instead of `300-minute window`. A rolling limit that merely repeats the weekly quota (same percentage, same reset) is dropped, so the shorter window behind it takes the slot it was hiding in.
- **`ccusage_enabled` is now the master switch for cost figures**, not just for the ccusage subprocess. Off still means no cost rows at all; `cost_source` picks the mechanism. A missing ccusage runner is no longer a `--doctor` failure unless `cost_source = "ccusage"`.
- **A cost refresh is ~7x faster** (measured 3.9-4.4s -> 0.6s for a full fetch). Two things paid for that. Each refresh ran `ccusage daily` three times - today, month-to-date, rolling week - and every one of those re-read every transcript on disk to answer a narrower question than the last; one call now covers the widest range and the three figures are sliced out of it. And every ccusage invocation re-fetched the LiteLLM pricing table over the network, about 700ms a call, so all of them now pass `--offline`, which produces figures identical to the cent from the pricing data ccusage already carries.
- A ccusage too old for `--by-agent` or `--offline` exits non-zero rather than ignoring the flag, so each call retries in its bare form before giving up. An older install keeps working, with Claude and Codex costs as before.

### Fixed

- **A GLM credit plan reads its real usage.** The credit-based Coding Plans (lite, standard, pro) meter in `CREDIT_LIMIT` entries where the token plans meter in `TOKENS_LIMIT`, and only the token shape was understood: the credit windows were mistaken for the 30-day time limit, so those plans showed 0% used no matter how much had been spent.

## [0.21.1] - 2026-08-24

### Fixed

- `--doctor` no longer fails a machine for not running Waybar. The check dated from when Waybar was the only surface: it looked for `~/.config/waybar/config.jsonc`, counted its absence as a fault, and then reported the installed Plasma / GNOME / Omarchy frontend as healthy two sections further down. The section is now **Bar wiring** and reads the installed frontends first - it says which one draws the gauge, and only fails when nothing is wired up at all.
- A stale key inside `[waybar]` is now reported instead of dropped in silence. Unknown keys were collected at the top level and under `[providers]` only, so the `popover_command` left behind by 0.20.0's removal of the popover sat in the file with nothing to say it did nothing.

## [0.21.0] - 2026-08-24

### Added

- Frontends now **watch** for new data instead of only polling for it. A fetch by the daemon, the TUI or another frontend reaches the Omarchy widget, the Plasma applet and the GNOME extension at once rather than up to a full refresh interval later. Each surface watches a few-byte revision file the binary rewrites after every snapshot, so nothing but `--json` ever reads the snapshot itself. The periodic poll stays as the fallback that ages the cache out when no daemon is running.
- `--wait-change` blocks until the snapshot is rewritten (or `--wait-timeout` seconds pass) and exits 0. It is how the Plasma applet watches, since QML in a plasmoid has no file watcher; it is also usable from a script.
- The snapshot records which machine wrote it - `machineId`, `hostname`, a write timestamp and the provider set it was fetched with - so snapshots collected from several machines can be told apart and reconciled later. Nothing merges them yet.

### Changed

- **The snapshot moved out of the temp directory** to `$XDG_STATE_HOME/tokengauge/` (`%LOCALAPPDATA%\TokenGauge\` on Windows), and the state files beside it - selected provider, notify state, refresh sentinel, daemon socket - follow. It holds the only record of past days' tokens and costs, which a reboot wiping `/tmp` threw away. Existing files are moved on first run, and a config still naming the old temp path is read as never having chosen one, so upgrades keep their history without an edit. A `cache_file` pointing anywhere else is left alone.
- Snapshots are written atomically, so a reader watching the file never sees half of one.

### Fixed

- **Enabling a provider now shows it immediately.** The cache was only ever validated by age, so a provider switched on stayed invisible until the cache expired - up to `refresh_secs`, ten minutes by default, and forever on a machine with no daemon running to notice the config change. The snapshot now records the provider set it was fetched with, and any reader that finds a provider missing from it refetches. `--set-provider` fetches before it returns, so the `--json` a settings pane chains behind it already carries the new provider's row and the chip that selects it. Switching a provider *off* still costs nothing: the snapshot is a superset, so it is re-rendered rather than refetched.
- Toggling a provider no longer fetches twice. The daemon's reload asks whether the snapshot answers for the new config instead of comparing the before/after provider sets, so it re-renders after `--set-provider` has already fetched.

## [0.20.0] - 2026-08-23

### Added

- Extra rate windows - Claude's model-scoped weeklies (`Fable only`, `Sonnet only`) and `Daily Routines` - now carry a burn projection, the same `ends ~26%` / `empty in 2h 15m` badge the session and weekly gauges already showed. It renders on every frontend that draws those rows.
- **One panel, one definition.** `tokengauge-core` now resolves the whole panel - section order, labels, number formatting, sort order, colour tiers, tooltips - in `panel_spec()`, and every frontend renders that list instead of deciding its own. Rust frontends call it directly; the QML and JS ones read it off a new `panel` field on each row of `tokengauge-waybar --json`. A frontend implements three primitives (meter, bar row, key/value row) and loops, so a section added to the core appears everywhere at once. `CLAUDE.md` records the rule and a test fails the build when a frontend stops handling a section kind.
- Tokens by day and tokens by model, one bar per line, on every panel. Only the Omarchy widget had the per-model breakdown; the Plasma applet and GNOME extension drew a squashed 7-day column chart instead of per-day bars, and the waybar tooltip and Windows tray had neither.
- The waybar tooltip is now the waybar panel: it draws the same LIMITS / COST / TOKENS BY DAY / TOKENS BY MODEL sections as every other frontend, with its own text bars.
- The Windows tray window reaches parity: provider tabs, all limit meters (tertiary and extra windows included, with pace badges), the cost figures, tokens by day, tokens by model, and a settings pane for the provider toggles and the bar pin. It drew Session and Weekly and nothing else.

### Changed

- Cost figures read the same on every panel: `Today`, `Session`, `7-day`, `This month`, `Burn rate`, with the `↑161% vs prior avg` trend tinted by how far above the prior daily average today sits. The four panels previously disagreed on which rows existed and what they were called.
- A window the provider does not report is dropped rather than drawn as a permanently empty meter. The waybar tooltip was the last frontend still drawing them to hold its line count steady; now that it shares the panel spec, it follows the same rule.

### Removed

- **Breaking:** the bundled GTK4 popover (`tokengauge-popover`) is gone. The waybar tooltip now carries the full panel, so a second window that opened on click showed nothing the hover did not. A config still set to `click_action = "popover"` keeps loading and opens the TUI; the `popover_command`, `popover_margin_top` and `popover_margin_side` keys are ignored and can be deleted. Nothing else changes for `click_action = "tui"`, which was the default.

## [0.19.0] - 2026-08-22

### Added

- `--install-frontend <plasma|gnome|omarchy|all>` installs a desktop frontend from the release the running binary belongs to. Switching desktops - KDE to GNOME, Waybar to the Omarchy bar - is now one command rather than a checkout and an install script.
- `--update` now also refreshes whichever desktop frontends are already installed, and prints what it touched plus the restart each one needs. It only refreshes what is present; it never decides a machine should grow a GNOME extension.
- The release archive ships the Plasma applet, GNOME extension, and Omarchy widget under `frontends/`. Without them in the archive there was nothing for an update to install.
- `--doctor` reports each installed frontend's own version against the binary's, and prints the exact `--install-frontend` command when they disagree.

### Fixed

- The desktop frontends are QML and JavaScript installed outside `~/.local/bin`, so `--update` replaced the binaries and left them untouched: a 0.18.0 binary drove whatever QML the machine already had. That surfaced as the Plasma applet still drawing the placeholder limit rows that 0.18.0 removed - the snapshot carried the flag, the applet predated the code reading it.
- Each frontend now reports **its own** version rather than the binary's, and says so when the two differ. Showing only the binary's version - which is what 0.18.0 did - made that skew invisible: the applet would print "v0.18.0" while running older QML.

## [0.18.0] - 2026-08-21

### Added

- Every panel frontend now shows the version it is running: the popover and Omarchy settings panes, the Plasma settings pane, the TUI's help popup, and an About row in the GNOME preferences. The GUIs read the binary's version rather than their own, since a frontend and the binary it reads from are installed separately - the release tarball ships only the Waybar module and the TUI, so a popover built from source can sit at a different version. `tokengauge-waybar --json` carries it as a top-level `version`.

### Fixed

- The TUI, popover, Plasma applet, and GNOME extension no longer draw a permanently empty meter for a limit the provider exposes a slot for but reports nothing in - "Daily Routines" on an account without it, most visibly. The Omarchy widget already dropped these; the others were still rendering them from the same `placeholder` flag they were ignoring. The Waybar tooltip keeps them, which is what the flag exists for: its layout is fixed and a row appearing or vanishing shifts it.

## [0.17.0] - 2026-08-21

### Added

- The Omarchy widget regains the mouse and keyboard bindings the Waybar module has: scroll the bar icon to move through providers, middle-click for the usage dashboard, back (mouse 8) for the status page, and `u` / `s` in the panel to open those same two - spelled the way the TUI spells them. The settings pane moves from `s` to `,` rather than shadowing the status-page key.
- Each row in `tokengauge-waybar --json` carries `dashboard_url` and `status_url`. `--open` resolves the provider from the config rather than from the caller, so a frontend that keeps its own selection could not use it without opening the wrong provider.

### Changed

- The Omarchy widget's bar icon no longer carries a tooltip. It repeated the provider and plan that the panel's hero shows one click away, which is why the first-party widgets suppress theirs too. The per-row tooltips inside the panel stay - they carry the token split and exact figures, which are not on screen otherwise.

## [0.16.0] - 2026-08-21

### Added

- **Omarchy shell plugin** (`omarchy/arzaroth.tokengauge`, Omarchy 4+): a Quickshell bar widget for the Waybar replacement that ships with Omarchy 4. Bar icon with the provider glyph and headline percent, and a panel carrying the brand mark + plan hero, provider chips, usage meters with reset and pace, ccusage cost rows with the burn rate, a tokens-by-day chart, tokens by model, an update banner, and a settings pane with the provider toggles and the pin-to-bar picker. Left-click opens the panel, right-click refreshes, middle-click cycles providers; `h`/`l`, `j`/`k`, `r`, `s`, and Esc drive it from the keyboard, and `omarchy-shell arzaroth.tokengauge <open|close|toggle|refresh|next>` drives it over IPC. Reads the same `tokengauge-waybar --json` snapshot as the Plasma applet, so config, cache, and daemon are shared. Install with `scripts/install-omarchy.sh`.
- `tokengauge-waybar --json` now reports `cost.weekly_history`, the same window as `weekly_cost_history` but with each day's date and token count alongside its cost, so a frontend can chart tokens per day and label each bar from its own date.
- Per-model cost slices (`cost.today_models`, `cost.monthly_models`) now carry the `input_tokens` / `output_tokens` / `cache_creation_tokens` / `cache_read_tokens` split behind their total, for per-model hover detail.
- Extra rate windows carry a `placeholder` flag. Anthropic's usage endpoint exposes a slot for every limit kind it knows about and reports an explicit null for the ones an account does not have; those still render in the Waybar module so its shape stays fixed, but a frontend with room only for real windows can now tell them apart from an allowance the account holds and has not spent.

### Changed

- The 7-day cost history is now a fixed window of the last 7 calendar days instead of the days ccusage happened to report, and is queried on its own rather than sliced out of the month-to-date response - which could not cover a window reaching back past the 1st. ccusage omits days with no usage entirely, so an idle day used to vanish from the series rather than read as $0, shortening the chart and shifting every label in one drawn by counting backwards from today. Idle days now carry a zero entry, which also means `avg_daily_cost` (and the today-vs-average figure derived from it) divides by 7 days rather than by active days only, so the baseline drops for anyone who does not code daily.

## [0.15.0] - 2026-08-20

### Added

- **GNOME Shell extension** (`gnome/tokengauge@arzaroth.github.io`, GNOME 45+): panel indicator with the provider brand icon and usage percent, and a popup carrying provider tabs, tier-coloured usage meters with reset + pace, cost rows, a 7-day chart, pin-to-bar, and the update banner. Scroll the panel button to cycle providers, middle-click to refresh. Preferences (binary path, refresh interval, panel percent, OAuth provider toggles) are an Adwaita window. Install with `scripts/install-gnome.sh`.
- `tokengauge-waybar --json` now reports the full list of toggleable providers under `providers`, so frontends no longer hardcode it - the Plasma applet's settings pane had been stuck on Codex and Claude since Kimi, Grok, and GLM landed.

## [0.14.0] - 2026-08-19

### Changed

- **Pace tracking** now projects where a window lands at reset instead of reporting a signed delta against an even-consumption rate. The `+8%` / `-3%` badge was percentage points against an expectation that never appeared on screen; it now reads `ends ~16%` while the window lasts, or `empty in 2h 15m` when the current rate runs it out first.

### Fixed

- The cost trend percentage compared the active burn rate against a weekly average smeared over all 24 hours of each day, idle ones included, which made it read absurdly high. It now compares today's spend against the average of the previous days - excluding today's own partial entry from that baseline - and sits on the `Today` row where both sides share a unit.

## [0.13.0] - 2026-07-18

### Added

- Native Kimi provider (kimi.com/code): reads the `kimi` CLI token (`~/.kimi-code/credentials/kimi-code.json`, `KIMI_CODE_HOME` override) or `KIMI_CODE_API_KEY`, and fetches the Code API usage (weekly quota + rate-limit window). Enable with `kimi = true`.
- Native Grok provider (x.ai build): reads the `grok login` token (`~/.grok/auth.json`, `GROK_HOME` override) and fetches the grok.com build-billing usage over gRPC-web. Enable with `grok = true`.
- Native GLM provider (z.ai / zcode.z.ai): reads `Z_AI_API_KEY` (legacy `ZAI_API_TOKEN`) and fetches the GLM Coding Plan quota. Set `Z_AI_API_HOST` for the China BigModel region, or use `Z_AI_QUOTA_URL` to override the full quota endpoint. Enable with `glm = true`.
- Brand SVG logos for Kimi, Grok, and GLM in the popover / Plasma tab strips.
- `--doctor` now covers every supported provider: per-enabled-provider credential status (file or env key, including GLM's `Z_AI_API_KEY`), a sign-in CLI-on-PATH check when credentials are missing, a list of available-but-disabled providers, and a labeled live-fetch result per enabled provider.

## [0.12.0] - 2026-07-18

### Added

- **Pace tracking**: the session and weekly windows compute whether you're burning quota faster (deficit) or slower (reserve) than an even-consumption rate, plus a projected run-out. Shown as a `+8%` / `-3%` badge in the Waybar tooltip, TUI, popover, and Plasma applet (hidden until 3% of the window has elapsed).

## [0.11.1] - 2026-07-18

### Fixed

- Threshold notifications no longer spam when a usage window resets. Roll-over is now detected from the window's `resets_at` timestamp advancing, not a fragile "percent dropped 10 points" heuristic - which mis-fired when a freshly-reset window briefly reported a stale-high percent, or when the value wobbled near the top and cleared + re-fired the one-shot guard on every poll. An already-notified threshold fires again only after the window genuinely rolls over.

## [0.11.0] - 2026-07-16

Usage limits are now fetched natively - no external CodexBar CLI.

### Changed

- Usage limits are now fetched natively over HTTP for Claude and Codex; the external `codexbar` CLI is no longer required.
- Windows fetches Claude/Codex usage natively - Win-CodexBar is no longer needed.
- Codex refreshes its own OAuth token behind a cross-process lock and writes it back atomically (0600, unknown fields preserved), keeping a recovery copy if the final replace fails.

### Removed

- **BREAKING**: Dropped the zai, kimik2, copilot, minimax, and kimi providers. TokenGauge now supports only Claude and Codex.
- **BREAKING**: Removed the `codexbar_bin` config key (old configs still load; `--doctor` and the daemon log warn about it and any leftover `[providers.*]` keys).

### Fixed

- Expired/failed fetches keep serving the last-good cached number instead of a blank row (empty or malformed provider data is no longer treated as live usage).
- Guarded a u8 overflow in threshold notifications and a UTF-8 panic when truncating long error messages.

### Notes

- `socks5://` proxies now require building with reqwest's `socks` feature; `HTTP_PROXY`/`HTTPS_PROXY` still work.
- Minimum supported Rust version is now 1.89 (the native Codex refresh lock uses `File::try_lock`).

## [0.10.1] - 2026-07-16

### Fixed

- `--update` downloads the release binary again instead of failing with `extract failed: invalid gzip header`. The download hit GitHub's asset API URL without an `Accept: application/octet-stream` header, so GitHub returned the asset's ~1.6 KiB JSON metadata rather than the tarball, and extraction choked on the non-gzip bytes.

## [0.10.0] - 2026-07-16

Provider toggles apply immediately across all frontends.

### Added

- The popover and TUI fetch fresh data when opened; the popover shows the current cache immediately while the refresh runs, and the TUI blocks on the fetch behind its spinner.
- Refresh indicator in the popover: a ⟳ marker shows in the header for the duration of a fetch, and the view re-renders when the data lands.

### Fixed

- Disabling a provider now takes effect immediately. The daemon refetches when a config reload changes the enabled provider set (previously it re-rendered from cache, so a disabled provider kept showing, and a newly enabled one stayed missing, until the next refresh tick - up to `refresh_secs`).
- The popover's "updated" stamp reports when the cache was last written, taken from the cache's mtime, and shows the date when the write wasn't today. Note a stale-fallback round also writes the cache, so the stamp tracks the last write, not necessarily a successful fetch. It rendered the current time, so it always claimed the data was fresh even when the fetch behind it was hours old or failing.
- `scripts/install.sh` reports that `tokengauge-popover` isn't in the release tarball (it needs GTK4 at build time) instead of skipping it in silence, and points at the source build. An upgrade previously left a stale popover next to freshly-updated binaries with no hint anything had been left behind.
- Every read of the cache is scoped to the enabled providers, so a disabled provider can't surface from a cache written before the toggle. This covers the no-daemon case, where nothing signals a reload; the bar's scroll rotation is scoped too, so scrolling no longer stops on a disabled provider.
- The popover's Refresh button works without a running daemon. It shelled straight to the daemon socket and silently did nothing when there was none; it now goes through `--refresh`, which falls back to a detached worker.

## [0.9.1] - 2026-07-16

### Fixed

- The daemon resolves `codexbar` again. Its systemd unit inherited a PATH without `~/.local/bin` - where the installer puts the binary - so every fetch failed to spawn it, and the stale fallback silently served frozen usage indefinitely. The unit now sets `Environment=PATH` to include the install dir.
- Stale fallback rounds are visible in the fetch log (`stale=N`) instead of reporting `errors=0` and reading like a clean fetch.

## [0.9.0] - 2026-07-15

Windows support and self-updating binaries.

### Added

- Native Windows 10+ support for `tokengauge-tui`, installed via the new `scripts/install.ps1` PowerShell installer.
- `tokengauge-tray`, a Windows system-tray GUI. Renders current session usage as the tray icon number/colour, click-to-open surfaces the full window, and it builds as a windowless GUI app so no console window flashes on launch.
- Self-update from GitHub releases. `--check-update` performs a live GitHub check, caches the result and prints JSON status without installing anything; `--update` downloads the latest release and swaps the installed binaries in place. The Plasma applet and tray GUI drive the same path from their "Update" buttons, and a one-shot desktop notification fires when an update is available.
- Win-CodexBar usable as a codexbar drop-in on Windows, so the Codex provider works there without a separate shim.

### Fixed

- All cached payloads are restored for a failed provider, not just the first - a provider erroring no longer drops the rest of its cached data.
- The Plasma update button resets on completion or failure instead of sticking in its in-progress state, and the update-flag reset is scoped so one applet's update doesn't clear another's.
- The Plasma applet matches the exact update source, so an update offered for one install target can't be applied against another.
- Update stderr is preserved rather than swallowed, so a failed update reports why.
- snake_case and float-percent codexbar JSON parse correctly - Win-CodexBar's output shape no longer trips the core parser.

### Changed

- Linux CI builds exclude `tokengauge-popover` instead of apt-installing GTK, and the tree is clean under current-stable rustfmt/clippy.

## [0.8.0] - 2026-07-14

Native KDE Plasma 6 applet - run TokenGauge as a panel widget instead of (or alongside) the Waybar module.

### Added

- Native KDE Plasma 6 applet (`org.tokengauge.plasmoid`): compact and full representations, provider/pin settings pane, installed via `scripts/install-plasma.sh`.
- Per-window limits on panel hover - the compact representation surfaces session/weekly limits without opening the full view.
- Waybar JSON bridge for non-waybar frontends. `tokengauge-waybar --json` emits the full snapshot as one enriched JSON object (label, brand SVG path, glyph, colour), and `--set-provider` / `--set-primary` let the applet edit config and signal the daemon.

### Fixed

- The plasmoid refetches stale data instead of serving the cache forever. `--json` now goes through `maybe_refresh` (serve-if-fresh / refetch-if-stale), so a standalone applet with no daemon keeping the cache warm still updates on its 60s timer.
- Config edits actually reach the running daemon. `--set-provider` / `--set-primary` now signal via `pkill -HUP -f 'tokengauge-waybar --daemon'` (the plain-name `pkill` matched nothing - 17-char name vs procps' 15-char `comm` cap). The reload helper is lifted into core so the popover and applet share one fix.
- A failed toggle surfaces its error - the applet chains the action flag and `--json` with `&&`, so a failed `--set-provider` reports stderr instead of being masked by the `--json` exit code.
- `install-plasma.sh` fails clearly on a missing asset dir (nullglob guard) instead of `set -e` aborting on an unexpanded glob.

## [0.7.0] - 2026-07-14

Waybar / codexbar parity release - the popover and core catch up to the Waybar module, plus resilience when providers misbehave.

### Added

- Claude CLI source fallback on OAuth error. When a provider's OAuth fetch fails, core falls back to the Claude CLI source instead of surfacing a blank error.
- Stale last-good cache on fetch failure. A transient `429` or network blip serves the last-good cached usage marked `stale` instead of a blank bar.
- Staggered provider fetches (`stagger_ms`). Config knob spreading codexbar calls out by `index * stagger_ms` for rate-limit relief; `0` disables (all at once).
- Real provider brand SVG logos in the popover, with the glyph as fallback.
- Inline settings pane in the popover: toggle OAuth providers and pick the bar-pinned provider live, comments preserved, daemon reloaded on change.

### Fixed

- Inline provider tables are no longer wiped on toggle. `providers = { codex = true }` configs keep their keys instead of being overwritten with an empty table.
- No duplicate stale rows when a provider returns mixed success/error sub-payloads or multiple error entries.
- The daemon reload signal now actually reaches the daemon. The 17-char binary name exceeds procps' 15-char `comm` cap, so `pkill tokengauge-waybar` matched nothing - the live provider/pin toggles never reloaded. Now `pkill -HUP -f 'tokengauge-waybar --daemon'`, and the child is reaped instead of leaking a zombie.
- Settings pane reflects edits immediately - a disabled provider drops from the bar-pin list without restarting the popover.
- Stagger sleep uses `saturating_mul` to remove a theoretical overflow panic.

## [0.6.3] - 2026-06-29

Patch release - waybar click + daemon environment fixes.

### Fixed

- Middle/back waybar clicks opening no browser tab. `--open` was routed through the daemon socket, and the daemon (a systemd `--user` service started at boot) runs with a stripped environment, so the browser it spawned could not reach the running instance. `--open` now runs in the waybar-invoked process, which has the full graphical session env.
- Daemon notifications being silently dropped. The daemon unit was `WantedBy=default.target`, so it started before the compositor imported `WAYLAND_DISPLAY` / `DBUS_SESSION_BUS_ADDRESS` / `BROWSER` into the systemd `--user` env. It is now bound to `graphical-session.target` (with `PartOf=`), and `install.sh` reenables the unit so upgrades drop the stale early-start symlink.

## [0.6.2] - 2026-05-27

### Fixed

- Provider fetch errors now surface the full anyhow cause chain. `failed to run codexbar for codex` previously hid the actual reason; the cache and tooltip now show e.g. `failed to run codexbar for codex: timeout after 10s`.

### Changed

- Default `timeout_secs` bumped from 10 to 20. Codexbar's typical fetch is 9-10s, so the old default raced the deadline and intermittently failed. Override in config if you need it tighter.

## [0.6.1] - 2026-05-27

### Fixed

- Waybar text stacked all providers on first boot (no scroll state, no `waybar.primary` set). It now defaults to the first provider; scrolling rotates as before. Tooltip and popover unchanged - both still surface every provider via their own tab UI.

## [0.6.0] - 2026-05-26

Native GTK4 popover + click action.

### Added

- Config-driven left-click action (`[waybar].click_action = "tui" | "popover"`). Waybar's `on-click` uniformly calls `tokengauge-waybar --click`; the binary dispatches based on config. `--doctor` reports the resolved launcher and warns when it isn't on `$PATH`.
- Bundled native GTK4 popover (`tokengauge-popover`) for `click_action = "popover"`: `gtk4-layer-shell` window anchored under waybar (margins configurable via `popover_margin_top` / `popover_margin_side`), codexbar-style provider tabs, a card per provider with proportional usage bars, monospace-aligned cost rows, a collapsible 7-day chart, `--toggle` second-click close (PID-file based), and an initial active tab that respects waybar's scroll selection.
- `scripts/eww-popup/`, a starter eww window for users who'd rather drive their own widget toolkit; set `popover_command = "eww open --toggle tokengauge-popup"`.
- Daemon SIGHUP reloads config + theme without restarting the systemd unit; socket protocol covered by 6 new tests.

### Changed

- TUI UX redesign: module split (`app.rs`, `ui.rs`, `refresh.rs`, `theme.rs`), `ratatui::init()` lifecycle, sidebar provider list + per-card layout, BarChart 7-day cost, popup help (`?`).
- Tooltip left-click hint reflects the configured action (`open TUI` vs `open panel`).

## [0.5.0] - 2026-05-26

Major feature batch on top of upstream v0.4.2.

### Added

- Daemon mode (`tokengauge-waybar --daemon`): long-lived Unix-socket service that owns fetch + cache writes. `install.sh` enables a systemd `--user` unit when available. Waybar polls become near-instant snapshots.
- Cost tracking via ccusage: today / month / 7-day / per-model breakdown / current burn rate \$/hr / 7-day sparkline / trend vs 7d average. Cost section mirrors Session / Weekly usage windows.
- Threshold notifications: `notify-send` alerts on configurable percentages (default 50/80/95). One-shot per threshold with reset on window roll-over.
- `--doctor`: diagnostic checklist for codexbar, ccusage runner, notify-send, xdg-open, providers (live fetch), waybar wiring.
- CSS tier classes: waybar class flips to `tokengauge-warn` (>=50%) / `tokengauge-crit` (>=80%) / `tokengauge-error` for theming.
- Mouse + key bindings: middle-click dashboard, back-button status, right-click refresh (with `⟳ Refreshing...` indicator), scroll rotate provider with debounce. TUI gains `h/l/Tab` tabs, `u` dashboard, `s` status.
- Config knobs: `waybar.primary`, `waybar.scroll_throttle_ms`, `ccusage_enabled`, `ccusage_timeout_secs`, `notifications.enabled`, `notifications.thresholds`.

### Changed

- Provider tabs in TUI with brand-coloured icons (Anthropic / OpenAI / GitHub Copilot / Z.ai).
- CodexBar-style hover popup with Session, Weekly (all), Weekly (Sonnet), Extra usage rows.
- Reset time renders with a days bucket when > 24h.
- Hover hint footer documents all mouse actions.

## [0.4.2] and earlier

Released by the upstream project, [oorestisime/TokenGauge](https://github.com/oorestisime/TokenGauge/releases). This fork's own history starts at 0.5.0.

[Unreleased]: https://github.com/Arzaroth/TokenGauge/compare/v0.11.0...HEAD
[0.11.0]: https://github.com/Arzaroth/TokenGauge/compare/v0.10.1...v0.11.0
[0.10.1]: https://github.com/Arzaroth/TokenGauge/compare/v0.10.0...v0.10.1
[0.10.0]: https://github.com/Arzaroth/TokenGauge/compare/v0.9.1...v0.10.0
[0.9.1]: https://github.com/Arzaroth/TokenGauge/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/Arzaroth/TokenGauge/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/Arzaroth/TokenGauge/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/Arzaroth/TokenGauge/compare/v0.6.3...v0.7.0
[0.6.3]: https://github.com/Arzaroth/TokenGauge/compare/v0.6.2...v0.6.3
[0.6.2]: https://github.com/Arzaroth/TokenGauge/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/Arzaroth/TokenGauge/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/Arzaroth/TokenGauge/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/Arzaroth/TokenGauge/compare/v0.4.2...v0.5.0
