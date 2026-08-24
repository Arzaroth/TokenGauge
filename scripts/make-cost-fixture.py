#!/usr/bin/env python3
"""Regenerate the cost-reader fixture and its ccusage golden.

The fixture is a fake home directory holding transcripts reduced to the fields
that are actually billed. Message content, prompts, file paths, branch names and
project names never reach it: `content` is emptied and `cwd` is redacted. Every
identifier is replaced with a deterministic stand-in and every timestamp is
shifted to a fixed epoch, so what is checked in says how many tokens were spent
and nothing about on what, for whom, or when. Repeats are preserved - two
records of one streamed message keep sharing an id - because that is exactly
what the dedup path is there to exercise.

The golden records **ccusage's** token verdict on that same tree, which is what
lets CI check the readers without installing Node.

Usage (from the repo root, with ccusage available):

    python3 scripts/make-cost-fixture.py

Two things this learned the hard way and must keep doing:

- **Emit compact JSON.** ccusage prefilters transcript lines with a string match
  against compact separators, so `json.dumps` defaults (`": "`) make it read a
  perfectly valid file as empty.
- **Compare token totals, not per-day buckets or money.** Days depend on the
  reader's timezone and money depends on whichever price table each side
  fetched; token counts come from the files and are the parser invariant.
"""

import json
import os
import subprocess
import sys
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

class Anonymizer:
    """Deterministic stand-ins for every identifier and timestamp.

    Repeats matter: two records of the same streamed message must keep sharing
    a requestId and message id, or the dedup they exist to exercise stops being
    exercised. So each distinct value maps to one stand-in, and timestamps are
    all shifted by the same delta - order and spacing survive, the calendar the
    work happened on does not.
    """

    EPOCH = datetime(2026, 1, 1, tzinfo=timezone.utc)

    def __init__(self):
        self._ids = {}
        self._shift = None

    def ident(self, prefix, value):
        if not value:
            return value
        key = (prefix, value)
        if key not in self._ids:
            self._ids[key] = f"{prefix}-{len(self._ids):05d}"
        return self._ids[key]

    def note_time(self, value):
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
        if self._shift is None or parsed < self._shift:
            self._shift = parsed

    def time(self, value):
        """Millisecond precision, which is what both CLIs write.

        `datetime.isoformat` emits microseconds, and ccusage's Codex reader
        rejects those outright ("Invalid Codex timestamp").
        """
        if not value:
            return value
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
        moved = self.EPOCH + (parsed - self._shift)
        return f"{moved.strftime('%Y-%m-%dT%H:%M:%S')}.{moved.microsecond // 1000:03d}Z"


REPO = Path(__file__).resolve().parent.parent
FIXTURE = REPO / "crates/tokengauge-core/tests/fixtures/cost-home"
GOLDEN = REPO / "crates/tokengauge-core/tests/fixtures/cost-golden.json"

# The fields the readers bill on. Everything else is dropped.
CLAUDE_USAGE_FIELDS = (
    "input_tokens",
    "output_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
    "cache_creation",
)
# Whole files are selected rather than loose records, so a streamed message's
# duplicates always travel together. The budget keeps the checked-in tree small;
# the model spread is what makes it worth checking in at all.
CLAUDE_BUDGET_BYTES = 320 * 1024
CODEX_BUDGET_BYTES = 400 * 1024


def compact(obj):
    return json.dumps(obj, separators=(",", ":"))


def newest(pattern):
    return sorted(Path.home().glob(pattern), key=lambda p: -p.stat().st_mtime)


def reduce_claude_file(path):
    """Billing fields only: no prompts, no output, no paths, no branch names."""
    rows, groups, models = [], {}, Counter()
    for line in path.open(errors="ignore"):
        if '"usage"' not in line:
            continue
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        message = d.get("message") or {}
        usage = message.get("usage") or {}
        if not usage or not message.get("model"):
            continue
        key = (message.get("id", ""), d.get("requestId", ""))
        groups.setdefault(key, []).append(usage.get("output_tokens", 0))
        models[message["model"]] += 1
        rows.append(
            {
                "type": d.get("type", "assistant"),
                "timestamp": d["timestamp"],
                "requestId": d.get("requestId", ""),
                "uuid": d.get("uuid", ""),
                "sessionId": d.get("sessionId", ""),
                "isSidechain": d.get("isSidechain", False),
                "version": d.get("version"),
                "message": {
                    "id": message.get("id", ""),
                    "role": "assistant",
                    "type": "message",
                    "model": message["model"],
                    "content": [],
                    "usage": {k: usage[k] for k in CLAUDE_USAGE_FIELDS if k in usage},
                },
            }
        )
    # A group whose output grows is the one that matters: keeping the first
    # record of it silently loses whatever the message went on to say, and
    # first-wins and max-wins only disagree here.
    growing = sum(1 for v in groups.values() if len(v) > 1 and max(v) > v[0])
    return rows, models, growing


def anon_claude(anon, row):
    message = row["message"]
    return {
        **row,
        "timestamp": anon.time(row["timestamp"]),
        "requestId": anon.ident("req", row.get("requestId", "")),
        "uuid": anon.ident("uuid", row.get("uuid", "")),
        "sessionId": anon.ident("sess", row.get("sessionId", "")),
        "message": {**message, "id": anon.ident("msg", message.get("id", ""))},
    }


def anon_codex(anon, row):
    payload = dict(row["payload"])
    if "id" in payload:
        payload["id"] = anon.ident("sess", payload.get("id") or "")
    if payload.get("timestamp"):
        payload["timestamp"] = anon.time(payload["timestamp"])
    return {**row, "timestamp": anon.time(row["timestamp"]), "payload": payload}


def build_claude(dest):
    """Cover every model cheaply, then spend what is left of the budget.

    Smallest-first inside each pass: a fixture is worth checking in for the
    variety of records it holds, not the volume, and one chatty transcript can
    eat the whole budget while covering a single model.
    """
    candidates = []
    # Recursive: subagent transcripts live at projects/<p>/<session>/subagents/
    # and they are where the growing streamed duplicates are.
    for path in newest(".claude/projects/**/*.jsonl")[:400]:
        rows, models, streamed = reduce_claude_file(path)
        if rows:
            size = sum(len(compact(r)) + 1 for r in rows)
            candidates.append((size, path, rows, models, streamed))
    candidates.sort(key=lambda c: c[0])

    chosen, seen_models, total_bytes, growing_total = [], set(), 0, 0

    def take(entry):
        nonlocal total_bytes, growing_total
        size, path, rows, models, growing = entry
        chosen.append((path, rows))
        seen_models.update(models)
        total_bytes += size
        growing_total += growing

    # The growing-output groups come first: they are the only thing that tells
    # a correct reader from one that keeps the first record of a group.
    for entry in candidates:
        if entry[4] and total_bytes + entry[0] <= CLAUDE_BUDGET_BYTES:
            take(entry)
    # Then one file per model, cheapest that carries it.
    for entry in candidates:
        if any(c[0] == entry[1] for c in chosen):
            continue
        if set(entry[3]) - seen_models and total_bytes + entry[0] <= CLAUDE_BUDGET_BYTES:
            take(entry)
    # Then whatever else fits, biggest first, for volume and duplicate groups.
    for entry in sorted(candidates, key=lambda c: -c[0]):
        if any(c[0] == entry[1] for c in chosen):
            continue
        if total_bytes + entry[0] <= CLAUDE_BUDGET_BYTES:
            take(entry)

    # `<synthetic>` records are Claude Code's own local messages, not billed
    # calls. They are rare enough to miss the selection by luck, and the test
    # that proves the reader drops them is worthless without one, so a few are
    # placed deliberately.
    synthetic = []
    for _, _, rows, models, _ in candidates:
        if "<synthetic>" not in models:
            continue
        synthetic.extend(r for r in rows if r["message"]["model"] == "<synthetic>")
        if len(synthetic) >= 5:
            break
    if synthetic and chosen:
        chosen[0][1].extend(synthetic[:5])

    anon = Anonymizer()
    for _, rows in chosen:
        for row in rows:
            anon.note_time(row["timestamp"])

    for index, (path, rows) in enumerate(chosen):
        # Subagent transcripts keep their nesting; the reader walks recursively
        # and a flat fixture would stop proving that.
        nested = "subagents" in path.parts
        out = dest / "projects" / f"demo-project-{index}"
        if nested:
            out = out / "session" / "subagents"
        # Real session UUIDs and agent hashes do not travel either.
        out = out / (f"agent-{index:03d}.jsonl" if nested else f"session-{index:03d}.jsonl")
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text("".join(compact(anon_claude(anon, r)) + "\n" for r in rows))

    count = sum(len(r) for _, r in chosen)
    print(f"claude: {count} records in {len(chosen)} file(s), "
          f"{growing_total} growing streamed-duplicate groups")
    print(f"        models: {', '.join(sorted(seen_models))}, "
          f"{len(synthetic[:5])} <synthetic> record(s)")
    if not growing_total:
        sys.exit("refusing to write a fixture with no growing duplicate groups")
    if len(seen_models) < 3:
        sys.exit(f"refusing to write a fixture covering only {sorted(seen_models)}")
    if not synthetic:
        sys.exit("refusing to write a fixture with no <synthetic> record in it")
    return growing_total, len(synthetic[:5])


def reduce_codex_file(path):
    rows, models = [], set()
    for line in path.open(errors="ignore"):
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind, payload = d.get("type"), d.get("payload") or {}
        if kind == "session_meta":
            rows.append({"timestamp": d["timestamp"], "type": kind, "payload": {
                "id": payload.get("id"), "timestamp": payload.get("timestamp"),
                "cwd": "/redacted", "originator": payload.get("originator"),
                "cli_version": payload.get("cli_version"), "source": payload.get("source"),
                "model_provider": payload.get("model_provider")}})
        elif kind == "turn_context":
            models.add(payload.get("model"))
            rows.append({"timestamp": d["timestamp"], "type": kind,
                         "payload": {"model": payload.get("model"), "cwd": "/redacted"}})
        elif payload.get("type") == "token_count":
            rows.append({"timestamp": d["timestamp"], "type": "event_msg",
                         "payload": {"type": "token_count", "info": payload.get("info")}})
    has_usage = any(r["type"] == "event_msg" for r in rows)
    return (rows if has_usage else []), models


def build_codex(dest):
    """Prefer sessions that switch model mid-flight - that is the trap."""
    candidates = []
    for path in newest(".codex/sessions/**/*.jsonl"):
        rows, models = reduce_codex_file(path)
        if rows:
            candidates.append((path, rows, models))

    sized = [(sum(len(compact(r)) + 1 for r in rows), path, rows, models)
             for path, rows, models in candidates]
    ordered = sorted(sized, key=lambda c: c[0])
    picked, seen = [], set()
    # Cheapest session per model first, so attribution across models is covered
    # even when one long session would otherwise fill the budget alone.
    for entry in ordered:
        if set(m for m in entry[3] if m) - seen:
            picked.append(entry)
            seen.update(m for m in entry[3] if m)
    for entry in sorted(sized, key=lambda c: -c[0]):
        if entry not in picked:
            picked.append(entry)

    selected = []
    total_bytes = 0
    for size, path, rows, models in picked:
        if total_bytes + size > CODEX_BUDGET_BYTES and selected:
            continue
        total_bytes += size
        selected.append((rows, models))

    anon = Anonymizer()
    for rows, _ in selected:
        for row in rows:
            anon.note_time(row["timestamp"])

    written, switches, seen_models = 0, 0, set()
    for index, (rows, models) in enumerate(selected):
        # Rollout filenames carry a real timestamp and session UUID; the fixture
        # keeps the directory shape and nothing else.
        target = dest / "sessions" / "2026" / "01" / "01" / f"rollout-{index:04d}.jsonl"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("".join(compact(anon_codex(anon, r)) + "\n" for r in rows))
        written += len(rows)
        switches += max(0, len(models) - 1)
        seen_models |= models
    print(f"codex:  {written} records, {switches} mid-session model switch(es)")
    print(f"        models: {', '.join(sorted(m for m in seen_models if m))}")
    # A session that switches model mid-flight is the Codex attribution trap.
    # Fail if one was available and went unpicked; only warn when the source
    # data holds none, since no selection can conjure one.
    available = any(len({m for m in models if m}) > 1 for _, _, _, models in sized)
    if available and not switches:
        sys.exit("a mid-session model switch was available and was not selected")
    if not available:
        print("        note: no session in the source data switches model "
              "mid-flight, so that path is covered by unit tests only")


def ccusage_totals(agent, env_key, root):
    env = dict(os.environ, **{env_key: str(root)})
    proc = subprocess.run(
        ["bunx", "ccusage", agent, "daily", "--since", "20250101", "--offline", "--json"],
        capture_output=True,
        env=env,
        check=True,
    )
    parsed = json.loads(proc.stdout)
    per_model = Counter()
    for day in parsed.get("daily", []):
        # The two subcommands disagree on shape: `claude` emits a list of
        # modelBreakdowns, `codex` a dict keyed by model name.
        for b in day.get("modelBreakdowns", []):
            per_model[b["modelName"]] += (
                b.get("inputTokens", 0)
                + b.get("outputTokens", 0)
                + b.get("cacheCreationTokens", 0)
                + b.get("cacheReadTokens", 0)
            )
        for name, b in (day.get("models") or {}).items():
            per_model[name] += b.get("totalTokens", 0)
    total = parsed["totals"]["totalTokens"]
    if total == 0:
        sys.exit(f"ccusage read 0 tokens for {agent} - is the fixture compact JSON?")
    return total, dict(per_model)


def main():
    if FIXTURE.exists():
        for path in sorted(FIXTURE.rglob("*"), reverse=True):
            path.unlink() if path.is_file() else path.rmdir()

    growing, synthetic = build_claude(FIXTURE / ".claude")
    build_codex(FIXTURE / ".codex")

    claude_total, claude_models = ccusage_totals(
        "claude", "CLAUDE_CONFIG_DIR", FIXTURE / ".claude"
    )
    codex_total, codex_models = ccusage_totals(
        "codex", "CODEX_HOME", FIXTURE / ".codex"
    )

    golden = {
        "_comment": (
            "ccusage's token verdict on tests/fixtures/cost-home. Regenerate with "
            "scripts/make-cost-fixture.py. Token counts only: days depend on the "
            "reader's timezone and money on whichever price table each side has."
        ),
        "generated_by": subprocess.run(
            ["bunx", "ccusage", "--version"], capture_output=True, text=True
        ).stdout.strip(),
        "traps": {
            "growing_streamed_groups": growing,
            "synthetic_records": synthetic,
        },
        "providers": {
            "claude": {"total_tokens": claude_total, "models": claude_models},
            "codex": {"total_tokens": codex_total, "models": codex_models},
        },
    }
    GOLDEN.write_text(json.dumps(golden, indent=2, sort_keys=True) + "\n")

    size = sum(p.stat().st_size for p in FIXTURE.rglob("*") if p.is_file())
    print(f"\ngolden: claude {claude_total} tokens, codex {codex_total} tokens")
    print(f"fixture: {size / 1024:.0f} KB at {FIXTURE.relative_to(REPO)}")


if __name__ == "__main__":
    main()
