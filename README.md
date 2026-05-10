# fetchdoc

> Fetch invoices from Gmail, classify with AI, export to GnuCash and more — Unix-style CLI.

`fetchdoc` is a small composable CLI for receipt / invoice ingestion and
bank-statement bookkeeping. Each subcommand reads JSON Lines on stdin and
writes JSON Lines on stdout, so you build pipelines by piping them together.
Designed to be **AI-callable**: predictable I/O schemas, machine-readable
output, no interactive prompts in non-tty mode.

```sh
# Invoice pipeline (Gmail → classify → file + GnuCash)
fetchdoc fetch gmail --since 2026-04-01 \
  | fetchdoc render-body \
  | fetchdoc classify \
  | fetchdoc export local --root ~/受領請求書 \
  | fetchdoc export gnucash \
      --debit-account "Expenses:諸経費" \
      --credit-account "Liabilities:買掛金" \
      --out ~/finance/imports/$(date +%F).csv

# Bank-statement pipeline (CSV/xlsx → Transaction JSONL → GnuCash)
fetchdoc import csv --profile smbc statement.csv \
  | fetchdoc import dedup --against ~/finance/smbc.all.jsonl \
  | tee -a ~/finance/smbc.all.jsonl \
  | fetchdoc export gnucash \
      --account "Assets:Bank:SMBC" \
      --out ~/finance/imports/smbc-$(date +%F).csv
```

## Status

**v0.0.1 — pre-release.** What works today:

| Area | Status |
|---|---|
| `auth init / login / status / logout` (Gmail) | ✅ |
| `fetch gmail` (Gmail API, BYO-credentials) | ✅ |
| `fetch eml` / `fetch mbox` / `fetch maildir` | ✅ |
| `fetch dir` (manually-downloaded PDFs from Amazon / ヨドバシ / NUROモバイル …) | ✅ |
| `render-body` (chromium / weasyprint / wkhtmltopdf) | ✅ |
| `classify --ocr=anthropic` | ✅ |
| `classify --ocr=vertex` / `--ocr=openai` | ❌ not implemented |
| `import csv` / `import xlsx` (profile-driven, `--infer`) | ✅ |
| `import csv --dir` (two-file SBI Sumishin-style join) | ✅ |
| `import dedup` | ✅ |
| `export local` (filename template) | ✅ |
| `export gnucash` (Document and Transaction) | ✅ |
| `verify-tnumber` | ⚠️ regex-only; NTA API not yet wired (issue #26) |

See the [v0.1 CLI MVP milestone](https://github.com/asarikz/fetchdoc/milestones)
for what's still landing.

## Why

Japanese e-bookkeeping rules (電帳法) require receipts to be searchable by
**date / amount / counterparty**. Most existing tools either lock you into
their accounting suite or store the originals on a vendor's cloud. fetchdoc
**writes to your own filesystem** (or your own GnuCash book) and never
holds the originals.

The CLI uses a **bring-your-own-credentials** model: you create your own
OAuth client in your own Google Cloud project (Desktop application type),
which avoids the Google CASA Tier 2 verification that a centralised SaaS
would need to ship `gmail.readonly` access publicly.

## Install

```sh
# Cargo (any platform — requires Rust 1.85+)
cargo install fetchdoc

# Homebrew (macOS / Linux) — coming once first release ships
brew install asarikz/tap/fetchdoc
```

Pre-built binaries for Linux, macOS, and Windows will be attached to each
GitHub Release once the v0.1 milestone closes.

## Two pipelines

fetchdoc has two record shapes flowing through stdin/stdout JSONL:

- **`Document`** — invoice / receipt PDFs. Produced by `fetch ...`,
  enriched by `classify`, written by `export local` / `export gnucash`.
- **`Transaction`** — one bank or credit-card statement line item.
  Produced by `import csv` / `import xlsx`, optionally enriched by
  `classify`, written by `export gnucash`.

Both records accumulate fields as they pass through subcommands; later
stages add `extracted` / `exported` rather than mutating earlier fields.

---

## Pipeline 1: Invoices and receipts (PDFs → archive + accrual)

### A. Fetching the PDFs

Pick **one** source. The output JSONL is identical regardless of source, so
the rest of the pipeline does not change.

#### Gmail (BYO-credentials)

```sh
fetchdoc fetch gmail --since 2026-04-01 --limit 50 > raw.jsonl
```

Hits the Gmail API with your own OAuth client (set up via `auth init` /
`auth login` — see [Setting up Gmail access](#setting-up-gmail-access)
below). For each matched message, the raw RFC 822 bytes are written to
`<cache_dir>/<messageId>.eml` and the parsed message is handed to the same
mail processor that drives `fetch eml` / `fetch mbox` / `fetch maildir` —
so PDF attachments and body-primary records share one code path.

| Flag | Default | Purpose |
|---|---|---|
| `--since YYYY-MM-DD` | none | Server-side filter (`after:` query) plus a `Date:` recheck per message |
| `--query Q` | `has:attachment filename:pdf` | Gmail search syntax (`OR`, `from:`, `subject:`, etc.) |
| `--limit N` | none | Stop after N records |
| `--cache-dir PATH` | `<os-cache>/fetchdoc/gmail-attachments/` | Where `.eml` and PDFs land |
| `--quiet` | off | Suppress per-message stderr progress |

Each emitted record carries `source_meta.gmail_message_id` /
`gmail_thread_id` / `eml_path`, so re-classification works without
re-hitting Gmail.

**Refresh-token expiry:** while your Cloud project is in `Testing`,
Google rotates refresh tokens after **7 days**. When you hit
`invalid_grant`, just re-run `fetchdoc auth login --source gmail`.

#### Local mail (no OAuth)

If you'd rather not — or can't — set up a Google Cloud OAuth client, point
fetchdoc at local mail files instead. Apple Mail, Thunderbird, Outlook,
Google Takeout, and `offlineimap` / `mbsync` can all produce these.

```sh
# A folder of individual .eml files (Thunderbird "Save As", drag-out from Mail).
fetchdoc fetch eml --dir ~/mail-export --since 2026-04-01 > raw.jsonl

# An mbox archive — Apple Mail's "Save Mailbox", Thunderbird's per-folder
# files, Google Takeout's `All mail.mbox`, mbsync mboxrd output.
fetchdoc fetch mbox --file ~/Takeout/Mail/All\ mail.mbox --since 2026-04-01 > raw.jsonl
fetchdoc fetch mbox --dir  ~/Library/Mail/V10                                > raw.jsonl

# A Maildir / Maildir++ tree — offlineimap, mbsync default layout, mu/notmuch.
fetchdoc fetch maildir --dir ~/Maildir --since 2026-04-01 > raw.jsonl
```

#### Manually-downloaded PDFs (`fetch dir`)

Some receipts arrive *outside* email — Amazon の「領収書」, ヨドバシカメラ
の領収書 PDF, NURO モバイル / 楽天モバイル / SaaS ポータルの請求書 etc.
Download them by hand into a watched folder and let `fetch dir` ingest them
into the same `Document` JSONL the mail sources emit. The rest of the
pipeline is unchanged.

```sh
# Drop downloaded PDFs into ~/Inbox/receipts and run:
fetchdoc fetch dir --dir ~/Inbox/receipts \
    --move-to ~/.cache/fetchdoc/dir-archive \
    --since 2026-04-01 \
  > raw.jsonl
```

| Flag | Default | Purpose |
|---|---|---|
| `--dir PATH` | required | Folder to scan recursively |
| `--include-ext EXT` | `pdf` | File extension (case-insensitive). Repeat for multiple (e.g. `--include-ext pdf --include-ext png`) |
| `--since YYYY-MM-DD` | none | Filter by mtime (local-midnight cutoff) |
| `--limit N` | none | Stop after N records |
| `--move-to PATH` | none | After ingestion, move the file to `<PATH>/<sha256>.<ext>`. **Idempotent**: if the destination already holds a file with that hash, the source is left alone and *not* re-emitted. Lets you safely re-run on the same inbox folder. |
| `--quiet` | off | Suppress per-file stderr progress |

`external_id` is `sha256:<hex64>` of the file contents — re-running on the
same file produces the same id, so downstream `import dedup` (or your own
filter on `external_id`) handles duplicates cleanly. `source_meta` carries
`original_path`, `mtime`, and `file_size`.

**Tips for the inbox workflow**
- Browser tip: most browsers let you set a per-site download folder via an
  extension. Point Amazon / ヨドバシ / NUROモバイル at `~/Inbox/receipts`
  and the only manual step is clicking "領収書を保存".
- Pair with a launchd / systemd timer to run `fetchdoc fetch dir … |
  fetchdoc classify | fetchdoc export local …` on a schedule and your
  inbox empties itself.
- For sites where the "receipt" is an HTML page (Amazon's order-history
  page has no PDF button), use the browser's print → "Save as PDF" once
  per order.

All three subcommands share the same flags:

| Flag | Default | Purpose |
|---|---|---|
| `--dir PATH` / `--file PATH` | required | What to scan |
| `--since YYYY-MM-DD` | none | Filter by `Date:` header |
| `--limit N` | none | Stop after N records |
| `--cache-dir PATH` | `<os-cache>/fetchdoc/{eml,mbox,maildir}-attachments/` | Where extracted PDFs land |
| `--quiet` | off | Suppress per-file stderr progress |

Each record carries `attachment_path` to the cached PDF and `source_meta`
with subject / from / date / `eml_path` so later stages can re-parse the
original message if needed.

### B. Body-primary receipts (`render-body`)

Some receipts (Stripe / AWS / many SaaS billing notices) ship the invoice
**as the email body itself** with no PDF attached. For those, the `fetch`
sources emit a *body-primary* record (no `attachment_path`, but
`source_meta.body_is_primary == true` plus an `eml_path` pointing at the
underlying `.eml`). Run `fetchdoc render-body` between `fetch` and
`classify` to render those bodies to PDF for 電帳法 archival:

```sh
fetchdoc fetch eml --dir ~/mail-export \
  | fetchdoc render-body \
  | fetchdoc classify \
  | fetchdoc export local --root ~/受領請求書
```

`render-body` shells out to whichever HTML→PDF tool is on your `$PATH` —
checked in order: `chromium` / `chromium-browser` / `google-chrome` / `chrome`
(recommended), `weasyprint`, `wkhtmltopdf` (deprecated, last resort). Pin a
specific renderer with `--renderer chromium|weasyprint|wkhtmltopdf|auto`
(default `auto`). `--cache-dir PATH` overrides where rendered PDFs land
(default `<os-cache>/fetchdoc/body-pdfs/`).

Records that already have an `attachment_path` are passed through untouched,
so it's safe to keep `render-body` in every pipeline.

### C. Classification (`classify`)

```sh
ANTHROPIC_API_KEY=sk-ant-... fetchdoc classify < raw.jsonl > classified.jsonl
```

For each input Document, `classify` opens the cached PDF, sends it to the
Anthropic Messages API as a `document` block, and asks Claude to return the
qualified-invoice fields:

```jsonc
{
  "transaction_date": "2026-04-30",          // ISO 8601 — converts 令和 dates
  "total_amount_jpy": 12100,                 // 税込合計, integer JPY
  "counterparty_name": "アクメ株式会社",
  "counterparty_t_number": "T1234567890123", // null if not printed
  "confidence": 0.94
}
```

Flags:

| Flag | Default | Purpose |
|---|---|---|
| `--ocr=anthropic\|vertex\|openai` | `anthropic` | Backend (only `anthropic` is wired today) |
| `--model NAME` | backend default | Override the model name |

Per-PDF size cap is 16 MB (Anthropic's document-block limit is ~32 MB; we stop
well before to avoid runaway batches). Failed extractions flip `status` to
`needs_review` and pass through; the run is never aborted on a single bad PDF.

### D. Export

#### `export local` — write the PDF with a 電帳法-friendly filename

```sh
fetchdoc export local --root ~/受領請求書 < classified.jsonl
```

Default filename template is `{yyyy-mm-dd}_{counterparty_name}_{total_amount}円.pdf`
— a single directory listing already meets the 電帳法 search-by-three-fields
requirement. Available placeholders: `{yyyy-mm-dd}` `{yyyy}` `{mm}` `{dd}`
`{counterparty_name}` `{total_amount}` `{external_id}` `{source}`.
Use `/` in `--name-template` to fan into subdirectories
(e.g. `{yyyy}/{mm}/{yyyy-mm-dd}_...pdf`).

Records without `attachment_path` or `extracted` are passed through with
`status = needs_review` and a stderr warning (never aborts).

#### `export gnucash` — emit the GnuCash CSV importer format

```sh
fetchdoc export gnucash \
  --debit-account "Expenses:諸経費" \
  --credit-account "Liabilities:買掛金" \
  --out ~/finance/imports/2026-04.csv \
  < classified.jsonl
```

For `Document` input it emits the classical accrual A/P pair (debit expense,
credit payable). For `Transaction` input (see Pipeline 2) you pass `--account`
instead — see below.

---

## Pipeline 2: Bank / card statement bookkeeping

This pipeline operates on `Transaction` records (one row = one statement line)
and feeds them into GnuCash.

### A. Importing the statement (`import csv` / `import xlsx`)

Import is **profile-driven**: a small TOML file tells fetchdoc the encoding,
delimiter, header row, date format, and which column maps to which schema
field. Profiles live in `~/.config/fetchdoc/profiles/<name>.toml` and are
human-editable.

```sh
# Single CSV file with a saved profile.
fetchdoc import csv --profile smbc statement.csv > tx.jsonl

# Same idea for .xlsx workbooks (pick a sheet with --sheet "明細" or "0").
fetchdoc import xlsx --profile mufg --sheet "明細" book.xlsx > tx.jsonl

# stdin works too (UTF-8 only):
gunzip -c last-month.csv.gz | fetchdoc import csv --profile smbc - > tx.jsonl
```

#### Auto-generating a profile (`--infer`)

When you don't yet have a profile, hand the first ~50 lines to Anthropic
and have it generate one:

```sh
ANTHROPIC_API_KEY=sk-ant-... \
  fetchdoc import csv --infer --name smbc statement.csv > tx.jsonl
```

The generated TOML is saved to `~/.config/fetchdoc/profiles/smbc.toml` and
used to parse the file in the same run; subsequent runs can drop `--infer`.
**Per-row data is never sent**; only the head of the file goes to the LLM.

`--name` defaults to the input file's stem if omitted. xlsx works the
same way (`fetchdoc import xlsx --infer book.xlsx --sheet 0`).

#### Profile shape

Minimal, signed-amount style:

```toml
name = "rakuten-card"
encoding = "utf-8"
date_format = "%Y/%m/%d"

[columns]
posted_date = "利用日"
description = "利用店名・商品名"
amount      = "利用金額"           # signed: negative = outflow
```

Japanese-bank style, with separate withdrawal/deposit columns and Shift_JIS:

```toml
name = "smbc"
encoding = "shift_jis"
date_format = "%Y/%m/%d"

[columns]
posted_date = "年月日"
description = "お取り扱い内容"
withdrawal  = "お支払金額"
deposit     = "お預り金額"
balance     = "差引残高"
```

See [`examples/profiles/`](examples/profiles) for ready-to-copy SMBC, GMO
あおぞら and SBI Sumishin profiles. Sign convention: `amount_jpy` in the
output JSONL is **signed** — outflows negative, inflows positive — which
matches GnuCash's transfer semantics.

#### Two-file imports (`import csv --dir`)

Some banks (e.g. SBI Sumishin) ship the statement in two files: a header
file (the bank-account ledger) and a separate detail file (per-debit-card
purchase splits with FX rate, fee, merchant, etc.). A profile with a
`[multi]` section joins them by date ± window and amount, producing one
GnuCash multi-split transaction per primary row:

```sh
# All CSVs (statement + debit detail) live in one directory.
fetchdoc import csv --profile sbi-sumishin --dir ~/Downloads/sbi/ > tx.jsonl
```

The resulting `Transaction.splits` field carries principal / 海外事務手数料 /
ATM fee / per-tx fee on individually configurable accounts. See
[`examples/profiles/sbi-sumishin.toml`](examples/profiles/sbi-sumishin.toml).

#### Built-in normalisation

Each profile can opt into text normalisation. Default ON:

- `[normalize] halfwidth_kana = true` — folds half-width katakana
  (`ｱｸﾒ`→`アクメ`, `ｶﾞ`→`ガ`, `ﾊﾟ`→`パ`). Result is written to
  `description_normalized`; the original `description_raw` is never modified.

### B. Idempotent re-import (`import dedup`)

Bank web UIs typically let you re-download "the last 90 days" rather than
"only what's new", so an idempotent re-import flow is essential.

```sh
fetchdoc import csv --profile smbc statement.csv \
  | fetchdoc import dedup --against ~/finance/smbc.all.jsonl \
  | tee -a ~/finance/smbc.all.jsonl \
  | fetchdoc export gnucash --account "Assets:Bank:SMBC" \
      --out ~/finance/imports/smbc-$(date +%F).csv
```

`external_id` is a stable hash of `profile + posted_date + amount + description`,
so the same statement line emits the same id no matter how many times you
re-import. `--against` may be repeated to merge several histories.

### C. Export to GnuCash (`export gnucash`)

```sh
fetchdoc export gnucash \
  --account "Assets:Bank:SMBC" \
  --default-other "Imbalance-JPY" \
  --out ~/finance/imports/smbc-2026-04.csv \
  < tx.jsonl
```

| Flag | Applies to | Purpose |
|---|---|---|
| `--out PATH` | both | Output CSV (required; stdout is the JSONL passthrough) |
| `--account` | Transaction | Bank account that *is* this statement (e.g. `Assets:Bank:SMBC`) |
| `--default-other` | Transaction | Offset account when category is unknown (default `Imbalance-JPY`) |
| `--debit-account` | Document | Expense side of an invoice (e.g. `Expenses:諸経費`) |
| `--credit-account` | Document | Payable / cash side (e.g. `Liabilities:買掛金`) |
| `--currency` | both | Commodity code (default `JPY`) |

Output targets the **GnuCash 4.x+ "Import Transactions from CSV" format**.
Multi-split transactions (from `Transaction.splits`) come out as one
date-stamped row + one continuation row per extra split — exactly what
GnuCash expects. After writing the CSV, each input record is re-emitted on
stdout with `exported.gnucash` so you can chain another export target.

---

## `verify-tnumber`

```sh
fetchdoc verify-tnumber T1234567890123
```

Validates a Japanese qualified-invoice (適格請求書) registration number
against the National Tax Agency public registry. **Today** only the regex
format check is wired (`T` + 13 digits); the live NTA API call is tracked
by issue #26.

---

## Setting up Gmail access

fetchdoc reads Gmail through your **own** Google Cloud OAuth client
(BYO-credentials), so there is a one-time setup. Plan ~10 minutes the first time.

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

Verify with `fetchdoc auth status`. To clear, run `fetchdoc auth logout --source gmail`.
If something looks off, the [client_secret.json troubleshooting](#troubleshooting-auth) section below has the common pitfalls.

---

## Subcommand reference

| Subcommand | Reads | Writes |
|---|---|---|
| `auth init [--from PATH]` | — | `~/.config/fetchdoc/client_secret.json` |
| `auth login --source gmail` | — | OS keychain (refresh token) |
| `auth status` | — | stderr summary |
| `auth logout --source gmail` | — | OS keychain (delete) |
| `fetch gmail [--since DATE] [--query Q] [--limit N]` | — | `Document` JSONL via Gmail API |
| `fetch eml --dir PATH` | — | `Document` JSONL (no OAuth) |
| `fetch mbox --file PATH` / `--dir PATH` | — | `Document` JSONL (no OAuth) |
| `fetch maildir --dir PATH` | — | `Document` JSONL (no OAuth) |
| `fetch dir --dir PATH [--include-ext EXT…] [--move-to DIR]` | — | `Document` JSONL from manually-downloaded files |
| `render-body [--renderer auto\|chromium\|weasyprint\|wkhtmltopdf]` | `Document` JSONL | JSONL with rendered-PDF `attachment_path` for body-primary records |
| `classify [--ocr=anthropic\|vertex\|openai] [--model NAME]` | `Document` JSONL | JSONL with `extracted` field |
| `import csv [--profile NAME\|--infer] [--dir DIR] FILE` | CSV file | `Transaction` JSONL |
| `import xlsx [--profile NAME\|--infer] [--sheet NAME] FILE` | xlsx file | `Transaction` JSONL |
| `import dedup --against FILE [--against FILE…]` | `Transaction` JSONL | filtered `Transaction` JSONL |
| `export local --root PATH [--name-template TPL]` | `Document` JSONL | files + JSONL with `exported.local` |
| `export gnucash --out CSV …` | `Document` or `Transaction` JSONL | CSV file + JSONL with `exported.gnucash` |
| `verify-tnumber TXXXXXXXXXXXXX` | — | one-line summary on stdout |

### Common conventions

- **stdin / stdout = JSONL**, **stderr = human progress**. Never mix.
- `--quiet` suppresses stderr progress on every subcommand.
- Exit codes: **0** all OK · **1** fatal (auth, network, bug) · **2** partial success (`needs_review` mixed in).
- Records accumulate fields. `fetch` sets `attachment_path` + `source_meta`,
  `classify` adds `extracted`, `export *` adds `exported.{local,gnucash}`.
- All paths default under `$XDG_CONFIG_HOME` / `$XDG_CACHE_HOME` (or
  `~/.config` / `~/Library/Caches` / `%APPDATA%` per OS).

---

## Design principles

- **Each subcommand does one thing**: fetch, classify, import, or export. No god command.
- **Idempotent**: re-running an import / fetch with the same inputs returns
  the same `external_id`s, so `import dedup` (or downstream tools) can skip
  duplicates.
- **No central server**: every credential lives on your machine, every PDF
  is written to your filesystem (or your Drive). fetchdoc keeps no state of
  its own beyond a local cache.
- **AI-callable**: predictable subcommand naming, schemas declared in code,
  no interactive prompts when stdin is not a TTY.
- **Bring-your-own-credentials**: no central OAuth client, no central LLM
  key, no telemetry. You pay your own AI bill, you own your own Gmail
  scope, you can `auth logout` at any time.

---

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
