# codex-bridge

ChatGPT サブスクリプション (Codex) のバックエンドをそのまま HTTP API として叩けるようにする、最小構成のパススループロキシです。Rust 製、静的リンクの単一バイナリ (約 1.6 MB)、Docker イメージは distroless ベースで約 10 MB。

- リクエストはメソッド・パス・クエリ・ボディを **一切加工せず** `https://chatgpt.com/backend-api/codex` に転送します
- レスポンスも成功 / SSE ストリーム / エラーを問わず **そのまま** 返します
- やることは OAuth トークンの管理だけ:
  - `~/.codex/auth.json` (Codex CLI の認証情報) を読む
  - 期限 5 分前になったら事前にリフレッシュ
  - 上流が 401 を返したらリフレッシュして 1 回だけリトライ
  - ローテーションした `refresh_token` を `auth.json` に書き戻す (CLI 側と乖離しない)

## 必要なもの

- Rust (stable) — ローカルビルドの場合
- `codex login` 済みの `~/.codex/auth.json`

## 起動

```sh
cargo run --release      # http://localhost:3000
```

環境変数 (すべて任意、`.env.example` 参照):

| 変数 | 既定値 | 説明 |
| --- | --- | --- |
| `PORT` | `3000` | 待ち受けポート |
| `CODEX_AUTH_PATH` | `~/.codex/auth.json` | 認証情報ファイル |
| `CODEX_UPSTREAM` | `https://chatgpt.com/backend-api/codex` | 転送先 |
| `CODEX_USAGE_URL` | `https://chatgpt.com/backend-api/wham/usage` | `GET /usage` の転送先 |
| `CODEX_CLI_VERSION` | `0.0.0` | `User-Agent: codex_cli/<ver>` に使うバージョン |

## 使い方

上流は OpenAI Responses API 互換なので、`base_url` をこのプロキシに向けるだけです。

```sh
curl -N http://localhost:3000/responses \
  -H 'content-type: application/json' \
  -d '{
    "model": "gpt-5.3-codex-spark",
    "instructions": "You are a helpful assistant.",
    "input": [{"role":"user","content":[{"type":"input_text","text":"hello"}]}],
    "store": false,
    "stream": true
  }'
```

```python
from openai import OpenAI
client = OpenAI(base_url="http://localhost:3000", api_key="unused")
with client.responses.stream(
    model="gpt-5.3-codex-spark",
    instructions="You are a helpful assistant.",
    input="hello",
    store=False,
) as stream:
    for event in stream:
        ...
```

### ブリッジ側で持っているエンドポイント

上流のパスと衝突しない範囲で、以下だけ特別扱いしています。それ以外はすべてパススルーです。

| エンドポイント | 説明 |
| --- | --- |
| `GET /usage` | `backend-api/wham/usage` へ転送。プラン・レート制限の消費率・リセット時刻がそのまま返る |
| `GET /health` | 上流を叩かずに応答。トークンの残り有効期限・最終リフレッシュ時刻・アカウント情報 (id_token 由来) を返す。ロードバランサーのヘルスチェックや「refresh_token が死んでいないか」の監視に |
| `POST /refresh` | 強制的にトークンをリフレッシュして `/health` と同じ内容を返す (デバッグ用) |

```sh
curl -s localhost:3000/health
# {"ok":true,"account_id":"...","email":"...","plan_type":"pro",
#  "token":{"expires_at":1789900829,"expires_in_seconds":799397,"last_refresh":"...","has_refresh_token":true}}

curl -s localhost:3000/usage | jq .rate_limit.primary_window
# {"used_percent":1,"limit_window_seconds":604800,"reset_after_seconds":541165,"reset_at":1789642596}
```

モデル一覧は上流にあるのでそのままパススルーで取れます: `GET /models?client_version=0.154.0`

`codex-bridge --health` で起動すると自分自身の `/health` を叩いて exit 0/1 を返します (Docker の `HEALTHCHECK` に使用)。

### 上流 (chatgpt.com/backend-api/codex) の制約

パススルーなので、以下を満たさないリクエストは上流のエラーがそのまま返ります。

- `stream: true` / `store: false` / `instructions` (空でない文字列) が必須
- `max_output_tokens` など公開 API にはあるが未対応のパラメータは 400
- モデルは Codex CLI で使えるもののみ (`gpt-5.5`, `gpt-5.3-codex-spark` など)
- レスポンスは SSE ですが `Content-Type: text/event-stream` が付かないことがあります

## Docker

```sh
docker compose up --build          # または
docker build -t codex-bridge . && docker run -p 3000:3000 -v ~/.codex/auth.json:/data/auth.json codex-bridge
```

マルチアーキ:

```sh
docker buildx build --platform linux/amd64,linux/arm64 -t <registry>/codex-bridge --push .
```

- ビルドステージは `rust:alpine` で各プラットフォームのネイティブ musl 静的ビルド、実行ステージは `gcr.io/distroless/static-debian12:nonroot`
- `auth.json` はリフレッシュ時に書き換わるため、読み取り専用 (`:ro`) でマウントしないでください。コンテナは `nonroot` (uid 65532) で動くので、ファイルに書き込み権限が必要です

## 開発

`.devcontainer/` があるので VS Code の Dev Containers で開けば Rust ツールチェーン・rust-analyzer・clippy・rustfmt が揃った環境になります。ホストの `~/.codex/auth.json` が `/data/auth.json` にマウントされ、`CODEX_AUTH_PATH` もそこを指すので、コンテナ内で `cargo run` すればそのまま動きます。

```sh
cargo run              # デバッグビルドで起動
cargo clippy           # lint
cargo fmt              # フォーマット (rustfmt.toml: max_width = 110)
```

## デプロイ

サブスクリプション 1 つに対して refresh_token は 1 つで、しかも使うたびにローテーションするため、**動かすプロセスは常に 1 つ**にしてください。水平スケールする基盤 (Lambda 等) は向きません。

EC2 `t4g.nano` (arm64) に Docker を入れて `compose.yaml` で動かすのが最も安価 (月 $5 前後) で、この設計に一番合っています。TLS 終端は Cloudflare Tunnel か Caddy を前段に置いてください。

## 注意

- `refresh_token` は使い捨てでローテーションします。同じ `auth.json` を複数プロセスで共有すると、片方が持つ `refresh_token` が無効化されることがあります
- 認証機能はありません。到達できる相手は誰でもあなたのサブスクリプションを消費できるので、Security Group やファイアウォールで接続元を必ず絞ってください
