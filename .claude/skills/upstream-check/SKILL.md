---
name: upstream-check
description: Routine review of the two projects TokenGauge borrowed code from - steipete/CodexBar (the native provider fetchers, the pace metric, the provider icons) and basecamp/omarchy (the shell surfaces the bar widget rides on, plus the omarchy.agents widget ours was adapted from). Use for "did upstream move", "should we backport", "check codexbar", "check omarchy", "upstream check", or any periodic upstream drift review.
---

# Upstream check

TokenGauge internalized work from two projects and depends on neither at
runtime. Nothing tells us when they move except this check.

| Upstream | What we took | What can rot |
| --- | --- | --- |
| [steipete/CodexBar](https://github.com/steipete/CodexBar) (MIT, macOS/Swift) | The provider protocols behind `crates/tokengauge-core/src/{claude,codex,kimi,grok,glm}.rs`, `pace.rs` (a port of `UsagePace.swift`), and `assets/providers/ProviderIcon-*.svg` | An endpoint or auth shape moves, a payload grows a field we drop on the floor, a provider bug they fixed is still ours |
| [basecamp/omarchy](https://github.com/basecamp/omarchy) (default branch `quattro`) | Nothing copied. `omarchy/arzaroth.tokengauge` is a third-party plugin loaded into omarchy-shell, adapted from their `omarchy.agents` widget | The shell internals we import have no stability promise (the project is `4.0.0.alpha`), and their widget grows panel features ours lacks |

Read `BASELINES.md` in this folder first: it records what each upstream looked
like at the last check. Update it at the end of every run.

## CodexBar

Releases are the readable unit; commits are not. Pull the notes published since
the baseline and triage them:

```bash
gh api repos/steipete/CodexBar/releases --paginate \
  --jq '.[] | select(.published_at > "<baseline date>") | "## \(.tag_name) (\(.published_at[0:10]))\n\(.body)\n"' \
  > /tmp/codexbar-releases.md
```

CodexBar is a macOS menu-bar app with a much wider provider list than ours.
Most of every release is noise for us.

**Signal** - anything touching a provider we ship (Claude, Codex, Kimi, Grok,
z.ai/GLM), specifically: auth file locations and credential shapes, endpoints
and their headers, response fields and quota types, window classification and
labels, ccusage-equivalent cost maths, and pace.

**Noise** - SwiftUI, menu bar layouts and tokens, widgets, Settings, iCloud and
Keychain plumbing, localization, the Spend dashboard's internals, and every
provider we do not ship (Cursor, Antigravity, Kiro, Fireworks, OpenCode, ...).
macOS-only paths we deliberately skipped stay skipped: browser-cookie imports
(`kimi-auth`, Grok web cookies), claude-swap, iCloud sync.

Check a candidate against our code before calling it a gap. Useful probes:

```bash
gh search code --repo steipete/CodexBar "CREDIT_LIMIT" --limit 5 --json path --jq '.[].path'
gh api repos/steipete/CodexBar/contents/<path> --jq '.content' | base64 -d | grep -n "<thing>" -A6 -B6
gh api "repos/steipete/CodexBar/commits?path=Sources/CodexBarCore/UsagePace.swift&per_page=15" \
  --jq '.[] | "\(.commit.committer.date[0:10]) \(.commit.message|split("\n")[0])"'
```

Their JS provider plugins (`Sources/CodexBarCore/Resources/Plugins/*.js`) and
`docs/<provider>.md` are the fastest read of a wire format; the Swift fetchers
live under `Sources/CodexBarCore/Providers/<Provider>/`.

Icons: our copies are the upstream files recoloured to `currentColor`, so
compare them modulo `fill`:

```bash
gh api repos/steipete/CodexBar/contents/Sources/CodexBar/Resources/ProviderIcon-<name>.svg \
  --jq '.content' | base64 -d > /tmp/up.svg
diff <(sed 's/fill="[^"]*"/fill=X/g' /tmp/up.svg) \
     <(sed 's/fill="[^"]*"/fill=X/g' assets/providers/ProviderIcon-<name>.svg)
```

Upstream renames happen (`ProviderIcon-zai.svg` was our `-glm`); a missing file
is a rename to hunt down, not a deletion.

## Omarchy

Two questions, one repo.

**1. Did a surface our widget rides on move?** These are the paths whose
breakage we would otherwise learn about from a user with a blank panel:

```bash
for p in shell/plugins/agents shell/services/PluginRegistry.qml shell/Ui \
         shell/Commons/Style.qml shell/Commons/Color.qml config/omarchy/shell.json; do
  echo "### $p"
  gh api "repos/basecamp/omarchy/commits?path=$p&since=<baseline date>&per_page=100" \
    --jq '.[] | "\(.commit.committer.date[0:10]) \(.sha[0:8]) \(.commit.message|split("\n")[0])"'
done
```

Ask per path. The compare endpoint caps `files` at 300 without saying so, and a
trial month once spanned 314 commits, so watched changes hide behind unwatched
ones.

Deliberately not watched:

- `migrations/` - upstream lands one with nearly every change (46 of 61 commits
  in a trial month), and a monitor that always fires is one you stop reading. A
  migration that reaches our widget arrives with a `shell.json` change anyway.
- `bin/omarchy-agent-usage-*` - their usage-record contract. We render from
  `tokengauge-waybar --json`, so it is not a surface we depend on. Add it if
  that ever changes.

A hit means "read the diff", not "you are broken".

**2. Did their widget grow something ours lacks?** `shell/plugins/agents/`
(`Panel.qml`, `Main.qml`, `manifest.json`) is the parity target, and the
installed copy at `/usr/share/omarchy/shell/plugins/agents/` is easier to read
than the API. Their manifest's `defaults` and `schema` are the quickest diff
against `omarchy/arzaroth.tokengauge/manifest.json`. Feature parity is a
judgement call, not an obligation: they ship Claude, Codex, and Fireworks only,
and our five-frontend rule (see `CLAUDE.md`) makes a panel feature five
implementations, not one.

A monthly systemd timer that did check 1 automatically shipped in 0.17.0 and was
removed in the same branch (`a1ddb5d`): polling GitHub on behalf of every
TokenGauge user, for a project they may not run, does not belong in the product.
It stays a local maintenance task, which is what this skill is.

## Report

Answer both questions plainly, then list backport candidates worth the work:
what upstream fixed, which of our files owns it, and whether a user on our side
can actually hit it. Say "nothing to do" when that is the answer. Do not open
issues or start implementing without being asked - this check ends in a
recommendation.

Finish by rewriting `BASELINES.md` with today's date, the newest CodexBar
release tag, and the current `basecamp/omarchy` HEAD sha on `quattro`:

```bash
gh api repos/basecamp/omarchy/commits/quattro --jq '.sha'
gh api repos/steipete/CodexBar/releases/latest --jq '.tag_name'
```
