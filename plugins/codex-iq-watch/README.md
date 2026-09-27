# codex-iq-watch — Codex 降智监控插件

按账号观察每次请求的终态信号，持续命中降智特征时通过 Webhook／邮件 HTTP API 告警，并提供管理页面查看状态。

## 判定规则

| 信号 | 条件 | 默认阈值 |
| --- | --- | --- |
| `upstream_overload` | 上游返回 502/503/529，或错误码含 overload/capacity 关键词；429/rate_limit 属限流，不计入 | — |
| `cache_collapse` | 账号此前缓存持续命中，本次输入 ≥ 下限且 `cached_tokens = 0` | 输入 ≥ 1000 tokens |
| `slow_response` | 首 token 耗时超阈值（整体耗时不计入，避免长回答误报） | 首 token ≥ 10s |

同一账号在窗口（默认 15 分钟）内满足「带信号请求 ≥ 2 个」且「不同信号 ≥ 2 种」记为一次降智结论；连续 2 次结论后触发告警，同一账号 30 分钟冷却。全部阈值可在管理页「通知设置 → 检测参数」页签调整（生效值回填，清空字段即恢复宿主配置）；旧版 `latency_ms` 仅兼容保留、不再参与判定。

## 通知渠道

- **Webhook**：`webhook_url` + `webhook_format`（generic 结构化 JSON，或 wecom/dingtalk/feishu/slack/bark 机器人格式），可选 `webhook_auth_header` 敏感字段（格式 `Header-Name: value`）
- **邮件 HTTP API**：`email_url` + `email_format`（resend/postmark/sendgrid/generic 模板）+ `email_from`/`email_to`，可选 `email_auth_header`；原始 SMTP 不在宿主受管网络范围内，需走 HTTP API

`enabled=false` 时仍统计信号，不发送通知。

页面保存设置时先做完整校验：非法类型、越界范围、无效 URL、错误认证头格式或超长字段返回 400 且不覆盖已保存配置；`null` 表示移除覆盖项，回到宿主配置。告警与各渠道「投递结果未确认」记录会先持久化，再发送并补写结果。宿主观察回调的总期限最多 2 秒，慢 Webhook／邮件 API 可能超时；中断或结果回写失败时保留未确认记录，不代表确定未送达，也不会自动重发，以免重复通知。冷却期仍生效，管理页测试通知使用管理调用，不能代替真实观察链路的时限验证。

## 调度避让（可选）

插件声明了 `scheduler` 能力：开启后宿主每次为请求选账号时先回调插件，候选中被判定为「降智」（已触发告警）的账号会被排除，在剩余账号中按宿主内置同款次序（in_flight 最少、失败率最低、权重最大）挑一个。

启用两步：

1. 实例配置 → 能力绑定中勾选「调度 · 账号调度」（失败策略建议 `delegate`：插件异常/超时时回退内置调度，不影响流量）；可按 Key／账号组／Provider／模型限定范围。
2. 管理页「通知设置 → 检测参数」勾选「调度时排除降智账号」（对应配置 `schedule_exclude_degraded`，默认关闭；未绑定调度时该开关无效果）。

- 「疑似」账号不排除，只排除已告警的「降智」账号。
- 候选账号全部降智时按 `schedule_all_degraded` 处理：`delegate`（默认）交回内置调度、仍会用到降智账号但不断流；`reject` 直接拒绝该请求。管理页同位置可改。
- 被排除的账号不参与调度便不会产生新观察记录，状态不会自动恢复；状态页「清除」或测试正常后手动清除即可重新参与调度。

## 能力声明

- `request_lifecycle` + `usage`：观察请求终态与用量（`policy.observe_request`）
- `scheduler`：账号调度阶段排除降智账号（`policy.schedule_account`，需实例绑定启用）
- `management`：状态页（账号状态＋聚合告警历史＋信号详情）与 `status`/`events`/`alerts`/`settings`/`test-notify`/`account-clear`/`models`/`candy-test` 管理路由
- 权限：`requests`（观察事实）、`network`（通知出站）、`accounts`（账号显示名解析）、`models`（糖果题测试）、`public_endpoints`（页面图标）

## 账号显示名

与宿主账号列表主标识一致：`api_key` 凭据显示用户填的账号名称，其余（OAuth 等）优先邮箱；都为空时回退为截断的内部 ID。插件取不到宿主备注（notes）。

状态页账号列表以宿主账号全集为基底（`host.auth.list`），逐个读取观察状态：尚未产生观察的账号显示为「未观察」，读取失败显示「未知」并提示；停用账号带「停用」标记，列表按降智 > 疑似 > 正常 > 其他排序。

私有状态为索引和设置预留记录，最多记录 254 个账号；达到记录或总字节配额后不自动淘汰账号、不解除降智隔离，新记录可能无法写入。状态页提示容量限制，可手动清除不再需要的记录。旧版被 64 条索引截断的账号，只要仍在宿主账号列表中也会读取真实状态；旧版遗留但已从宿主删除且未进入索引的记录，公开 SDK 无枚举接口，不能自动发现或清理。

## 糖果题测试

管理页每行账号可发起「糖果题测试」（顶部也有全局入口，弹窗内可选账号与客户端 Key）：通过 `host.keys.list`/`host.models.list`/`host.model.execute`（`models` 权限）借用所选 Key 的身份、按账号发送一道推理题（正确答案 21）。弹窗内可选客户端 Key（模型下拉仅列该 Key 可见模型，默认选中 `gpt-6-astra`）与 reasoning effort（`low`/`medium`/`high`/`xhigh`/`max`，默认 `low`，`default` 为不指定），用于对比不同思考档位下的推理质量。结果（答对/答错/调用失败）写入账号 `last_probe` 并显示在「最近测试」列；这是主动探针，与信号窗口判定相互独立。

## 构建

```bash
cargo test --manifest-path Cargo.toml --locked
cargo build --manifest-path Cargo.toml --release --locked --target x86_64-unknown-linux-gnu
```

SDK 未发布，`Cargo.toml` 固定引用宿主仓库 commit `zyycn/codex-proxy-rs@1c6b5a8f`；锁文件随仓库管理，构建需要 Rust ≥ 1.97（见 `rust-toolchain.toml`）。

打包见 `../../scripts/package`（`cpr-plugin package` 封装），或在 CI 打 `codex-iq-watch-*` tag 产出 Release 附件。
