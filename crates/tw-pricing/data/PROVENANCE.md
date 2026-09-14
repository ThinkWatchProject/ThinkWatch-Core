# Where the bundled price table comes from

| | |
|---|---|
| Source | https://github.com/BerriAI/litellm — `model_prices_and_context_window.json` (the copy at the repository root) |
| Commit | `ee7c7e14f3dd7c4c3930a423440ec26427e2c554` |
| Snapshot date | 2026-09-09 |
| License | MIT (**the root copy**. The exceptions under `enterprise/` do not apply to it) |
| Raw size | 2343859 bytes, 110309 gzipped |
| Models | 3860 |

## Why this source

The field that decided it (DESIGN.md §4.3.0):

```
cache_creation_input_token_cost_above_1hr
```

**It is the only public dataset that encodes Anthropic's 1-hour cache write
price.** Every other candidate carries the 5-minute tier alone — Sonnet 4.5 is
$3.75 at 5 minutes and $6.00 at 1 hour, so anyone running a 1-hour TTL is
**systematically underestimated by 60%**. An underestimate is worse than not
knowing: it arrives looking like a definite number.

## How to update it

**Pin a specific commit SHA, never `main`.** Following `main` means two builds
can pick up different prices, and "yesterday's figure does not match today's"
is not something anyone can explain.

```bash
# find the latest commit
curl -s "https://api.github.com/repos/BerriAI/litellm/commits?path=model_prices_and_context_window.json&per_page=1"
# fetch that revision and compress it
curl -sL "https://raw.githubusercontent.com/BerriAI/litellm/<SHA>/model_prices_and_context_window.json" \
  | gzip -9 > crates/tw-pricing/data/model_prices.json.gz
```

Then **update the SHA and the date in this file** — the freshness workflow
reads both straight out of the table above — and run the two-source
cross-check before release (§4.3.0): for the dozen or so models people
actually use, compare line by line against **the vendor's own pricing page**.
Not against a second third-party dataset; that only moves the bet somewhere
else.

One measured example. OpenAI cut the price of a model on 2026-08-22, and a
widely used dataset **did not correct it for three days**, returning a figure
25% too high the whole time. The cause was that their sync script automates
the model list but not the prices, which arrive by hand-written PR.
