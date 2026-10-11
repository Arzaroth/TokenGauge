# Testing

Rust unit and integration tests, three harnesses that drive the frontends against
one recording of `--json`, and the source-grepping guards that hold the frontends
to the panel spec. The gate itself is in [../operations.md](../operations.md).

## Frontend harnesses

`qmllint` and `tsc` only read the frontends; these run them, each against the
same recording, so a frontend and the binary cannot quietly disagree:

| Harness | What it loads | What it covers |
| ------- | ------------- | -------------- |
| `crates/tokengauge-waybar/tests/e2e.rs` | the shipped binary | config, cache, staleness, `panel_spec`, the JSON |
| `tests/qml/run.sh` | `Usage.qml`, `Service.qml` | bindings, the snapshot read back, the next command |
| `tests/gnome/run.sh` | the compiled `extension.js` | all of that, plus the widget tree it draws |
| `tests/gnome/shell/run.sh` | the compiled extension in a real headless GNOME Shell | GJS errors, sizing, scrolling; screenshots the popup |

- None touches a network, a credential or a daemon. The binary's tests seed a
  fresh snapshot so `cache_is_stale()` says serve; the one test that wants a fetch
  backdates the write and lands in the credential walk. The stubs answer a command
  by its command line.
- `scripts/make-panel-fixture.sh` records `tests/qml/fixtures/panel.json` off the
  same seeded snapshot the binary's tests use. Its machine has no fleet store on
  purpose: history comes out empty, the state a frontend most easily draws wrong.
  The fixture and the e2e harness point `XDG_RUNTIME_DIR` at a temp directory and
  set `[credentials] store = ""`.
- The GNOME harness runs the compiled JavaScript, so it needs `scripts/build.sh`
  first and says so rather than passing vacuously.
- The shell harness is local and opt-in (it pulls a Fedora image). Run it after a
  GNOME popup layout change, with `--live` and at `--gnome 45` as well as the
  default 50: their scroll views differ. The St stub throws on an `undefined`
  initializer property, as GJS does, after a crash cut the GNOME panel off at
  COST from 0.30.0 to 0.39.0.
- Waybar and the tray have no harness: waybar's surface is the binary's output
  (covered by e2e), the tray is Rust that builds only on Windows and macOS.

## Source-grepping guards

`panel::tests` reads each frontend's source to enforce the parity rules no
compiler can; the list is in [panel-spec.md](panel-spec.md#the-source-grepping-guards).

## Cost and sync tests

- `crates/tokengauge-core/tests/cost_fixture.rs`: readers vs a checked-in
  ccusage golden. See [cost.md](cost.md).
- `crates/tokengauge-core/tests/fleet_sync.rs`: the sync round trip and merge.
  See [sync.md](sync.md).
- `crates/tokengauge-core/tests/state_files.rs`: every state file derives from
  the snapshot's parent.
- Contract pins: `payload::tests::the_credential_keys_remuda_reads_are_spelled_as_decision_0003_says`,
  `remuda::tests::the_serve_keys_are_spelled_as_decision_0004_says`,
  `remuda::tests::a_start_time_is_the_twenty_second_stat_field`.

## Coverage

`scripts/coverage.sh` runs `cargo llvm-cov` and ranks files by uncovered lines
(`--html` for the browsable report). Local only, no CI floor. Process surfaces
(event loops, `main`, the daemon accept loop) stay uncovered on purpose; a logic
file drifting down the list is the signal.

## Sources

- `crates/tokengauge-waybar/tests/e2e.rs`, `crates/tokengauge-waybar/tests/fixtures/snapshot.json`
- `tests/qml/{run.sh,plasma.qml,omarchy.qml,plasma-contract.qml,Check.qml}`, `tests/qml/fixtures/panel.json`
- `tests/gnome/{run.sh,extension.test.mjs,widgets.test.mjs}`, `tests/gnome/shell/run.sh`
- `crates/tokengauge-core/tests/{cost_fixture,fleet_sync,state_files}.rs`
- `crates/tokengauge-core/src/panel.rs` (`tests`), `scripts/make-panel-fixture.sh`, `scripts/coverage.sh`
