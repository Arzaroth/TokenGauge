# Panel spec

The abstraction that keeps six frontends in agreement: the whole panel is
resolved once, in `crates/tokengauge-core/src/panel.rs`, and every frontend only
draws it. The parity rule this serves is the first hard rule in `CLAUDE.md`
("Frontend parity"); the per-frontend side is [frontends.md](frontends.md).

## What it produces

`panel_spec(&ProviderRow, &PanelConfig)` returns an ordered `Vec<Section>`, each
carrying a `SectionKind` and rows whose display strings, fractions, tones and
tooltips are already computed. Rust frontends (waybar, TUI, tray) call it
directly; QML and JS frontends read it off the `panel` field of each row in
`tokengauge --json`.

| `SectionKind` | Shape |
| ------------- | ----- |
| `Meters` | label + value on one line, full-width bar under it, then `footnote` and the tinted `badge`. The limit gauges and the combined header. |
| `Bars` | one line per row with the share bar filling the row behind the text. Tokens by day, model, device. |
| `Rows` | label, value, tinted `badge`, dim `suffix`, no bar. The cost figures, the status lines. |

Canonical sections, in order (`panel::SECTION_IDS`), each dropped when it has no
data: `status`, `plans`, `limits`, `cost`, `tokens_by_day`, `tokens_by_model`,
`tokens_by_device`.

- `plans` (the combined header) and a repeated `limits` exist only for a provider
  with several credentials; repeated sections share an id and differ in
  `Section.group`. See [../features/several-plans.md](../features/several-plans.md).
- `status` is how a stale row says why: the fetch error `apply_stale_fallback`
  drops rides on the payload as `stale_reason` and renders as a section, because
  a bare `stale` chip reads the same for a network blip and a credential expired
  for weeks.
- `tokens_by_device` appears only for a fleet-merged provider; presence is the
  indicator. See [../features/fleet-sync.md](../features/fleet-sync.md).
- Rows the spec drops: a window the provider does not report (`used: None`) and a
  `placeholder: true` extra window.

Adding a section is a `panel.rs`-only change. Adding a **kind** touches all six
frontends, and `panel::tests::every_panel_frontend_handles_every_section_kind`
greps each frontend's source as the backstop no compiler gives (the kind is a
Rust enum, a QML string and a TypeScript union).

## Tone

`Tone` is a semantic tier (`good` / `warn` / `critical` / `dim` / `normal`), not
a colour. Each frontend maps it onto its own palette; the Omarchy widget
collapses `good`/`warn` onto the bar foreground because omarchy themes carry
only a foreground and an urgent colour. Pace and trend tones are
`Tone::for_pace` / `Tone::for_trend`, resolved here and nowhere else.

## Content that looks like chrome

Chrome (header, update banner, provider selector, settings pane, input hints) is
per-frontend. Three things that sit in chrome are content and resolve here:

- **Refresh hint** - `panel::refresh_hint` turns `updated_iso` into the sentence
  every refresh control says on hover, carried as `refresh_hint` per row and
  asserted by `every_frontend_says_when_it_last_refreshed`. Resolved per render,
  because the age keeps moving; the instant is the payload's, never the
  process's.
- **Bar icon hover** - `panel::bar_tooltip`: every limit window with its tier
  (active group first, then the combined figures), then today's spend. Built off
  `panel_spec`, so it can never name a window the panel does not draw. Asserted
  by `every_frontend_with_a_bar_icon_says_the_same_thing_on_hover` (Plasma, GNOME,
  Quickshell, tray; waybar's tooltip already is the panel, the TUI has no icon).
- **`[panel]` options** - `active_credential_only`, `plans_total`, `split_bars`
  are read by `panel_spec` and `bar_tooltip`, never by a frontend. Split bars ride
  on `PanelRow.segments`; pixel frontends draw `Segment.width`, waybar and the TUI
  draw cells through `panel::segment_cells`. Guarded by
  `every_panel_frontend_draws_split_bars` and
  `every_settings_pane_offers_the_panel_options`.

Rendering is not fetching: a reset countdown is measured against the clock when
the row is built, so every frontend re-renders on a short cycle independent of
`refresh_secs` (see [snapshot.md](snapshot.md)).

## The source-grepping guards

`panel::tests` reads frontend sources (the GNOME `.ts`, not build output) to
enforce what no type system can:

| Test | Enforces |
| --- | --- |
| `every_panel_frontend_handles_every_section_kind` | every kind drawn everywhere |
| `every_frontend_says_when_it_last_refreshed` | `refresh_hint` used |
| `every_frontend_with_a_bar_icon_says_the_same_thing_on_hover` | `bar_tooltip` used |
| `every_panel_frontend_draws_split_bars` | segments drawn (matches the drawing call) |
| `every_settings_pane_offers_the_panel_options` | `--set-panel` options offered |
| `every_frontend_with_a_second_screen_draws_the_history_series` | history drawn (not waybar) |
| `every_frontend_with_a_header_offers_remuda_while_it_serves` | remuda button (TUI, GNOME, Plasma, Omarchy) |

## Sources

- `crates/tokengauge-core/src/panel.rs` (`panel_spec`, `SECTION_IDS`, `SectionKind`, `Tone`, `refresh_hint`, `bar_tooltip`, `segment_cells`, `tests`)
- `crates/tokengauge-core/src/rows.rs` (`ProviderRow`), `crates/tokengauge-core/src/fmt.rs`, `crates/tokengauge-core/src/theme.rs`
- `crates/tokengauge-core/src/config.rs` (`PanelConfig`)
- `crates/tokengauge-waybar/src/snapshot.rs` (`json_snapshot`), `crates/tokengauge-waybar/src/render.rs`
- `gnome/tokengauge@arzaroth.github.io/panel.ts` (the hand-declared `--json` contract)
