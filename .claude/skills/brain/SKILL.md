---
name: brain
description: Use and maintain TokenGauge's knowledge base - CLAUDE.md (the load-bearing rules), CONTEXT.md (the vocabulary), docs/*.md (design notes) and docs/adr/ (decisions). Invoke when the user asks how or where something works ("how does staleness work", "where does the panel get built", "explain fleet sync"), when onboarding, or when asked to update, audit or fix the docs after a change. Route through the map below rather than grepping the whole tree.
---

# The brain

TokenGauge has no `brain/` directory. Its knowledge base is four layers, each
with one job. Use them as the first stop for understanding, and keep them true
to the code.

| Layer | Holds | Answers |
| --- | --- | --- |
| `CLAUDE.md` | The rules a change can break, each with the incident that taught it | "why is it done this way", "what must I not regress" |
| `CONTEXT.md` | The domain vocabulary, one term per entry with `_Avoid_` synonyms | "what is a window / credential / fleet store" |
| `docs/*.md` | Design notes for subsystems too big for a CLAUDE.md section (`history.md`, `sync.md`) | "how does history / sync work end to end" |
| `docs/adr/NNNN-*.md` | Decisions with their alternatives, `status:` in frontmatter | "why X and not Y", "is this decided" |
| `README.md` | What a user installs and configures | "how does a user turn X on" |

## Navigating (to answer a question)

1. Match the question to a `CLAUDE.md` section heading first - it is the index.
   `grep -n '^##' CLAUDE.md` lists them. Most "how does X work" questions land
   in one of: frontend parity / panel spec, snapshot and staleness, credentials,
   costs, history, the frontend harnesses, the binary name, the updater, Windows
   install.
2. A term you do not recognise goes to `CONTEXT.md` before the code.
3. For sync or history, `docs/sync.md` / `docs/history.md` carry the detail the
   CLAUDE.md section only points at.
4. For "why not the other way", check `docs/adr/` - and its `status:`. A
   `proposed` ADR is not yet the code.
5. Only then open the source the doc names. If the doc and the code disagree,
   the code wins and the doc is the bug.

## Maintaining (after a change)

A change that makes one of these wrong fixes it **in the same branch**, as its
own docs layer on top of the behaviour commit (see the layered-commit rule in
the global CLAUDE.md).

- **New rule a future change could break** (an invariant, an ordering, a
  platform trap) -> a paragraph in the matching `CLAUDE.md` section, stating the
  rule and the incident or reason behind it. Name the test that asserts it, if
  one does. If no section fits, add one; keep sections about subsystems, not
  about features.
- **New or renamed domain term** -> `CONTEXT.md`, via the `domain-modeling`
  skill, which owns that file's format.
- **New non-obvious decision with real alternatives** -> a new
  `docs/adr/NNNN-*.md` (next number, `status: proposed` until it ships), also
  via `domain-modeling`.
- **Subsystem design moved** -> the matching `docs/*.md`.
- **User-visible behaviour, config key or flag** -> `README.md` and
  `CHANGELOG.md` `[Unreleased]`.
- Style: dense, present tense, prose that says why. No em-dashes. Cite paths and
  test names instead of restating code.

What does **not** go in: anything the code or git history already records, a
changelog of how a rule came to be beyond the one incident that justifies it,
and per-feature walkthroughs (TokenGauge's features are rows in the panel spec,
not documents).

## Auditing (drift check on request)

1. Scope it: the section(s) a recent change touched, or one named subsystem.
   Do not re-audit everything unless asked.
2. For each claim, check the thing it names still exists and behaves as stated:
   function and test names (`git grep`), constants and their values, file
   paths, section and kind lists (`panel::SECTION_IDS`, `SectionKind`), CLI
   flags (`tokengauge --help`).
3. The code wins: correct the doc and note what changed.
4. The `docs-update` skill does the scoping and the rewrite; this skill is the
   map it should follow for this repository.

For a broad audit, fan out read-only agents one per `CLAUDE.md` top-level
section plus one per `docs/*.md`, each returning `{doc, claim, reality, fix}`,
then apply the fixes yourself.
