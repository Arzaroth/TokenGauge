---
status: proposed
---

# A remuda button opens remuda's page, and TokenGauge never holds its token

`remuda serve` runs a local page for switching, signing in and labelling the
credentials ADR 0003 reads. The panel is where a user notices that a plan is
close to its limit, so it is where they want that page. We chose to show a
button **only while a page is being served**, to detect that from a status
file remuda writes, and to open the page **only by running `remuda open`**.
TokenGauge never sees the page's URL.

## The contract

remuda's side is `src/served.rs` and the section "Finding a running page" of
`brain/architecture/web.md` in its repository. TokenGauge relies on exactly
this.

- **The runtime directory** is `$XDG_RUNTIME_DIR/remuda`, or remuda's
  credential store when `XDG_RUNTIME_DIR` is unset or empty. TokenGauge takes
  the store from `[credentials] store`, as ADR 0003 does, rather than repeating
  remuda's `$REMUDA_STORE` / `$XDG_DATA_HOME` resolution.
- **`serve.json`** in it is `{"pid": u32, "started": u64, "port": u16,
  "version": string}`. It holds no secret. `started` is the process start time,
  field 22 of `/proc/<pid>/stat`, counted from the text after the last `)`
  because the command name in field 2 may hold spaces and parentheses.
- **`serve.url`** beside it holds the page URL with its access token, mode
  0600. TokenGauge never reads it, never logs its path, and never passes it on.
- **Neither file is removed when the page stops**, because a killed process
  cannot. A page is up only when `serve.json` parses, `/proc/<pid>/stat` gives
  the recorded start time (which rejects a dead pid and a reused one), and a
  TCP connect to `127.0.0.1:<port>` succeeds within 300 ms.
- **`remuda open`**, with no arguments, is the only way to the page. It reads
  the token itself and opens the browser through a private redirect file, so
  the token is on no command line, and prints the URL without its token. When
  nothing serves it starts `remuda-serve.service` if that unit is installed
  and waits up to 10s for it, and otherwise exits non-zero with a message.

`remuda::tests::the_serve_keys_are_spelled_as_the_adr_says` pins the key names
and `remuda::tests::a_start_time_is_the_twenty_second_stat_field` the parsing.
Renaming a key is a change to this ADR and to remuda.

## How it reaches the frontends

- `tokengauge-core::remuda::status` runs the check and `--json` carries the
  result as a top-level `remuda: {serving, version}`, resolved on every render
  like `refresh_hint`, not stored in the snapshot. A page started or stopped
  after the last fetch has to show or hide the button on the next render, and
  a stored flag would describe the moment of the fetch. The snapshot schema is
  untouched.
- The check never spawns `remuda`: a render is frequent and must stay cheap.
  A missing directory or file is one failed read.
- `tokengauge --open=remuda` runs `remuda open` and waits for it, so a frontend
  chaining `&& --json` behind it gets remuda's own error on stderr. The binary
  is `remuda` on `PATH`, else `~/.local/bin/remuda`, where remuda's installer
  puts it, because a GUI often starts without `~/.local/bin` on its `PATH`
  (`launch::remuda_binary`).
- The TUI and the Omarchy widget bind `m`; GNOME, Plasma and the Omarchy
  widget draw a header button.
  Waybar has no button to draw, and `--open=remuda` is there to bind to a click
  in its config. The tray builds only for Windows and macOS, where remuda does
  not run, so it has nothing to show. `status` returns not serving off Linux
  without looking at anything.

## Considered options

- **Detect an open port.** A listener on 7429 says nothing about whose it is,
  and the page refuses every request without the token, so opening the bare
  URL would show a page that can do nothing.
- **Read `serve.url` and open it with `xdg-open`.** It works, and it puts a
  credential for the page in TokenGauge's process, on a command line every
  local process can read, and in reach of every frontend. The token exists so
  that only the person who started the page can drive it; `remuda open` keeps
  it that way.
- **Show the button whenever remuda is installed**, and let `remuda open` start
  `remuda-serve.service`. A button that fails for anyone who has not installed
  that unit reads as broken. It can come later, when the unit is the common
  case. Until then the unit is started only for a page that stopped between a
  render and the click, which `--open=remuda` waits through like any other
  `remuda open`.

## Consequences

- remuda releases after 0.4.2 write the files. Against an older remuda the
  button never appears, which is the honest state: there is no page to open.
- A test of `--json` must not read the developer's runtime directory.
  `json_snapshot` takes the status as a parameter, the daemon reads it through
  a probe on its state that its tests stub, and the e2e harness and
  `scripts/make-panel-fixture.sh` point `XDG_RUNTIME_DIR` at their own
  temporary directory. The e2e test of `--open=remuda` empties `PATH` and puts
  a script where remuda's installer would, so no real remuda runs.
- `panel::tests::every_frontend_with_a_header_offers_remuda_while_it_serves`
  holds the TUI, GNOME, Plasma and the Omarchy widget to the button and keeps
  `serve.url` out of their sources.
