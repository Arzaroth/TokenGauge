#!/usr/bin/env python3
"""Regenerate the vendored price table in crates/tokengauge-core/src/cost/prices.json.

The vendored copy is what a cold or offline machine rates against, so it has to
be sliced by the same rule `PriceTable::from_json` applies at runtime: keep an
entry when its key walks down to a provider TokenGauge tracks. Mirror any change
to `attribute_price_key` here, or a machine that has never reached LiteLLM will
price a model the connected ones do.

    ./scripts/make-prices.py
"""

import json
import pathlib
import sys
import urllib.request

URL = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
OUT = (
    pathlib.Path(__file__).resolve().parent.parent
    / "crates/tokengauge-core/src/cost/prices.json"
)

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


def main():
    with urllib.request.urlopen(URL, timeout=60) as response:
        table = json.load(response)

    kept = {}
    for name, entry in table.items():
        lower = name.lower()
        if attribute(lower) is None:
            continue
        if not isinstance(entry, dict):
            continue
        priced = {f: entry[f] for f in FIELDS if isinstance(entry.get(f), (int, float))}
        # `from_json` drops an entry that prices neither input nor output.
        if priced.get("input_cost_per_token", 0) <= 0 and (
            priced.get("output_cost_per_token", 0) <= 0
        ):
            continue
        kept[lower] = dict(sorted(priced.items()))

    for provider in ("claude", "codex", "kimi", "grok", "glm"):
        count = sum(1 for k in kept if attribute(k) == provider)
        print(f"{provider:8} {count:5} entries", file=sys.stderr)
        if count == 0:
            sys.exit(f"no {provider} models survived the filter - check attribute()")

    OUT.write_text(json.dumps(dict(sorted(kept.items())), indent=1) + "\n")
    print(f"wrote {len(kept)} entries to {OUT}", file=sys.stderr)


if __name__ == "__main__":
    main()
