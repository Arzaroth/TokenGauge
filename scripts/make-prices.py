#!/usr/bin/env python3
"""Regenerate the vendored price tables in crates/tokengauge-core/src/cost/.

Writes two files:

  prices.json         the current table, sliced to the models TokenGauge can
                      attribute. What every rating falls back to.
  price-archive.json  per-month overrides for models whose price differed from
                      the current table, so history is rated at the prices that
                      were in effect rather than at today's.

The vendored copy is what a cold or offline machine rates against, so it has to
be sliced by the same rule `PriceTable::from_json` applies at runtime: keep an
entry when its key walks down to a provider TokenGauge tracks. Mirror any change
to `attribute_price_key` here, or a machine that has never reached LiteLLM will
price a model the connected ones do.

    ./scripts/make-prices.py

Set GITHUB_TOKEN to raise the API rate limit if the archive walk starts failing.
"""

import datetime
import json
import os
import pathlib
import sys
import urllib.request

RAW = "https://raw.githubusercontent.com/BerriAI/litellm/{ref}/model_prices_and_context_window.json"
COMMITS = (
    "https://api.github.com/repos/BerriAI/litellm/commits"
    "?path=model_prices_and_context_window.json&until={until}T00:00:00Z&per_page=1"
)
ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "crates/tokengauge-core/src/cost/prices.json"
ARCHIVE_OUT = ROOT / "crates/tokengauge-core/src/cost/price-archive.json"

# Months of overrides to carry. `fleet::STORE_RETENTION_DAYS` is 400, so this
# has to reach at least that far back or the oldest history in a store has no
# archive entry and falls back to today's prices.
ARCHIVE_MONTHS = 14

# The five fields `ModelPrice` reads, under LiteLLM's own names.
FIELDS = (
    "input_cost_per_token",
    "output_cost_per_token",
    "cache_creation_input_token_cost",
    "cache_creation_input_token_cost_above_1hr",
    "cache_read_input_token_cost",
)

PREFIXES = (
    ("claude", "claude"),
    ("gpt", "codex"),
    ("o1", "codex"),
    ("o3", "codex"),
    ("o4", "codex"),
    ("codex", "codex"),
    ("openai", "codex"),
    ("kimi", "kimi"),
    ("moonshot", "kimi"),
    ("grok", "grok"),
    ("glm", "glm"),
)


def model_to_provider(name):
    """`ccusage::model_to_provider`."""
    for prefix, provider in PREFIXES:
        if name.startswith(prefix):
            return provider
    return None


VENDOR_PREFIXES = {
    "glm": ("zai/", "z-ai/", "openrouter/z-ai/"),
    "grok": ("xai/", "x-ai/", "openrouter/x-ai/"),
    "kimi": ("moonshot/", "moonshotai/", "openrouter/moonshotai/"),
}


def attribute(key):
    """`pricing::attribute_price_key`."""
    provider = model_to_provider(key)
    if provider:
        return provider
    bare = key.rsplit("/", 1)[-1]
    provider = model_to_provider(bare)
    if not provider:
        return None
    prefixes = VENDOR_PREFIXES.get(provider, ())
    return provider if any(key == prefix + bare for prefix in prefixes) else None


def get_json(url):
    request = urllib.request.Request(url)
    token = os.environ.get("GITHUB_TOKEN")
    if token and url.startswith("https://api.github.com"):
        request.add_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def slice_table(table):
    """The models TokenGauge can attribute, priced, as `from_json` keeps them."""
    kept = {}
    for name, entry in table.items():
        lower = name.lower()
        if attribute(lower) is None or not isinstance(entry, dict):
            continue
        priced = {f: entry[f] for f in FIELDS if isinstance(entry.get(f), (int, float))}
        # `from_json` drops an entry that prices neither input nor output.
        if priced.get("input_cost_per_token", 0) <= 0 and (
            priced.get("output_cost_per_token", 0) <= 0
        ):
            continue
        kept[lower] = dict(sorted(priced.items()))
    return dict(sorted(kept.items()))


def month_starts(count):
    """The first of each of the `count` months before this one, oldest first."""
    cursor = datetime.date.today().replace(day=1)
    out = []
    for _ in range(count):
        cursor = (cursor - datetime.timedelta(days=1)).replace(day=1)
        out.append(cursor)
    return list(reversed(out))


def next_month(date):
    return (date.replace(day=28) + datetime.timedelta(days=8)).replace(day=1)


def build_archive(current):
    """Per-month overrides for models whose price was not what it is today.

    A month is represented by the table at its **end**, which is the one most
    likely to carry the model at all. A price that moved mid-month is therefore
    attributed to the whole of it: monthly is the granularity, and a bucket
    older than the hourly retention has no finer date to be rated by anyway.
    """
    archive = {}
    for start in month_starts(ARCHIVE_MONTHS):
        label = start.strftime("%Y-%m")
        until = next_month(start).isoformat()
        commits = get_json(COMMITS.format(until=until))
        if not commits:
            print(f"  {label}: no commit found, skipped", file=sys.stderr)
            continue
        table = slice_table(get_json(RAW.format(ref=commits[0]["sha"])))
        overrides = {
            name: price for name, price in table.items() if current.get(name) != price
        }
        if overrides:
            archive[label] = overrides
        print(
            f"  {label}: {len(table):4} models, {len(overrides):3} overrides",
            file=sys.stderr,
        )
    return archive


def main():
    current = slice_table(get_json(RAW.format(ref="main")))

    for provider in ("claude", "codex", "kimi", "grok", "glm"):
        count = sum(1 for k in current if attribute(k) == provider)
        print(f"{provider:8} {count:5} entries", file=sys.stderr)
        if count == 0:
            sys.exit(f"no {provider} models survived the filter - check attribute()")

    OUT.write_text(json.dumps(current, indent=1) + "\n")
    print(f"wrote {len(current)} entries to {OUT}", file=sys.stderr)

    print(f"building {ARCHIVE_MONTHS} months of price history", file=sys.stderr)
    archive = build_archive(current)
    ARCHIVE_OUT.write_text(json.dumps(archive, indent=1, sort_keys=True) + "\n")
    total = sum(len(v) for v in archive.values())
    size = ARCHIVE_OUT.stat().st_size
    print(
        f"wrote {total} overrides across {len(archive)} months "
        f"to {ARCHIVE_OUT} ({size / 1024:.1f} KiB)",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
