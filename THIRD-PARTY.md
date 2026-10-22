# Third-party work in TokenGauge

TokenGauge is dual-licensed MIT OR WTFPL (see `LICENSE`). The work listed here
came from elsewhere and keeps its own terms.

## CodexBar - MIT

<https://github.com/steipete/CodexBar>, Copyright (c) 2026 Peter Steinberger.

CodexBar is the macOS menu-bar app this project started from, and the reason
most of the providers work at all: which endpoint answers for a plan, what the
payload is shaped like, where each CLI keeps its credential.

What is here because of it:

- **`crates/tokengauge-core/src/pace.rs`** is a port of `UsagePace.swift`.
- **`crates/tokengauge-core/src/{claude,codex,kimi,grok,glm,cursor,openrouter,opencode}.rs`**
  implement provider protocols worked out there. The code is ours; the
  protocols are not discoverable without it.
- **`crates/tokengauge-core/src/payload.rs`**'s `Credits` / `CreditLimit`
  follow `CreditsSnapshot`, including the distinction between a key's balance
  and a subscription's cap.
- **The provider marks** came from its `ProviderIcon-*.svg`, but for Kimi's,
  from lobe-icons, and opencode's, redrawn from its favicon. They now live in
  [selvedge](https://github.com/Arzaroth/selvedge) (`selvedge::brands`), whose
  `assets/providers/NOTICE` carries each one's notice; it is shared with
  remuda.

CodexBar's MIT notice, which applies to the ported work above:

```text
    MIT License

    Copyright (c) 2026 Peter Steinberger

    Permission is hereby granted, free of charge, to any person obtaining a
    copy of this software and associated documentation files (the
    "Software"), to deal in the Software without restriction, including
    without limitation the rights to use, copy, modify, merge, publish,
    distribute, sublicense, and/or sell copies of the Software, and to
    permit persons to whom the Software is furnished to do so, subject to
    the following conditions:

    The above copyright notice and this permission notice shall be included
    in all copies or substantial portions of the Software.

    THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
    OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
    MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
    IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
    CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
    TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
    SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

## oorestisime/TokenGauge - no licence stated

<https://github.com/oorestisime/TokenGauge>

The repository this one was forked from, which described itself as "Waybar &
Tui support for codexbar". This fork's own history starts at 0.5.0.

**It states no licence**, and that is recorded here rather than resolved. The
terms in `LICENSE` are this project's own; they are not a grant anyone else
made.

## basecamp/omarchy - MIT

<https://github.com/basecamp/omarchy>, Copyright (c) David Heinemeier Hansson.

`omarchy/arzaroth.tokengauge` is a third-party plugin for omarchy-shell,
adapted from their own `omarchy.agents` plugin (`shell/plugins/agents/`). It
carries structure from it: the panel's Flickable and KeyboardPanel
scaffolding, the IpcHandler surface, and the shape a plugin presents to the
shell. Roughly a quarter of the widget's lines are theirs.

Their MIT notice travels with the payload in
`omarchy/arzaroth.tokengauge/NOTICE`.

This is a third-party plugin and is not endorsed by the Omarchy project.

## akitaonrails/ai-usagebar - MIT

<https://github.com/akitaonrails/ai-usagebar>

A parallel implementation of the same idea, in the same language. Nothing is
copied. It is listed because it is read: bugs found there are frequently ours
too, and their handling of a `null` where a list was expected was our bug on
two Claude fields.

## People

TokenGauge is not the work of one person. `LICENSE` covers the contributions
of everyone who has written part of it, and who that is stays where it cannot
go stale: `git shortlog -sn`, or the
[contributors page](https://github.com/Arzaroth/TokenGauge/graphs/contributors).

## Provider brand marks

Anthropic, OpenAI, xAI, Moonshot, Z.ai, Cursor, OpenRouter and the rest remain
the property of their owners. Their names and marks identify which service a
row describes and nothing more.
