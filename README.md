# fetchdoc

> Fetch invoices from Gmail, classify with AI, export to GnuCash and more — Unix-style CLI.

`fetchdoc` is a small composable CLI for receipt / invoice ingestion. Each
subcommand reads JSON Lines on stdin and writes JSON Lines on stdout, so you
build pipelines by piping them together. Designed to be **AI-callable**:
predictable I/O schemas, machine-readable output, no interactive prompts in
non-tty mode.

```sh
fetchdoc fetch gmail --since 2026-04-01 \
  | fetchdoc classify \
  | tee classified.jsonl \
  | fetchdoc export gnucash \
      --debit-account "Expenses:諸経費" \
      --credit-account "Liabilities:買掛金" \
      --out ~/finance/imports/$(date +%F).csv
```

## Status

**v0.0.x scaffold.** The subcommand surface is wired up but most subcommands
return `not implemented yet`. Implementation tracks the [v0.1 CLI MVP
milestone](https://github.com/asarikz/fetchdoc/milestones).

## Why

Japanese e-bookkeeping rules (電帳法) require receipts to be searchable by
**date / amount / counterparty**. Most existing tools either lock you into
their accounting suite or store the originals on a vendor's cloud. fetchdoc
**writes to your own filesystem** (or your own Drive, your own GnuCash book)
and never holds the originals.

The CLI uses a **bring-your-own-credentials** model: you create your own
OAuth client in your own Google Cloud project (Desktop application type),
which avoids the Google CASA Tier 2 verification that a centralised SaaS
would need to ship `gmail.readonly` access publicly.

## Install

```sh
# Cargo (any platform)
cargo install fetchdoc

# Homebrew (macOS / Linux) — coming once first release ships
brew install asarikz/tap/fetchdoc
```

Pre-built binaries for Linux, macOS, and Windows will be attached to each
GitHub Release once the v0.1 milestone closes.

## Quick start

```sh
# 1. One-time: set up an OAuth client in your own Google Cloud project
fetchdoc auth init

# 2. Authenticate against Gmail
fetchdoc auth login --source gmail

# 3. Fetch the last month of attachments
fetchdoc fetch gmail --since 2026-04-01 --limit 50 > raw.jsonl

# 4. Run them through OCR
ANTHROPIC_API_KEY=sk-ant-... fetchdoc classify < raw.jsonl > classified.jsonl

# 5. Pick exports
fetchdoc export local --root ~/受領請求書 < classified.jsonl
fetchdoc export gnucash \
  --debit-account "Expenses:諸経費" \
  --credit-account "Liabilities:買掛金" \
  --out ~/finance/imports/2026-04.csv \
  < classified.jsonl
```

## Subcommands

| Subcommand | Reads | Writes |
|---|---|---|
| `auth init / login / status / logout` | — | OS keychain |
| `fetch gmail` | — | JSONL of `Document` records |
| `classify [--ocr=anthropic\|vertex\|openai]` | JSONL | JSONL with `extracted` field |
| `export local --root PATH` | JSONL | files + JSONL with `exported` field |
| `export gnucash --out CSV` | JSONL | CSV file + JSONL |
| `verify-tnumber TXXXXXXXXXXXXX` | — | one-line summary |

## Design principles

- **Each subcommand does one thing**: fetch, classify, or export. No god command.
- **stdin / stdout = JSONL**, **stderr = human progress**. Never mix.
- **Idempotent**: re-running a fetch with the same `--since` returns the same
  records (deduped by `external_id` downstream).
- **No central server**: every credential lives on your machine, every PDF is
  written to your filesystem (or your Drive). fetchdoc keeps no state of its
  own beyond a local cache.
- **AI-callable**: predictable subcommand naming, schemas declared in code,
  no interactive prompts when stdin is not a TTY.

## License

Dual licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Following the Rust project's convention.
