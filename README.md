# codexusage

See how much your OpenAI Codex sessions cost - broken down by day, month, or session - right from your terminal.

Inspired by [`@ccusage/codex`](https://github.com/kevinlu1248/ccusage), `codexusage` is a ground-up Rust rewrite built for users who need a significantly faster and lighter alternative to scan large session histories.

## Install

```bash
cargo install codexusage
```

## Quick start

```bash
# Daily usage summary
codexusage daily

# Monthly report as JSON
codexusage monthly --json

# Last 7 days only
codexusage daily --last-days 7

# Custom session directory
codexusage session --session-dir /path/to/sessions
```

## Examples

Filter by date range:

```bash
codexusage daily --since 2026-03-01 --until 2026-03-31
```

Skip network and use cached pricing:

```bash
codexusage daily --offline
```

JSON output for scripts and dashboards:

```bash
codexusage daily --json
```

## Watch mode

Monitor today's usage live in your terminal. The screen refreshes automatically and shows a rolling burn rate so you can see how fast tokens are being consumed:

```bash
codexusage watch
```

Custom refresh interval (default 5 seconds):

```bash
codexusage watch --interval 10
```

Sample output:

```text
Current Day Codex Usage Watch
Date: 2026-03-11  Window: 60 minutes

+-----------+------------+----------------+
| Metric    | Today      | Burn Rate (/h) |
+-----------+------------+----------------+
| Input     |       450K |            56K |
| Cache     |       120K |            15K |
| Output    |        30K |             4K |
| Reasoning |        12K |             2K |
| Total     |       612K |            77K |
| Cost      |      $0.85 |          $0.11 |
| Updated   | 2026-03-11 |       14:32:23 |
+-----------+------------+----------------+
```

Watch mode does not support `--json`, `--since`, `--until`, or `--last-days`.

## Options

Commands:

- `daily` - group usage by day
- `monthly` - group usage by month
- `session` - group usage by session
- `watch` - live current-day monitor with burn rate

Flags:

- `--json` - emit structured JSON instead of a table
- `--since` / `--until` - inclusive date filters
- `--last-days N` / `-L N` - show the last N calendar days (daily only)
- `--timezone` - IANA timezone for grouping
- `--offline` - use cached pricing, never hit the network
- `--refresh-pricing` - force a pricing refresh
- `--session-dir` - override session directory (repeatable)
- `--threads N` - scanner worker count
- `--number-format full` - show full token counts instead of K/M/B/T

`--last-days` cannot be combined with `--since` or `--until`.

By default, `codexusage` scans `CODEX_HOME/sessions` (or `~/.codex/sessions` when `CODEX_HOME` is not set).

## Sample output

```text
Daily Codex Usage Report

+------------+--------------+-------+-------+--------+-----------+-------+-------+
| Date       | Model        | Input | Cache | Output | Reasoning | Total |  Cost |
+------------+--------------+-------+-------+--------+-----------+-------+-------+
| 2026-03-01 | TOTAL        |  120K |   30K |     8K |        4K |  158K | $0.20 |
|            |   gpt-5      |  120K |   30K |     8K |        4K |  158K | $0.20 |
+------------+--------------+-------+-------+--------+-----------+-------+-------+
| 2026-03-02 | TOTAL        |   90K |   20K |     6K |        2K |  118K | $0.03 |
|            |   gpt-5-mini |   90K |   20K |     6K |        2K |  118K | $0.03 |
|            | GRAND TOTAL  |  210K |   50K |    14K |        6K |  276K | $0.23 |
+------------+--------------+-------+-------+--------+-----------+-------+-------+
```

## Pricing

Pricing data is cached locally and refreshed automatically when stale. Use `--offline` to skip network access entirely, or `--refresh-pricing` to force an update.

## Performance

`codexusage` was built because `@ccusage/codex` became unusably slow on large session directories. The table below shows a side-by-side comparison on the same dataset.

**Test environment:** 4.4 GB across 7726 session files, Intel i7-1185G7 (8 logical CPUs).

| Tool | Wall time | Peak RSS |
| --- | ---: | ---: |
| `codexusage` | 1.82 s | 193 MB |
| `@ccusage/codex` | 5 min 36 s | 11,547 MB |

Notes:

- `@ccusage/codex` was already cached by Bun; the one-time download is not counted.
- Results reflect this specific machine and dataset, not a universal guarantee.

## Development

Build from source:

```bash
cargo build --release
```

Quality commands:

```bash
just fmt
just clippy
just test
just bench
```
