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
- **`assets/providers/ProviderIcon-*.svg`** are copied verbatim. Their own MIT
  notice travels with them in `assets/providers/NOTICE`.

The full MIT notice is reproduced in `assets/providers/NOTICE` and applies to
the ported work above as well.

## oorestisime/TokenGauge - no licence stated

<https://github.com/oorestisime/TokenGauge>

The repository this one was forked from, which described itself as "Waybar &
Tui support for codexbar". This fork's own history starts at 0.5.0; everything
before that is theirs.

**It states no licence.** That is recorded here rather than resolved: a fork of
a repository that grants no terms is not something a downstream licence can
fix, and anyone redistributing TokenGauge should know the pre-0.5.0 history is
in that position.

## basecamp/omarchy

<https://github.com/basecamp/omarchy>

`omarchy/arzaroth.tokengauge` is a third-party plugin for omarchy-shell,
adapted from their `omarchy.agents` widget. Nothing is copied.

## akitaonrails/ai-usagebar - MIT

<https://github.com/akitaonrails/ai-usagebar>

A parallel implementation of the same idea, in the same language. Nothing is
copied. It is listed because it is read: bugs found there are frequently ours
too, and their handling of a `null` where a list was expected was our bug on
two Claude fields.

## Provider brand marks

Anthropic, OpenAI, xAI, Moonshot, Z.ai, Cursor, OpenRouter and the rest remain
the property of their owners. Their names and marks identify which service a
row describes and nothing more.
