# remuda's page button

While `remuda serve` runs, the panel offers a button that opens remuda's local
page (switching, signing in, labelling credentials), because the panel is where a
user notices a plan is near its limit. TokenGauge never sees the page's URL or
token. Decision: [0004 in ../decisions.md](../decisions.md#0004-opening-remudas-page).

## The contract with remuda

remuda's side is its `src/served.rs` and "Finding a running page" in its
`brain/architecture/web.md`. TokenGauge relies on exactly this:

- **Runtime directory**: `$XDG_RUNTIME_DIR/remuda`, or remuda's credential store
  when `XDG_RUNTIME_DIR` is unset or empty (TokenGauge takes the store from
  `[credentials] store`).
- **`serve.json`**: `{"pid": u32, "started": u64, "port": u16, "version": string}`,
  no secret. `started` is field 22 of `/proc/<pid>/stat`, counted from the text
  after the last `)` (the command name may hold spaces and parentheses).
- **`serve.url`** holds the URL with its access token, mode 0600. TokenGauge never
  reads it, logs its path or passes it on.
- Neither file is removed when the page stops. A page is up only when
  `serve.json` parses, `/proc/<pid>/stat` gives the recorded start time (rejects a
  dead or reused pid), and a TCP connect to `127.0.0.1:<port>` succeeds within
  300 ms.
- **`remuda open`** (no arguments) is the only way to the page: it reads the token
  itself, opens the browser through a private redirect file, starts
  `remuda-serve.service` if installed and nothing serves (waiting up to 10s), and
  otherwise exits non-zero with a message.

Pinned by `remuda::tests::the_serve_keys_are_spelled_as_the_adr_says` and
`remuda::tests::a_start_time_is_the_twenty_second_stat_field`; renaming a key is a
change to decision 0004 and to remuda. Needs a remuda newer than 0.4.2; against an
older one the button never appears.

## How it reaches the frontends

- `remuda::status` runs the check; `--json` carries a top-level
  `remuda: {serving, version}` resolved on every render (like `refresh_hint`),
  never stored in the snapshot. It never spawns `remuda`; a missing file is one
  failed read. Off Linux it returns not serving without looking.
- `tokengauge --open=remuda` runs `remuda open` and waits, so a chained
  `&& --json` gets remuda's error on stderr. The binary is `remuda` on `PATH`,
  else `~/.local/bin/remuda` (`launch::remuda_binary`), since GUIs often start
  without `~/.local/bin` on `PATH`.
- Surfaces: the TUI and the Omarchy widget bind `m`; GNOME, Plasma and the Omarchy
  widget draw a header button. Waybar has no button (`--open=remuda` is for a
  click binding); the tray builds only where remuda does not run.
  `panel::tests::every_frontend_with_a_header_offers_remuda_while_it_serves` holds
  the four to it and keeps `serve.url` out of their sources.
- Tests never read the developer's runtime directory: `json_snapshot` takes the
  status as a parameter, the daemon reads it through a probe on `DaemonState` its
  tests stub, and the e2e harness and `make-panel-fixture.sh` point
  `XDG_RUNTIME_DIR` at a temp directory. The e2e test of `--open=remuda` empties
  `PATH` and plants a script where remuda's installer would.

## Sources

- `crates/tokengauge-core/src/remuda.rs` (`status`, tests)
- `crates/tokengauge-core/src/launch.rs` (`remuda_binary`)
- `crates/tokengauge-waybar/src/snapshot.rs` (`json_snapshot`), `crates/tokengauge-waybar/src/daemon.rs`, `crates/tokengauge-waybar/tests/e2e.rs`
- `crates/tokengauge-core/src/panel.rs` (`tests::every_frontend_with_a_header_offers_remuda_while_it_serves`)
