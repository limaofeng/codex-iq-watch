# codex-iq-watch — Codex 降智监控插件

按账号观察每次请求的终态信号，持续命中降智特征时通过 Webhook／邮件 HTTP API 告警，并提供管理页面查看状态。

## 判定规则

| 信号 | 条件 | 默认阈值 |
| --- | --- | --- |
| `upstream_overload` | 上游返回 502/503/529，或错误码含 overload/capacity 关键词 | — |
| `cache_collapse` | 账号此前缓存持续命中，本次输入 ≥ 下限且 `cached_tokens = 0` | 输入 ≥ 1000 tokens |
| `slow_response` | 整体耗时或首 token 耗时超阈值 | 10s |

同一账号在窗口（默认 15 分钟）内满足「带信号请求 ≥ 2 个」且「不同信号 ≥ 2 种」记为一次降智结论；连续 2 次结论后触发告警，同一账号 30 分钟冷却。全部阈值在插件设置中可调。

## 通知渠道

- **Webhook**：`webhook_url` + `webhook_format`（generic 结构化 JSON，或 wecom/dingtalk/feishu/slack/bark 机器人格式），可选 `webhook_auth_header` 敏感字段（格式 `Header-Name: value`）
- **邮件 HTTP API**：`email_url` + `email_format`（resend/postmark/sendgrid/generic 模板）+ `email_from`/`email_to`，可选 `email_auth_header`；原始 SMTP 不在宿主受管网络范围内，需走 HTTP API

`enabled=false` 时仍统计信号，不发送通知。

## 能力声明

- `request_lifecycle` + `usage`：观察请求终态与用量（`policy.observe_request`）
- `management`：状态页与 `status`/`events`/`alerts`/`test-notify` 管理路由
- 权限：`requests`（观察事实）、`network`（通知出站）、`public_endpoints`（页面图标）

## 构建

```bash
cargo test --manifest-path Cargo.toml
cargo build --manifest-path Cargo.toml --release --target x86_64-unknown-linux-gnu
```

SDK 未发布时使用相对路径依赖：`../../../codex-proxy-rs/backend/crates/gateway-plugin/sdk`。

打包见 `../../scripts/package`（`cpr-plugin package` 封装）。
