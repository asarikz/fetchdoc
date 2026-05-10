# CLAUDE.md

このファイルは Claude Code が新規セッションで読む。プロジェクトの非自明な決定・規約・落とし穴を簡潔に記録する。詳細は README.md と `docs/` 参照。

## プロダクト要点

`fetchdoc` は **OSS の Rust CLI**。Gmail から請求書 PDF を取得 → AI で分類・抽出 → GnuCash CSV 等に出力する Unix 哲学のツール群。電帳法対応がドメイン上の差別化点。

- **配布モデル**: 単一バイナリを `cargo install` / GitHub Releases / `brew install asarikz/tap/fetchdoc` で配布
- **収益モデル**: なし（OSS、寄付モデル）
- **ターゲット**: リテラシーのあるパワーユーザー（個人事業主・小規模法人の DIY 寄り）
- **AI フレンドリ**: stdin/stdout = JSONL、stderr = 人間向け、`--quiet` で消える

## 重要な決定（と理由）

### Rust（TypeScript ではなく）
- 単一バイナリ配布（`cargo install` / 各 OS バイナリ）が容易
- OS keychain アクセス（`keyring` crate）がネイティブモジュール問題なく動く
- 起動速度・メモリ効率（pipe で頻繁に立ち上がるツールに重要）
- Cargo + clap + tokio の標準スタックで CLI が組める

### BYO credentials（中央 OAuth client を持たない）
- ユーザーが自分の Google Cloud project で Desktop OAuth client を作る
- → 中央 app の verification 不要 = `gmail.readonly` の **CASA Tier 2** ($2K-$8K, 3-5ヶ月) を回避
- 引き換えにユーザーは GCP Console を 1 回触る必要がある（README に手順、`fetchdoc auth init` でも案内）

### OCR backend は pluggable、初期デフォルトは Anthropic
- `ANTHROPIC_API_KEY` 1 本で動くので導入摩擦が最小
- Vertex AI Gemini は BYO credentials で選択可能（追加 API 有効化が必要）
- OpenAI も後で追加予定

### Refresh token は OS keychain に保管
- 平文ファイル保管・envelope 暗号化は不要（OS の keychain が十分）
- macOS Keychain / Windows Credential Manager / Linux Secret Service を `keyring` crate で透過アクセス

### dual MIT / Apache-2.0
- Rust エコシステム標準。tokio・clap・serde 等と同じ。貢献ハードル最低

### CI なし→ あり（GitHub Actions）
- `cargo install` で誰でもビルドする OSS なので、green CI は contributor 信頼度に直結
- Linux + macOS で `fmt` / `clippy -D warnings` / `test`

## I/O 規約

### Document スキーマ（pipe で渡る JSONL）

```jsonc
{
  "source": "gmail",
  "external_id": "<messageId>",
  "attachment_path": "/path/to/cached.pdf",
  "source_meta": { /* 任意の追加情報 */ },
  "extracted": {
    "transaction_date": "2026-04-30",
    "total_amount_jpy": 12100,
    "counterparty_name": "Acme",
    "counterparty_t_number": "T1234567890123",
    "confidence": 0.94
  },
  "exported": { /* export 後の追加 */ },
  "status": "ok"  // or "needs_review"
}
```

実装は `src/io.rs::Document` / `Extracted`。フィールドは累積的（fetch → classify → export で足されていく）。

### exit code

| code | 意味 |
|---|---|
| 0 | 全件成功 |
| 1 | 致命的エラー（API 認証失敗、ネットワーク、バグ等） |
| 2 | 一部成功（needs_review が混じる） |

### --quiet / --json-schema

- `--quiet`: stderr の進捗表示を抑止
- `--json-schema`: そのコマンドの入出力 JSON Schema を stdout に吐く（AI から呼ぶときに使う）

## 電帳法 / インボイス制度 知識

- 適格請求書登録番号は **`T` + 13桁** 必ず（`src/invoicing_jp/tnumber.rs` に regex）
- 検索要件3項目：**取引年月日・取引金額・取引先名**。`extracted` の必須フィールドに対応
- ファイル命名デフォルト：`{yyyy-mm-dd}_{counterparty_name}_{total_amount}円.pdf`（命名だけで検索要件を満たす）
- 真実性確保は (c)「事務処理規程」パスを採用予定（v0.2 milestone）
- 国税庁「適格請求書発行事業者公表サイト Web-API」で T 番号検証可能（v0.2 で実装）

## ディレクトリ早見

```
src/
  main.rs             # エントリ。tokio::main + clap parse
  cli.rs              # 全サブコマンドのトップレベル定義
  io.rs               # Document / Extracted / JSONL helpers
  auth/
    mod.rs            # auth init/login/status/logout
    pkce.rs           # PKCE (RFC 7636) 生成
    google.rs         # Google OAuth 定数（scope / endpoint）
    storage.rs        # OS keychain (`keyring` crate)
  sources/
    mod.rs            # fetch サブコマンド
    gmail.rs          # Gmail API クライアント
  classify/
    mod.rs            # classify サブコマンド
    anthropic.rs      # Anthropic Claude backend
  export/
    mod.rs            # export サブコマンド
    gnucash.rs        # ★ MVP 優先：GnuCash CSV importer 形式
    local.rs          # ローカルファイル書き出し
  invoicing_jp/
    mod.rs            # 国税庁 API + verify-tnumber
    tnumber.rs        # T番号 regex
```

## 開発ワークフロー

- **1 Issue 1 PR**。ブランチ名 `<area>/<short-desc>`（例: `auth/oauth-pkce`、`classify/anthropic-impl`）
- コミットメッセージは英語。本文に `Closes #N`
- PR タイトルは Conventional 風（`feat:` `fix:` `docs:` `chore:` `refactor:`）
- マージは **メンテナが手動**

### 必須ローカルコマンド

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

CI と同じ。push 前に通すこと。

## 注意・落とし穴

- **`gh issue` 中で `RUSTFLAGS=-D warnings` を設定している**: ローカルで warning が出るとビルドが落ちる。warning を無視せず直すか、特定箇所だけ `#[allow(...)]` を当てる
- **edition = "2024"**: Rust 1.85 以降が必要
- **`reqwest` は rustls-tls only**: native-tls (OpenSSL) には依存しない（クロスコンパイル容易、cargo install ユーザの環境差吸収）
- **stdout を JSONL 用に占有する**: `println!` で人間向けメッセージを出してはならない。`eprintln!` を使う
