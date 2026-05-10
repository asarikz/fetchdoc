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

**v0.0.x — pre-release.** Working today: `auth` (init/login/status/logout),
`import csv`/`xlsx`/`dedup`, `classify --ocr=anthropic`, `export local`,
`export gnucash` (Document and Transaction), `verify-tnumber` (regex only).
Still landing for v0.1: `fetch gmail` itself, NTA T-number API verification,
`classify --ocr=vertex|openai`. See the
[v0.1 CLI MVP milestone](https://github.com/asarikz/fetchdoc/milestones).

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

## Local mail import (no OAuth)

If you'd rather not — or can't — set up a Google Cloud OAuth client, point
fetchdoc at a folder of `.eml` files instead. Apple Mail, Thunderbird,
Outlook, Google Takeout, and `offlineimap` / `mbsync` can all produce these.

```sh
# Drop a few .eml files into ~/mail-export/ first.
fetchdoc fetch eml --dir ~/mail-export --since 2026-04-01 > raw.jsonl
ANTHROPIC_API_KEY=sk-ant-... fetchdoc classify < raw.jsonl > classified.jsonl
fetchdoc export local --root ~/受領請求書 < classified.jsonl
```

`fetch eml` recurses into subdirectories, extracts every PDF attachment into
a cache directory, and emits the same Document JSONL that `fetch gmail`
will produce — so the rest of the pipeline is identical.

## Setting up Gmail access

fetchdoc reads Gmail through your **own** Google Cloud OAuth client (BYO-credentials), so there is a one-time setup. Plan ~10 minutes the first time.

> **Use a dedicated GCP project for fetchdoc** (e.g. `fetchdoc-personal`).
> The OAuth consent screen and quotas are per-project, so a dedicated
> project keeps `gmail.readonly` isolated from your other apps and lets you
> revoke everything by deleting one project later. Reusing an existing
> project would change *its* consent screen for any other OAuth clients in
> it. For multiple Gmail accounts, create one project per account.

1. **Create the project**: https://console.cloud.google.com/projectcreate → name it `fetchdoc-personal` (no organisation needed for personal Gmail) → **Create**, then select it in the project picker.
2. **Enable Gmail API**: https://console.cloud.google.com/apis/library/gmail.googleapis.com → **Enable**. (Or `gcloud services enable gmail.googleapis.com`.)
3. **Configure the OAuth consent screen**: https://console.cloud.google.com/apis/credentials/consent
   - **User Type**: `External` for personal Gmail (`Internal` for Workspace-only).
   - Fill **App name** (`fetchdoc`), **User support email**, **Developer contact email**.
   - **Scopes** → **Add or remove scopes** → tick `https://www.googleapis.com/auth/gmail.readonly`.
   - **Test users** → add the Gmail address you want to read from.
   - **Leave the publishing status as `Testing`.** Do *not* click *Publish App* — `Testing` is what keeps you out of [CASA Tier 2 verification](https://support.google.com/cloud/answer/13463073). The trade-off is that refresh tokens expire after 7 days; just re-run `auth login` when that happens.
4. **Create the OAuth client**: https://console.cloud.google.com/apis/credentials → **+ Create Credentials** → **OAuth client ID** → **Application type: Desktop app** (← required; *not* Web application — fetchdoc uses a loopback redirect) → **Create** → **Download JSON**.
5. **Hand the JSON to fetchdoc**:

   ```sh
   fetchdoc auth init --from ~/Downloads/client_secret_xxx.apps.googleusercontent.com.json
   ```

   This copies the file into `~/.config/fetchdoc/` and validates that it is a Desktop client. The original download can be deleted.

6. **Log in**:

   ```sh
   fetchdoc auth login --source gmail
   ```

   Browser opens → choose the Gmail account you added as a test user → click **Advanced** → **Go to fetchdoc (unsafe)** on the unverified-app warning (it is your own client; the warning is just because the project is in `Testing`) → **Allow**. The refresh token is stored in your OS keychain (macOS Keychain / Windows Credential Manager / Linux Secret Service).

Verify with `fetchdoc auth status`. If something looks off, the [client_secret.json troubleshooting](#troubleshooting-auth) section below has the common pitfalls.

## Quick start

```sh
# 1. One-time: set up an OAuth client (see "Setting up Gmail access" above)
fetchdoc auth init --from ~/Downloads/client_secret_xxx.apps.googleusercontent.com.json

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
| `fetch eml --dir PATH` | — | JSONL of `Document` records (no OAuth) |
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

## Troubleshooting auth

| Symptom | Cause / fix |
|---|---|
| `Web-application OAuth client; fetchdoc requires a Desktop` | Step 4 picked *Web application*. In **Credentials**, delete that client and create a new one with **Application type: Desktop app**. |
| `Access blocked: ... has not completed the Google verification process` | The Gmail account you signed in with is not on the OAuth consent screen's **Test users** list — or you signed in with a different account. |
| `Google returned no refresh_token` | A previous consent is still active. Visit https://myaccount.google.com/permissions, **Remove access** for fetchdoc, and re-run `fetchdoc auth login`. |
| `invalid_grant` when fetching, ~7 days after login | `Testing`-mode refresh tokens expire after 7 days. Re-run `fetchdoc auth login --source gmail`. To remove this expiry you would have to publish the app and go through Google verification (not recommended for personal use). |

## License

Dual licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Following the Rust project's convention.
