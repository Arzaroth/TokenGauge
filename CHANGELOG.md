# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.29.3] - 2026-09-02

### Security

- **Panel text is drawn as text, not as markup.** A QML `Text` with no
  `textFormat` is `Text.AutoText`: Qt inspects the string, promotes anything
  that looks like markup to rich text, and rich text fetches
  `<img src="http://...">` while it lays the label out. Every string the panel
  draws comes from outside - a window title and a model name off a provider's
  API, a model id out of a transcript, the text of a fetch error - so one `<`
  in any of them was enough to make the desktop issue an unauthenticated
  request with nothing clicked. The Omarchy widget and the Plasma applet now
  declare `textFormat: Text.PlainText` on every label they draw. The Waybar
  tooltip already escaped its Pango markup and the GNOME extension renders none,
  so neither was affected.

### Fixed

- **A one-shot `codex exec` run is counted.** A headless Codex run writes no
  rollout envelope: the usage rides on the line itself, one row per call under
  whichever field names the serving API used. The reader only understood the
  interactive shape, so every `codex exec` session contributed nothing to the
  cost panel.
- **Codex cache reads are priced as cache reads whichever field carries them.**
  Some Codex builds report the cached subset of the input as
  `cache_read_input_tokens` rather than `cached_input_tokens`. The reader knew
  only the second name, so on those builds the whole prompt was billed at the
  fresh-input rate - the token counts stayed right, which is why nothing caught
  it, and the cost ran high by roughly the cache hit rate.
- **A Codex reading written out of order no longer bills its span twice.** The
  session's running total is the baseline for the next delta, and a snapshot
  restating an earlier state was allowed to become that baseline. Nothing was
  billed for the stale row itself, but the reading after it then charged the
  whole span again from the lowered mark.
- **A Codex session that is still valid is no longer refreshed on age alone.**
  `auth.json` carries no expiry, so the token was refreshed once its
  `last_refresh` passed eight days regardless of whether it had expired. The
  access token's own `exp` claim now decides, with the age rule kept as the
  fallback for a token that carries no claim. A refresh spends a rotating
  refresh token, and a rejected one reported a working credential as
  "run `codex`".

## [0.29.2] - 2026-09-01

### Fixed

- **A model the price table has never heard of no longer reads as $0 for a
  day.** The table is served for 24h without asking, so a model released after
  the last download was counted, rated at nothing, and indistinguishable in the
  panel from a day that cost nothing - Claude Fable 5.1 landed mid-window and
  every machine showed its tokens beside $0.00. Tokens with no price are now
  proof the table is behind, the way a window that has reset is proof the
  snapshot is, and they buy one download outside the freshness window. Asked
  once per set, so a model upstream will never carry - a local model, or a
  provider sold under a namespace `vendor_prefixes` misses - cannot turn every
  fetch into a re-download; a download that failed records nothing, so an
  offline machine retries rather than burning its one ask.
- **The vendored price table caught up with upstream.** Claude Fable 5.1, Kimi
  K2.7 Code, the Grok 4.20 family and GLM 5.2 / 5.3 Flash are priced on a cold
  or offline machine now, and 21 Grok prices that had moved were refreshed.

## [0.29.1] - 2026-09-01

### Fixed

- **A frontend's `--json` no longer fetches on its own, so sync credentials
  stop vanishing.** `--json` did its own `maybe_refresh`, so whichever desktop
  frontend polled first after the snapshot went stale ran the fetch as a child
  of the compositor. That child has the compositor's environment, not the
  daemon's, and credentials arriving through `environment.d` reach the systemd
  unit and nothing else - the fetch failed to sign an S3 request and wrote
  `no S3 access key` into the snapshot every other frontend reads, where it sat
  until the daemon's next fetch cleared it. `--json` now asks the daemon, as
  the waybar path already did, and falls back to fetching in-process only when
  there is no daemon to ask.
- **The daemon notices a window rollover between fetches.** It slept
  `refresh_secs` in one go, so a snapshot invalidated mid-cycle by a window
  reset - an instant no timer lines up with - stayed stale until the next tick.
  The wait now re-checks `cache_is_stale` every 15s, and wakes early only after
  a fetch that actually wrote, so a fetch that cannot write does not spin.

## [0.29.0] - 2026-09-01

### Added

- **A per-user MSI for Windows.** There was no way to uninstall TokenGauge:
  `install.ps1` copied binaries and appended to `PATH`, and nothing took either
  back. `tokengauge-<version>-win64.msi` is now a release asset, a
  per-user install needing no administrator rights, that puts the binaries where
  the script and the self-updater already expect them, manages the `PATH` entry
  through MSI so uninstall removes it, registers a Start Menu entry, and offers
  run-at-login as an opt-in feature (`ADDLOCAL=Main,RunAtLogin`). It is built
  and validated on the Windows runner in both CI and the release workflow. The
  zip and `install.ps1` stay the recommended path: the MSI is unsigned, so it
  trips SmartScreen where the script does not.
- **`--update` upgrades an MSI install through MSI.** Replacing the binaries
  underneath the installer would leave Windows describing a version that is not
  on disk, a repair restoring the old one, and a later MSI comparing against a
  version nobody is running. When the marker key the MSI writes is present,
  `--update` now downloads the `.msi` and hands the upgrade to `msiexec`
  instead. It returns while the installer runs, because the package replaces
  the executable running the update, and the tray quits when it starts one for
  the same reason. A zip or `install.ps1` install has no marker and keeps the
  in-place path unchanged.

- **A panel says why its figures are frozen.** A provider whose fetch fails
  serves its last good payload with a `stale` badge, and the error that caused
  it was dropped on the floor - so the badge was the whole explanation, whether
  the network blipped once or a credential expired weeks ago. The failure now
  rides on the payload and `panel.rs` turns it into a STATUS section above the
  limits: how old the figures are, and what the last fetch said. It comes off
  the panel spec, so all six frontends carry it.

### Fixed

- **The updater picks its download by name, not by luck.** `asset_for` matches
  a release asset by substring, and with only one asset per platform passing no
  filter was unambiguous. Adding a second Windows asset made it a coin toss
  decided by whatever order GitHub returns, and losing it means handing an MSI
  to the zip extractor. New builds ask for the archive explicitly, and the MSI
  is published as `win64` rather than `windows-x86_64` so the updaters already
  in the wild cannot match it at all.
- **The tray GUI is a tray panel, not a window.** It was an ordinary decorated,
  resizable window that opened wherever Windows put it. It is now a flyout:
  undecorated, no taskbar button, always on top, anchored to the tray icon it
  was opened from, and dismissed by clicking away, pressing `Esc` or the new `×`
  button. The limit bars stretch to the panel width instead of stopping at a
  fixed 220pt, which left the LIMITS section ending halfway across.
- **The tray GUI was drawing light-theme text on its dark panel.** `set_visuals`
  applies to the current egui theme only, so on a machine running Windows in
  light mode every string without an explicit colour - the provider name, every
  cost figure - was painted in the light theme's near-black over the panel's
  hardcoded dark fills. The palette is now installed on both themes.
- **`↑` and `↓` rendered as tofu boxes in the tray GUI.** egui bundles those
  arrows in Hack alone and puts Hack in the monospace family only, so the cost
  trend badge boxed on every proportional surface. Hack is now the proportional
  family's last fallback.
- **A limit at 100% no longer says "not started".** The panel wrote that
  footnote whenever a window had no reset time, which is right at 0% and
  self-contradictory above it. A counting window whose reset the provider does
  not report now says nothing rather than the opposite of what the bar shows.
- **`install.ps1` installs the tray GUI.** It only ever copied
  `tokengauge-tui.exe`, so the Windows GUI had to be unzipped by hand and had
  no way to be launched but its full path. It now installs both binaries, adds
  a Start Menu entry, and takes `-RunAtLogin` to start the tray with Windows.
## [0.28.1] - 2026-08-31

### Fixed

- **Panel tooltips line up again on Plasma, GNOME and the Windows tray.** The
  sub-tables a tooltip carries - a day's split by model and by device, a model's
  split by device - are padded to a monospace grid by the panel spec, but those
  three surfaces drew them in their toolkit's proportional font, so the token
  and cost columns landed wherever the labels happened to end. All three now
  render tooltip text in a monospace face, as the waybar tooltip already did.

## [0.28.0] - 2026-08-31

### Added

- **Claude credentials are read from three sources, not one.** The token was
  read only from `~/.claude/.credentials.json`. Claude Code 2.1.x moved it into
  the OS credential store on macOS (keychain) and leaves that file a stub, and
  the Windows desktop app delegates auth over IPC and writes a stub too - so on
  both, TokenGauge held a hollow file and reported the provider as dead while it
  was live. It now tries, in order, the `TOKENGAUGE_CLAUDE_OAUTH_TOKEN`
  override, the file, and the OS credential store (macOS keychain / Windows
  Credential Manager), taking the first that is *usable* rather than the first
  that is *present*, so a stub file no longer shadows a good keychain entry.
  The store read is gated to macOS and Windows, so the Linux build pulls no
  secret-service / dbus stack.
- **`--doctor` runs on Windows.** The diagnostic lived in the waybar binary,
  which is Linux-only, so the users with no bar, no daemon and no `--json` were
  exactly the ones who could not run one - and answering "why is my panel
  frozen" took a screenshot, a binary size comparison and five rounds of
  guessing. The report moved to `tokengauge-core` and `tokengauge-tui --doctor`
  prints it. The `tokengauge` binary passes its own sections in (bar wiring,
  the click-action launcher, fleet sync) so its output is unchanged, order
  included, and the Linux-shaped checks - `notify-send`, `xdg-open`, the
  desktop frontends - are omitted rather than reported as failures on a
  platform that has no such thing.
- **The doctor says where the snapshot is, and whether that path means what it
  says.** A `cache_file` left over from another OS - `/tmp/tokengauge-usage.json`
  in a Windows config - is rooted but carries no drive, so Windows resolves it
  against whichever drive happens to be current and writes the snapshot
  somewhere nobody looks. On the machine that prompted this it read as the file
  never having been written at all. The Filesystem section now prints the
  resolved path and fails a `cache_file` that is not absolute, without creating
  the directory it has just called wrong.

### Fixed

- **A hollow credential file says "not signed in", not "expired".** A
  `.credentials.json` with every key present but the token values emptied - what
  Claude Code leaves when the desktop app owns auth or a login lapses - was
  reported as an expired token, sending the user to re-run a login that does not
  repopulate that file. It now reads as not signed in and points at
  `claude setup-token`, the supported way to hand a tool its own token.
- **`--doctor`'s credential check validates instead of stat-ing.** It reported a
  green tick whenever `.credentials.json` existed, even a hollow one every fetch
  rejects. It now checks the same sources the fetcher does, without a network
  call, so the Credentials line says what the bar will actually see.
- **The credential reader honours `CLAUDE_CONFIG_DIR`.** The transcript reader
  already did; the credential reader did not, so a user who relocated `~/.claude`
  had their costs read from the new directory and their credential looked for in
  the old one.

## [0.27.0] - 2026-08-31

### Added

- **A day tooltip names the models that spent it.** Hovering a bar in TOKENS BY
  DAY showed the date, the tokens and the figure, and a "By device" split only
  on a fleet of more than one machine - never what the day actually went on, a
  question the month-wide TOKENS BY MODEL section cannot answer. A "By model"
  table now sits above the device one: what spent the day, then where it was
  spent. It comes off `panel.rs`, so all six frontends carry it. Four rows at
  most, the biggest spenders first, with the tail folded into one `other` row
  once there are more than four: the split rides in every snapshot and hangs
  off a tooltip.

## [0.26.0] - 2026-08-31

### Added

- **Kimi and Grok costs are read natively.** Both CLIs write per-call token
  counts and neither was being read: `~/.kimi-code/sessions/**/wire.jsonl` and
  `~/.grok/sessions/**/updates.jsonl` now go through readers of their own, so
  those plans get the same day, model and burn-rate detail Claude and Codex have
  had, without ccusage. Each carries the trap its format hides - a Kimi
  session-scoped `usage.record` restates the running total of the turns beside
  it, and Grok's `cachedReadTokens` sits *inside* `inputTokens` the way Codex
  reports cache reads rather than beside them, as Anthropic does. Summing either
  naively roughly doubles the bill.
- **Both can take part in fleet sync**, which they could not before: sync buckets
  per-call events, and until now there were none behind either provider. GLM is
  the one left out, having no reader of its own - it is read only when the plan
  is driven through Claude Code.
- **A credit balance is part of the panel.** Three surfaces drew one below the
  panel rather than in it - the waybar tooltip, the TUI and the Windows tray,
  each with its own copy of the same rule - and the Plasma applet, the GNOME
  popup and the Omarchy widget drew it nowhere at all. It is a row in the COST
  section now, so all six show it and one place decides how. It keeps its cents
  past a hundred dollars, where a spend figure drops them: a month's spend is a
  magnitude and a balance is what is left to spend it from. This is also the
  seat a provider selling prepaid credits rather than a usage window needs,
  since such a provider has no meter to draw.

### Fixed

- **A GLM, Kimi or Grok call was counted and then priced at zero.** LiteLLM keys
  a model by where you buy it - `zai/glm-4.6`, `xai/grok-4`,
  `moonshot/kimi-k2-thinking` - and TokenGauge looked it up by the bare name a
  transcript carries. Worse, the table was *filtered* on the same mistaken
  assumption, so all 75 Grok and all 98 GLM entries were dropped before any
  lookup could reach them. A GLM plan driven through Claude Code was landing in
  the right provider, the right day and the right model row, showing tokens and
  $0.00. A lookup now walks the vendor paths, and `kimi-for-coding` - the
  subscription, not a model - is rated at the model the plan serves.
- The zero it produced was self-concealing: `auto` asks ccusage only about
  providers the readers found *nothing* for, and a row with tokens and no money
  is not nothing, so the fallback that would have priced it was never spawned.
- `--doctor` now reports drift when ccusage finds a Kimi or Grok session that
  the readers missed, rather than excusing it as a provider that only ccusage
  can see. That was fair when neither had a reader.
- `scripts/make-prices.py` regenerates the vendored price table, which had no
  generator and had to be sliced by hand with the same rule the runtime applies.
  A machine that has never reached LiteLLM rates against this copy, so it was
  carrying the same hole.

## [0.25.4] - 2026-08-27

### Fixed

- **The GNOME panel draws its bars again.** A limit gauge's fill sat centred in its track rather than starting at the left, and the tokens-by-day and tokens-by-model rows drew no share bar at all: both sized a plain widget and left St to place it, and the row fill chased the row's width from a `notify::width` handler that ran a frame behind the allocation. Both are drawn now, at the width the popup actually gave the row, which is what the other panels have always shown.

## [0.25.3] - 2026-08-27

### Fixed

- **A reset countdown counts down.** "Resets in 6m" against a dashboard saying 3 minutes was not a stale number, it was an unrendered one: the instant a window resets at is absolute, so the countdown is measured against the clock at the moment a row is built - and nothing built a row between polls. The desktop panels re-read the snapshot on the same ten-minute cycle they fetch on, so a panel sat on the countdown it opened with, and the waybar bar replayed the output the daemon had rendered at its last fetch. The panels now re-read while they are on screen (every 30 seconds for Omarchy, Plasma and GNOME; every 15 for the TUI and the tray), and the daemon renders each snapshot request instead of replaying the last one. No provider is asked anything extra for it: a re-render serves the snapshot already on disk, and only its age decides a fetch. The percentages still wait for that fetch - they have nowhere else to come from.
- A window whose reset time had come and gone read "not started", which is what the panel says for a window that never had a reset time at all - so the two limits about to roll over looked like the two that had never been touched. It says "Resets now" from the last minute onwards.
- **A snapshot is stale once a window it reported has reset.** Its percentages describe a window that no longer exists, however young the snapshot is, and the reset is exactly the moment someone looks. Only a rollover since the write counts: a provider reporting an instant already in the past reports the same one on the next fetch, and asking again on every render would never stop.

## [0.25.2] - 2026-08-27

### Fixed

- The "By device" lines in a day or model tooltip lined up only by luck: nothing padded the columns, so they aligned exactly when the label and token widths happened to cancel out, and read as broken on the day they stopped. Labels, token counts and dollar figures are real columns now.

## [0.25.1] - 2026-08-27

### Fixed

- **A panel showing last known values keeps its pace projections.** Every window kept its percentage and its reset time while the "ends ~62%" beside them vanished, which read as the projection being uncomputable when it was being deliberately withheld: `used` stops moving when a fetch fails, so a pace measured against a clock that kept going decays on its own, and the longer the outage lasted the more it read as a slowdown that never happened. The projection is now measured from the instant the figures were true, which is what the rest of a stale panel already shows, so it holds still instead of drifting. A window that has since rolled over still carries none - that one really does describe nothing that is still the case.

## [0.25.0] - 2026-08-27

### Added

- **A day and a model say which machine they came from.** Hovering a row in "Tokens by day" or "Tokens by model" now lists the fleet's split of it, tokens and dollars per device, under the figure it divides. The by-device section answers where a month's total came from; this answers the same question one row at a time, which is the one you have when a Tuesday looks wrong. It comes out of the buckets the row total is built from, so the split cannot disagree with the row above it, and a machine that joined part-way through carries the same `partial` marker the by-device rows use.
- The split is filled only when the fleet holds more than one machine. On a lone machine it would restate the row it hangs off, and a hover target that says nothing teaches you not to hover.
- The per-device figures drop the copy of a day the fleet total drops. Two machines that sync `~/.claude/projects` between them read the same transcripts, so the same day arrives twice; `synthetic_events` has always kept one copy, but the by-device section summed both, which overstated a machine whenever that happened. One rule decides it now, so a split cannot out-run the row it splits.
- **The GNOME popup has tooltips at all now.** It was the one panel that rendered none, so the sync note's full sentence, a day's exact token count and now the per-device split were all written for it and never shown. Meters, bar rows and cost rows all carry one.

## [0.24.4] - 2026-08-26

### Fixed

- **A cost row's badge and suffix take their own line in the desktop panels.** They shared the line with the label and the value, which fits the waybar tooltip, where the line is as wide as the sentence needs, and fits nothing else: the Omarchy widget drew the sync note straight over "Sync", and "Today  $623  ↑109% vs prior avg  ·  865.3M tokens" filled a popup edge to edge. Omarchy, Plasma, GNOME and the Windows tray now put the label and the figure on one line and the badge and suffix on a caption line under it. The TUI and the waybar tooltip are unchanged: both are monospace and as wide as the terminal.
- A row with no badge opened its suffix on the separator that was supposed to divide the two, so "This month" read "·  3.9B tokens". The separator now appears only when a badge precedes it.
- The trend badge's arrow falls back to a font with taller metrics, and a top-aligned row put the badge and the suffix beside it on different baselines. The Omarchy caption line aligns on the baseline, and the Plasma one asks its layout for the same.

## [0.24.3] - 2026-08-26

### Fixed

- The Omarchy widget drew a `Rows` suffix over its own label. That section anchors the label to the left edge and the value group to the right, with nothing between them, which holds while every suffix is a figure and breaks on the first one that is a sentence: the Sync row's note ran back across "Sync". The suffix now elides to whatever room is left beside the label, and the full sentence is still on hover.

## [0.24.2] - 2026-08-26

### Fixed

- Threshold notifications re-fired on every refresh. Anthropic recomputes `resets_at` per request, so the same window returns a few hundred microseconds later each fetch; the roll-over check read any forward move as a fresh window and cleared the one-shot guard. A window now has to move by more than a minute to count as rolled over.
- The one-line installer and updater in the README pointed at a `main` branch that no longer exists, so both would have started 404ing once GitHub's CDN cache expired. They point at `master`, which is and was the default branch.

## [0.24.1] - 2026-08-26

### Fixed

- The Omarchy widget printed every provider fetch error twice - once in the banner added in 0.24.0, once in the error list at the foot of the panel. The banner now carries only the frontend's own read failure, which is what it was for.
- A typo under `[sync.dir]` or `[sync.s3]` was dropped in silence. `--doctor` names it, as it already did for every other section - and a typo there is the one that makes sync quietly not work.
- `--sync-status` read "Sync off" for a fleet that had just been switched on, because it reported what the last completed cycle ran with rather than what the config says.
- The TUI trimmed the by-device list to six machines. A model list has a long tail worth trimming; a device list is the answer to where a total came from, and a hidden machine makes the rows stop adding up.
- `--doctor` walks `PATH` itself instead of spawning `which`, so a machine without `which` installed no longer reports every binary as missing.

## [0.24.0] - 2026-08-26

### Added

- **Fleet sync: one panel covering every machine you code on.** Enable `[sync]` and the cost, token and burn figures cover the whole fleet instead of one machine, with a new "Tokens by device" section showing where the spend came from. Every frontend gets it at once, because the merged figures land where the local ones did and the new section reuses an existing row kind.
- Sync moves one encrypted object per machine through storage you already have: a folder your sync tool handles (Syncthing, Dropbox, Nextcloud, a NAS) or an S3-compatible bucket. There is no service to run and no account to make. `--sync-init` creates the fleet key and prints it; `--sync-join -` reads it from stdin on the next machine, keeping it out of shell history and `/proc`.
- What crosses the wire is token counts bucketed by UTC hour, provider and model - never dollars, so a machine with a stale price table cannot skew the fleet total, and never prompts, paths or credentials. Objects are sealed with XChaCha20-Poly1305 and named with a keyed digest, so whoever holds the folder cannot read your usage or tell which object is which machine. They can still count your machines - there is one object each - and see when you work.
- The COST section grows a "Sync" row that leads with problems: a transport that is down, an object it could not use, or a fleet that has gone stale. Configured-but-not-working under-reports silently, and a total that is quietly too low is worse than one that is visibly missing.
- `[sync.providers]` turns sync off per provider. Use it if you sync `~/.claude/projects` between machines yourself - both machines would otherwise read the same transcripts and double the total. TokenGauge detects that case and says so, but the fix is to leave that provider out.
- Providers whose costs come from ccusage (a Kimi or Grok plan driven from its own CLI) cannot sync yet: there are no per-call events behind them to bucket.
- **A sync screen in the TUI**, reached with `S` or from anywhere with `tokengauge --sync-setup`, which opens it in a terminal for you. Turn sync on, name this machine, point it at a folder, generate or paste a fleet key, and run a round-trip test, all in one place. There is no per-desktop settings pane by design: this screen handles a fleet key, and five implementations of a secret input is five chances to leak one.
- **An S3-compatible bucket as the alternative to a folder**: S3, Cloudflare R2, Backblaze B2's S3 endpoint, MinIO and Garage. Credentials come from `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, never written into the config by the setup screen. Requests are SigV4-signed directly over the HTTP client TokenGauge already links, rather than pulling an AWS SDK and an async runtime in to sign three verbs.
- Every frontend can reach the setup screen: a button in the Plasma settings pane, one in the GNOME popup header, `y` in the Omarchy widget's settings, and a Windows tray menu item.
- `--sync-status` prints what the last cycle did, with `--json` for the raw object. `--sync-test` writes a probe, reads it back and removes it, so you can check the transport and the key before trusting the figures. `--sync-forget <device>` drops a machine you no longer use.

### Fixed

- Middle-clicking the waybar module opened the dashboard of a provider you had switched off. The click resolved the provider from an unfiltered snapshot, so it counted rows nothing else was drawing.
- A transient cost-read failure during `--set-provider` or `--refresh` wiped the recorded history of past days' tokens and costs. The snapshot holds the only copy; every path that refetches now keeps the prior figures when a fetch reads no costs at all, as two of the four already did.
- The Omarchy widget painted the pace projection as dim caption text, so `ends ~120%` read like a footnote rather than the warning it is. It now carries its own tone, and the widget surfaces a fetch error and the "showing last known values" state that the GNOME and Plasma panels already did.
- The COST section's Sync row kept its full detail in a tooltip that no frontend drew. The Plasma applet, the GNOME popup, the Omarchy widget and the Windows tray now show it on hover; the waybar tooltip keeps the shortened inline copy, having nowhere to hover.
- The Plasma compact tooltip formatted today's spend itself and disagreed with every other surface above a hundred dollars ($312.21 against $312). It reads the figure off the panel spec now.
- **The TUI reads the panel from the core like every other frontend does.** It carried its own copies of the pace and trend thresholds, its own section labels ("Rate", "Weekly" against "Burn rate", "7-day"), its own money formatter, and it listed today's models where every other surface lists the month's. All four had drifted. It now loops over the same ordered section list, so the fleet's sync note and per-device breakdown reach it too, and a section added to the core reaches the terminal with no edit. The gauges, the sidebar and the 7-day chart stay - the chart's weekday letters now come off the data's own dates rather than counting back from the wall clock, which relabelled the whole week in a shell left open past midnight.
- PageUp and PageDown in the TUI did nothing: the scroll offset they moved was never read by anything that draws. Removed rather than left as a key that looks bound.
- Sync wording lived in four places - the panel badge, `--sync-status`, `--doctor` and the TUI sync screen - and all four had drifted. There is one `sync::findings()` list now; the doctor's cloned-disk check, which only it had, reaches the panel and `--sync-status` too, and the "no other machine has published yet" line the TUI printed itself is the core's.
- CI checks the desktop frontends. `qmllint` over the Plasma and Omarchy QML, `node --check` over the GNOME extension, and a parse of every frontend manifest - the desktop frontends install separately from the binary, so until now a syntax error in one shipped without any job noticing. The Windows job also runs clippy over the tray, which is the only place it can run at all.
- `--doctor` names where the price table came from: downloaded, cached, cached-past-its-window, or compiled into the binary. A machine that has never reached LiteLLM rates everything against the copy from release day, which is the intended fallback and was invisible - it showed up only as a model priced since then reading as unpriced, which looks like a reader bug.
- Every surface tiers a usage percentage the same way. The 50/80 boundaries were written out five times - in the panel spec, twice in the theme, in waybar's CSS class picker, in the tray icon, and again in the GNOME and Plasma frontends, each of which also decided for itself which window the headline number came from. `--json` rows now carry a resolved `bar` with the percentage and its tone, and everything else maps a tone to a colour.
- A rate limit reads as one on every provider. z.ai, Grok and Codex reported a 429 as a bare `HTTP 429`, which looks like a bug rather than "wait a moment"; they now say what Claude and Kimi already did.
- The GNOME popup and the Plasma applet tracked the selected provider by its position in the list, so a row that appeared or dropped out on a refresh slid a different provider's numbers under what you were reading. Both follow the provider id now, as the Omarchy widget already did.
- The Plasma settings pane listed a hardcoded `codex, claude` when the snapshot had no provider list, hiding every provider added since that line was written.
- A refresh finishing mid-update put the GNOME "Updating…" button back to "Update" while the update was still running.
- The pin-to-bar setting was called four things across four frontends - "Auto", "Highest", "Highest usage" and the raw id. It is "Highest usage" everywhere.
- A provider error body that merely mentioned the word "timeout" was reported as the request having timed out - including one advising you to raise your own timeout.
- Two providers whose names share a prefix could answer for each other's costs, and which one won depended on hash order, so the same snapshot could put the money on a different row from one run to the next.
- Passing two commands at once - `--json --set-provider claude=true` - silently ran whichever the flag chain happened to test first and dropped the other. It now says which two and stops. `--sync-status --json` still means what it always did: `--json` is that command's output format, not a second command.
- `--doctor` no longer prints a section heading with nothing under it. "Credentials" came out empty on a machine with no provider enabled, which reads as a check that failed to run.
- Installing a frontend no longer risks leaving nothing behind: the old copy is moved aside and put back if the replacement fails, rather than deleted before the new one is in place.
- `--set-provider` and a frontend's settings pane writing the config at the same moment could clobber each other's staging file and rename the result over the config.

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
