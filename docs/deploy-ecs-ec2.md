# codex-bridge を ECS on EC2 にデプロイする

> **このドキュメントは Terraform を書く LLM エージェント向け**に、このアプリの実行時契約と
> 「守らないと壊れる制約」をまとめたものです。数値や名前はそのまま使えます。
> 判断に迷ったら、§1 の制約を最優先にしてください。

## 0. 前提となる固定値

| 項目 | 値 |
| --- | --- |
| AWS アカウント | `526049455470` |
| リージョン | `ap-northeast-1` |
| ECR リポジトリ | `526049455470.dkr.ecr.ap-northeast-1.amazonaws.com/llm-gateway/codex-bridge` |
| イメージタグ | `latest` / `0.1.0` / `sha-<git short sha>`（すべて同一ダイジェストのマルチアーキ manifest） |
| 対応アーキテクチャ | `linux/amd64`, `linux/arm64`（Graviton 可。ECS agent が自動選択） |
| イメージサイズ | 約 2 MB（musl 静的バイナリ + `distroless/static:nonroot`） |
| コンテナ内ユーザー | **uid/gid `65532`**（nonroot、root には切り替えられない） |
| 待ち受けポート | `3000` (`PORT` で変更可) |
| ヘルスチェック | `["CMD", "/codex-bridge", "--health"]`（イメージに curl/sh は無い） |
| 起動タイプ | **EC2**（Fargate は §1-2 の理由で EFS が必須になるため非推奨） |

## 1. 絶対に守る制約

### 1-1. プロセスは常に 1 つだけ

ChatGPT サブスクリプションの `refresh_token` は **使い捨てで、使うたびにローテーション**します。
同じ認証情報を持つプロセスが 2 つ同時に存在すると、片方がリフレッシュした瞬間にもう片方の
`refresh_token` が無効になり、以後そのプロセスは 401 → リフレッシュ失敗を繰り返します。

したがって:

- `desired_count = 1` 固定。オートスケーリングは設定しない
- デプロイ設定は **`minimum_healthy_percent = 0` / `maximum_percent = 100`**（「旧タスクを止めてから新タスクを起動」）。
  既定の `100 / 200` は新旧が一瞬同居するので **不可**
- 同じ `auth.json` を別のコンテナやローカルの Codex CLI と共有しない

### 1-2. `auth.json` は「書き換わる状態」であって「秘密の設定値」ではない

アプリはトークンをリフレッシュするたびに `auth.json` を**その場で上書き**します。
そのため:

- Secrets Manager / SSM Parameter Store / 環境変数から渡す方式は **不可**（書き戻せない）
- コンテナ内のエフェメラルストレージに置く方式は **不可**（タスク再起動で最新の `refresh_token` を失う）
- **ホストの永続ディレクトリを bind mount する**（EC2 起動タイプならこれが最も簡単）
- マウントは **read-write**。`readOnly = true` にしない
- ファイルは **uid 65532 が書けること**（`chown 65532:65532`）
- 初回の投入は手作業（§4）。Terraform の管理対象外にする

### 1-3. 認証は `BRIDGE_API_KEY` 頼み（未設定なら素通し）

`BRIDGE_API_KEY` を設定すると `Authorization: Bearer <key>`（または `x-api-key`）が無い
リクエストを `401` で弾きます。**未設定だと到達できる相手は誰でもサブスクリプションを
消費でき、`POST /refresh` でトークンをローテーションさせることもできます。**

- `auth.json` と違ってこちらは書き戻しが無いので、Secrets Manager / SSM から
  `secrets` で渡して構いません（むしろ推奨）
- カンマ区切りで複数キーを持てるので、利用者ごとにキーを分けたり無停止で
  ローテーションしたりできます
- キーを設定していても、Security Group で `3000/tcp` のインバウンドは
  **明示的に許可した送信元だけ**に絞ってください。`0.0.0.0/0` は不可
- `GET /health` だけは認証なしでも応答します（ヘルスチェック用）。ただしキーの無い
  呼び出しには `{"ok":true}` / `503` しか返さず、アカウント情報は出しません

## 2. コンテナの実行時契約

### 環境変数（すべて任意）

| 変数 | 既定値 | 用途 |
| --- | --- | --- |
| `PORT` | `3000` | 待ち受けポート。ヘルスチェックも同じ値を見る |
| `BRIDGE_API_KEY` | (なし) | 受け付ける API キー。カンマ区切りで複数可。未設定は認証なし（§1-3） |
| `CODEX_AUTH_PATH` | `/data/auth.json`（イメージ側で設定済み） | 認証情報ファイル |
| `CODEX_CLI_VERSION` | `0.0.0` | `User-Agent: codex_cli/<ver>`。Codex CLI の最新版に合わせる（例 `0.154.0`） |
| `CODEX_UPSTREAM` | `https://chatgpt.com/backend-api/codex` | 通常は変更不要 |
| `CODEX_USAGE_URL` | `https://chatgpt.com/backend-api/wham/usage` | 通常は変更不要 |

### ファイルシステム

| パス | 内容 |
| --- | --- |
| `/codex-bridge` | 実行バイナリ（ENTRYPOINT） |
| `/data/auth.json` | 認証情報。**ホストの `/opt/codex-bridge/auth.json` を bind mount** |

ファイル単体ではなく **ディレクトリ `/opt/codex-bridge` → `/data`** をマウントしてください。
ファイル単体をマウントすると、ホスト側にファイルが無い状態でタスクが起動したとき Docker が
同名の**ディレクトリ**を作ってしまい、以後ファイルを置けなくなります。

### エンドポイント

| パス | 用途 |
| --- | --- |
| `GET /health` | 上流を叩かない。プロセス生存 + トークン期限 + `has_refresh_token`。**監視・LB のヘルスチェックはこれ** |
| `GET /usage` | サブスクリプションの消費率。上流を叩く（サブスク消費は無し） |
| `POST /refresh` | 強制リフレッシュ（デバッグ用） |
| それ以外 | すべて上流へパススルー（`/v1/...` は `/v1` を外して転送） |

### リソース

実測でメモリ数 MB、CPU はほぼゼロ。`memory_reservation = 32`, `memory = 128`, `cpu = 128` で十分。

### ログ

stdout/stderr にプレーンテキスト。`awslogs` ドライバで CloudWatch Logs へ。
`[auth] access token refreshed` が定期的（数時間〜1 日おき）に出ていれば正常。
`token refresh failed` が出たら `refresh_token` が死んでいるので §4 の再投入が必要。

## 3. Terraform で作るもの

### 3-1. 一覧

| リソース | 要点 |
| --- | --- |
| `aws_ecr_repository` | **作成済み**（`llm-gateway/codex-bridge`）。`data` で参照するか import する |
| `aws_ecs_cluster` | 1 つ |
| EC2 インスタンス（ASG `min=max=desired=1` または単体） | ECS-optimized AMI。arm64 (t4g.nano/micro) で十分。インスタンスプロファイルに `AmazonEC2ContainerServiceforEC2Role` + `AmazonSSMManagedInstanceCore`（§4 で Session Manager を使うため） |
| `aws_security_group` | インバウンド `3000/tcp` を許可元に限定。SSH は不要（SSM を使う） |
| `aws_iam_role` (task execution role) | `AmazonECSTaskExecutionRolePolicy`（ECR pull + CloudWatch Logs）。`BRIDGE_API_KEY` を Secrets Manager から渡すなら、その ARN への `secretsmanager:GetSecretValue` をインラインポリシーで追加 |
| `aws_secretsmanager_secret` (+ `_version`) | `BRIDGE_API_KEY`。`openssl rand -hex 32` などで生成。SSM Parameter Store (SecureString) でも可 |
| task role | **不要**（アプリは AWS API を呼ばない） |
| `aws_cloudwatch_log_group` | `/ecs/codex-bridge`、保持 14〜30 日 |
| `aws_ecs_task_definition` | §3-2 |
| `aws_ecs_service` | §3-3 |

### 3-2. タスク定義

```hcl
resource "aws_ecs_task_definition" "codex_bridge" {
  family                   = "codex-bridge"
  requires_compatibilities = ["EC2"]
  network_mode             = "bridge"
  execution_role_arn       = aws_iam_role.ecs_task_execution.arn
  # task_role_arn は不要

  volume {
    name      = "codex-data"
    host_path = "/opt/codex-bridge"
  }

  container_definitions = jsonencode([{
    name  = "codex-bridge"
    # latest は mutable なので Terraform が差分を検知できない。
    # 明示的にロールアウトしたいなら sha-<short> か 0.1.0 を固定して変数で管理する。
    image             = "526049455470.dkr.ecr.ap-northeast-1.amazonaws.com/llm-gateway/codex-bridge:0.2.0"
    essential         = true
    cpu               = 128
    memory            = 128
    memoryReservation = 32

    portMappings = [{ containerPort = 3000, hostPort = 3000, protocol = "tcp" }]

    mountPoints = [{
      sourceVolume  = "codex-data"
      containerPath = "/data"
      readOnly      = false           # 必須: リフレッシュ時に auth.json を書き戻す
    }]

    environment = [
      { name = "CODEX_CLI_VERSION", value = "0.154.0" },
    ]

    # auth.json と違い書き戻しが無いので、こちらは Secrets Manager で渡せる。
    secrets = [
      { name = "BRIDGE_API_KEY", valueFrom = aws_secretsmanager_secret.bridge_api_key.arn },
    ]

    healthCheck = {
      command     = ["CMD", "/codex-bridge", "--health"]
      interval    = 30
      timeout     = 5
      retries     = 3
      startPeriod = 5
    }

    logConfiguration = {
      logDriver = "awslogs"
      options = {
        "awslogs-group"         = aws_cloudwatch_log_group.codex_bridge.name
        "awslogs-region"        = "ap-northeast-1"
        "awslogs-stream-prefix" = "ecs"
      }
    }
  }])
}
```

### 3-3. サービス

```hcl
resource "aws_ecs_service" "codex_bridge" {
  name            = "codex-bridge"
  cluster         = aws_ecs_cluster.this.id
  task_definition = aws_ecs_task_definition.codex_bridge.arn
  launch_type     = "EC2"

  desired_count = 1                                   # §1-1: 固定。増やさない

  deployment_minimum_healthy_percent = 0              # §1-1: 止めてから起動
  deployment_maximum_percent         = 100

  deployment_circuit_breaker {
    enable   = true
    rollback = true
  }

  # ALB を挟む場合のみ。ヘルスチェックパスは /health、間隔は 30s 程度で十分
  # load_balancer { ... }
}
```

### 3-4. EC2 の user_data（ホスト側ディレクトリの準備）

```sh
#!/bin/bash
echo "ECS_CLUSTER=<cluster name>" >> /etc/ecs/ecs.config
mkdir -p /opt/codex-bridge
chown 65532:65532 /opt/codex-bridge
chmod 700 /opt/codex-bridge
# auth.json 自体はここでは作らない（§4 で手動投入）
```

## 4. `auth.json` の投入（Terraform 管理外・手作業）

初回、および `refresh_token` が死んだとき（`/health` の `has_refresh_token` が `false`、
またはログに `token refresh failed`）に行います。

```sh
# 1. 手元の PC で発行（ブラウザで OAuth ログイン）
codex login                                   # → ~/.codex/auth.json

# 2. インスタンスへ送る（SSM Session Manager 経由。SSH を開けていない前提）
INSTANCE_ID=$(aws ecs list-container-instances --profile ecs --region ap-northeast-1 \
  --cluster <cluster name> --query 'containerInstanceArns[0]' --output text \
  | xargs -I{} aws ecs describe-container-instances --profile ecs --region ap-northeast-1 \
      --cluster <cluster name> --container-instances {} --query 'containerInstances[0].ec2InstanceId' --output text)

aws ssm send-command --profile ecs --region ap-northeast-1 \
  --instance-ids "$INSTANCE_ID" \
  --document-name AWS-RunShellScript \
  --parameters commands="[
    \"cat > /opt/codex-bridge/auth.json <<'JSON'\n$(cat ~/.codex/auth.json)\nJSON\",
    \"chown 65532:65532 /opt/codex-bridge/auth.json\",
    \"chmod 600 /opt/codex-bridge/auth.json\"
  ]"

# 3. タスクを再起動して読み込ませる
aws ecs update-service --profile ecs --region ap-northeast-1 \
  --cluster <cluster name> --service codex-bridge --force-new-deployment

# 4. 確認（Security Group で許可した場所から）
curl -s http://<host>:3000/health | jq .
#   .ok == true, .token.has_refresh_token == true, .token.expires_in_seconds > 0

# 5. 手元の Codex CLI を使い続けるなら、同じ refresh_token を共有しないよう再ログインして別グラントを取る
codex login
```

投入後はインスタンス上の `auth.json` だけが正です。手元のコピーは数時間以内に
`refresh_token` が無効になるので、バックアップとしての価値はありません。
インスタンスを作り直したら（AMI 更新・ASG リプレース）、この手順を最初からやり直します。

## 5. 監視（任意）

- CloudWatch Logs のメトリクスフィルタ: `"token refresh failed"` → アラーム（= 要 §4）
- 外形監視: `GET /health` の `token.expires_in_seconds` が減り続けて `token.last_refresh` が
  更新されない場合、リフレッシュが機能していない
- ECS サービスの `RunningTaskCount < 1` アラーム

## 6. やってはいけないことのチェックリスト

- [ ] `desired_count` を 2 以上にする / Application Auto Scaling を付ける
- [ ] `deployment_minimum_healthy_percent` を 0 以外にする
- [ ] Fargate に載せ替える（EFS が無いと `auth.json` が永続しない）
- [ ] `auth.json` を Secrets Manager / SSM / 環境変数 / イメージに焼き込む
- [ ] マウントを `readOnly = true` にする、またはファイル単体を bind mount する
- [ ] `3000/tcp` を `0.0.0.0/0` に開ける
- [ ] 同じ `auth.json` をローカルの Codex CLI や別環境と共用する
- [ ] ヘルスチェックに `curl` を使う（イメージに無い）
