# codex-plugins

Codex Proxy RS（codex-proxy-rs）的独立网关插件集。插件通过公开 `gateway-plugin-sdk` 与宿主通信，不修改宿主业务模块。

## 目录

| 目录 | 说明 |
| --- | --- |
| `plugins/codex-iq-watch` | Codex 降智监控插件：观察账号级信号（上游过载、缓存命中骤降、响应变慢），按窗口判定降智并通过 Webhook／邮件 HTTP API 告警 |
| `scripts/` | 打包等工程脚本（`cpr-plugin package` 封装） |

## 契约版本

- SDK `0.1.0`、`manifestVersion: 2`、进程协议 `2`
- 目标宿主：`codex-proxy-rs >=3.19.0, <4.0.0`（以 `engines` 声明为准）
- 宿主侧合同：`codex-proxy-rs/backend/crates/gateway-plugin/{sdk,runtime}`

## 构建

插件只依赖公开 SDK。SDK 未独立发布，当前固定引用宿主发行标签 [`v3.19.0`](https://github.com/zyycn/codex-proxy-rs/releases/tag/v3.19.0)，由 `Cargo.lock` 锁定到对应提交，仓库可脱离本地并排目录独立构建。

```bash
cargo test --manifest-path plugins/codex-iq-watch/Cargo.toml --locked
cargo build --manifest-path plugins/codex-iq-watch/Cargo.toml --release --locked --target x86_64-unknown-linux-gnu
```

CI（`.github/workflows/codex-iq-watch.yml`）执行 fmt/clippy/test/Linux 构建并产出 `cpr-plugin` 归档；打 `codex-iq-watch-*` tag 会把归档挂到 Release。手动打包见 `scripts/package` 与各插件 README。
