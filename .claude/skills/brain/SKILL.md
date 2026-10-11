---
name: brain
description: Use and maintain the brain/ knowledge base - the committed docs describing how TokenGauge works (architecture, features, decisions, glossary) - together with CLAUDE.md, which holds the rules a change can break. Invoke when the user asks how or where something works ("how does staleness work", "where does the panel get built", "explain fleet sync"), when onboarding, or when asked to update, audit or fix the brain after a change. Read brain/BRAIN.md first; navigate via the indexes, not a whole-tree grep.
---

# The brain

`brain/` is the committed knowledge base: how TokenGauge works, written so you
can answer without reading the source. Use it as the first stop for
understanding, and keep it true to the code. `CLAUDE.md` is its companion: the
rules a change can break, each with the incident that taught it.

## Navigating (to answer a question)

1. Open `brain/BRAIN.md` - the entry index. It has topic tables and a **Find by
   question** table.
2. Drill via the sub-indexes: `brain/architecture/index.md` (the how) and
   `brain/features/index.md` (the what). Don't grep the whole tree; the indexes
   are the routing layer.
3. Land on the leaf doc. It is self-contained and ends with a `## Sources` list
   of the real code paths - follow those only if the doc isn't enough or you
   suspect drift.
4. Terms -> `brain/glossary.md` (each with the synonyms to avoid). Rationale /
   "why not the other way" -> `brain/decisions.md` (the former ADRs keep their
   numbers: "ADR 0003" in a commit or a test name is `### 0003` there).
5. About to change code -> also read the matching `CLAUDE.md` section
   (`grep -n '^##' CLAUDE.md`): frontend parity, snapshot and staleness,
   credentials, costs, history, the harnesses, the binary name, the updater,
   macOS, Windows.

Structure:

```
brain/BRAIN.md                 root index + find-by-question
brain/stack.md  operations.md  glossary.md  decisions.md
brain/architecture/index.md    overview panel-spec frontends snapshot
                               credentials cost sync testing distribution
brain/features/index.md        providers several-plans cost-tracking
                               spend-history fleet-sync pace notifications
                               remuda-page doctor updates
```

## Maintaining (after a change)

The brain is load-bearing and the rule is in `CLAUDE.md` ("The brain
(knowledge base)"). The code is the source of truth - if a doc disagrees with
reality, fix the doc.

- A change that makes a brain doc wrong fixes that doc **in the same branch**,
  as its own docs layer on top of the behaviour commit (one kind of work per
  commit, each leaving the tree green).
- New feature -> add `brain/features/<name>.md`, then a row in
  `brain/features/index.md` and the features table in `brain/BRAIN.md` (and a
  Find-by-question row if it answers a new question). New cross-cutting
  subsystem -> add or update a `brain/architecture/*.md` and its index rows.
- New non-obvious decision -> `brain/decisions.md`, under its area: the
  decision, the why and the rejected alternatives. One with real alternatives
  gets the next `### NNNN` number; a smaller one is a bullet.
- New or renamed term -> `brain/glossary.md` under its area: bold term, one
  definition, a link to its doc, and an `Avoid:` list of synonyms.
- New rule a future change could break -> a paragraph in the matching
  `CLAUDE.md` section stating the rule and the incident behind it, naming the
  test that asserts it; the brain doc links there rather than repeating it.
- Stack or toolchain change -> `brain/stack.md`; gate, script, CI or release
  change -> `brain/operations.md`.
- User-visible behaviour, config key or flag -> also `README.md` and
  `CHANGELOG.md` `[Unreleased]`.
- Style: dense, skimmable, present tense, normal prose. No em-dashes.
  Cross-link siblings with relative markdown links. Cite source paths and test
  names instead of restating code. End each leaf with `## Sources`.

What does **not** go in: anything git history already records, and a changelog
of how a rule came to be beyond the one incident that justifies it.

## Auditing (drift check on request)

When asked to verify the brain matches reality:

1. Scope it to the relevant leaf doc(s) - don't re-audit everything.
2. For each, read the `## Sources` paths and confirm the doc's claims still hold:
   function and test names (`git grep`), constants and their values, file
   paths, section and kind lists (`panel::SECTION_IDS`, `SectionKind`), CLI
   flags (`tokengauge --help`), config keys (`config.rs`).
3. Where they diverge, the code wins: correct the doc. Note what you changed.
4. If a subsystem moved or a feature shipped or was removed, update the indexes
   (`BRAIN.md`, the two `index.md`) too so navigation stays accurate.
5. Check links: every relative link and anchor resolves, and no doc points at a
   path that no longer exists.

For a broad audit, fan out read-only agents (one per architecture or feature
doc, plus one for the `CLAUDE.md` sections) that each diff their doc against
its `## Sources` and return `{doc, claim, reality, fix}`, then apply the fixes
yourself. A personal `docs-update` skill, where installed, can do the scoping;
this skill is the map it should follow here.
