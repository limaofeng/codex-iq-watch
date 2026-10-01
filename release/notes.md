# codex-iq-watch 0.2.4

- SDK 依赖和打包 CLI 固定到 `codex-proxy-rs v3.19.0`
- 适配清单、进程协议 v2，以及统一的 `observer` 请求完成事件；保持既有判定规则、告警冷却和私有状态格式

需要宿主 `>=3.19.0, <4.0.0`。安装后切换插件实例到新版本，并核对 `observer` 的 `request_completed` 绑定范围；旧的 `request_lifecycle`／`usage` 绑定不能直接用于新合同。调度避让仍由 `scheduler` 绑定及配置开关控制。

安装包为 Linux x86_64，下载 `.tar.gz` 与对应 `.sha256` 校验文件。
