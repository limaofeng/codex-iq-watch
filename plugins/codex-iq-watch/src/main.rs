//! Codex 降智监控插件入口：观察请求终态信号、判定降智并发送 Webhook/邮件通知，附带管理页面 API。

mod config;
mod detector;
mod notify;
mod store;
mod time_util;

use std::sync::Arc;

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        management::{ManagementPage, ManagementRegistration, ManagementResource, ManagementRoute},
        policy::ObserveRequest,
    },
    client::{Empty, HostClient, PluginBuilder, SessionConfig, TypedCall, TypedReply, methods},
};
use serde_json::json;

use crate::{
    config::WatchConfig,
    detector::{
        AccountState, AccountStatus, AlertRecord, Delivery, apply_event, extract_event,
        render_alert, should_alert,
    },
    notify::{Notice, deliver_all},
};

struct App {
    /// 宿主握手配置；页面保存的通知设置在 `effective_config` 中按需叠加。
    config: WatchConfig,
}

/// 生效配置 = 宿主配置 + 管理页保存的通知覆盖；每次调用读取，保证保存后立即生效。
async fn effective_config(app: &App, host: &HostClient) -> WatchConfig {
    let mut config = app.config.clone();
    if let Ok(Some((settings, _))) = store::load_settings(host).await {
        config.apply_notify_settings(&settings);
    }
    config.normalized()
}

#[tokio::main]
async fn main() {
    let session = match gateway_plugin_sdk::client::PluginSession::accept(
        tokio::io::stdin(),
        tokio::io::stdout(),
        SessionConfig::default(),
    )
    .await
    {
        Ok(session) => session,
        Err(_) => return,
    };
    let config: WatchConfig =
        serde_json::from_value::<WatchConfig>(session.handshake().configuration.clone())
            .unwrap_or_default()
            .normalized();
    let app = Arc::new(App { config });

    let plugin = match PluginBuilder::from_json(include_bytes!("../plugin.json"))
        .and_then(|builder| builder.on(methods::OBSERVE_REQUEST, observe(app.clone())))
        .and_then(|builder| builder.management(management_registration(), management(app.clone())))
        .and_then(|builder| builder.build())
    {
        Ok(plugin) => plugin,
        Err(_) => return,
    };
    let _ = session.run(plugin).await;
}

/// `TypedCall` 到盒装异步回复的通用形态；用于消减处理器签名里的复杂类型。
type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;
type ObserveResult = Result<TypedReply<Empty>, PluginFault>;
type ManagementCall = TypedCall<gateway_plugin_sdk::call::management::ManagementRequest>;
type ManagementResult =
    Result<TypedReply<gateway_plugin_sdk::call::management::ManagementResponse>, PluginFault>;

fn observe(app: Arc<App>) -> impl Fn(TypedCall<ObserveRequest>) -> BoxFuture<ObserveResult> {
    move |call: TypedCall<ObserveRequest>| {
        let app = Arc::clone(&app);
        Box::pin(async move { observe_request(&app, call).await })
    }
}

/// 终态观察：提取信号、更新账号窗口状态；需要告警时经宿主网络回调发通知。
/// 观察结果不携带业务错误；内部失败只记录日志。
async fn observe_request(
    app: &App,
    call: TypedCall<ObserveRequest>,
) -> Result<TypedReply<Empty>, PluginFault> {
    let observation = call.request;
    let account_id = observation.account_id.clone().filter(|id| !id.is_empty());
    let Some(account_id) = account_id else {
        return Ok(TypedReply::new(Empty {}));
    };
    let config = effective_config(app, &call.host).await;
    if !config.watches_provider(observation.provider.as_deref()) {
        return Ok(TypedReply::new(Empty {}));
    }
    if let Err(error) = observe_inner(app, &call.host, &config, &account_id, observation).await {
        log(&call.host, "watch_observe_failed", &error.message).await;
    }
    Ok(TypedReply::new(Empty {}))
}

async fn observe_inner(
    app: &App,
    host: &HostClient,
    config: &WatchConfig,
    account_id: &str,
    observation: ObserveRequest,
) -> Result<(), PluginFault> {
    let now_ms = observation.completed_at_ms;
    let mut attempts = 0;
    loop {
        attempts += 1;
        let loaded = store::load_account(host, account_id).await?;
        let (mut state, version) =
            loaded.unwrap_or_else(|| (AccountState::new(account_id, now_ms), 0));
        let event = extract_event(&observation, config, state.cache_baseline_hit);
        let verdict = apply_event(&mut state, event, &observation, config);
        let alert = should_alert(&state, &verdict, config);
        let expected = (version != 0).then_some(version);
        match store::save_account(host, &state, expected).await {
            Ok(_) => {
                let _ = store::touch_index(host, account_id, now_ms).await;
                if alert {
                    fire_alert(app, host, config, &state, &verdict).await;
                }
                return Ok(());
            }
            Err(error) if error.code == ErrorCode::Conflict && attempts < 3 => continue,
            Err(error) => return Err(error),
        }
    }
}

/// 发送告警并回写投递结果；通知失败会记录到投递明细与日志，不回滚判定。
async fn fire_alert(
    _app: &App,
    host: &HostClient,
    config: &WatchConfig,
    state: &AccountState,
    verdict: &detector::Verdict,
) {
    let now_ms = state.last_observed_at_ms;
    if !config.enabled {
        return;
    }
    let (title, detail) = render_alert(state, verdict, config);
    let notice = Notice {
        title,
        detail,
        account_id: state.account_id.clone(),
        occurred_at_ms: now_ms,
        verdict: serde_json::to_value(verdict).unwrap_or_default(),
    };
    let deliveries = deliver_all(host, config, &notice).await;
    // 没有任何渠道配置时也记录告警，管理页仍能看到事件。
    let mut updated = state.clone();
    updated.last_alert_at_ms = now_ms;
    updated.status = AccountStatus::Degraded;
    updated.alerts.push(AlertRecord {
        at_ms: now_ms,
        verdict: verdict.clone(),
        deliveries: deliveries
            .iter()
            .map(|item| Delivery {
                channel: item.channel.clone(),
                ok: item.ok,
                detail: item.detail.clone(),
            })
            .collect(),
    });
    updated.prune(now_ms, config.window_ms);
    // 告警回写允许覆盖：同一观察链路上的告警只做一次，丢失不重复发送。
    let _ = store::save_account(host, &updated, None).await;
    for outcome in &deliveries {
        if !outcome.ok {
            log(
                host,
                "watch_notify_failed",
                &format!("{}: {}", outcome.channel, outcome.detail),
            )
            .await;
        }
    }
}

fn management(app: Arc<App>) -> impl Fn(ManagementCall) -> BoxFuture<ManagementResult> {
    move |call: ManagementCall| {
        let app = Arc::clone(&app);
        Box::pin(async move { management_handle(&app, call).await })
    }
}

async fn management_handle(
    app: &App,
    call: TypedCall<gateway_plugin_sdk::call::management::ManagementRequest>,
) -> Result<TypedReply<gateway_plugin_sdk::call::management::ManagementResponse>, PluginFault> {
    let request = call.request;
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "status") => {
            let index = store::load_index(&call.host).await.unwrap_or_default();
            let mut accounts = Vec::new();
            for item in index {
                let Some(id) = item.get("account_id").and_then(|id| id.as_str()) else {
                    continue;
                };
                if let Ok(Some((state, _))) = store::load_account(&call.host, id).await {
                    accounts.push(json!({
                        "account_id": state.account_id,
                        "provider": state.provider,
                        "model": state.model,
                        "status": status_label(state.status),
                        "verdict_streak": state.verdict_streak,
                        "last_observed_at_ms": state.last_observed_at_ms,
                        "last_alert_at_ms": state.last_alert_at_ms,
                        "window_events": state.events.len(),
                        "signaled_events": state.events.iter().filter(|event| !event.signals.is_empty()).count(),
                    }));
                }
            }
            json_response(
                200,
                json!({
                    "accounts": accounts,
                    "config": effective_config(app, &call.host).await.redacted(),
                }),
            )
        }
        ("GET", "config") => json_response(200, effective_config(app, &call.host).await.redacted()),
        ("GET", "settings") => {
            let settings = store::load_settings(&call.host)
                .await
                .ok()
                .flatten()
                .map(|(value, _)| value)
                .unwrap_or_else(|| json!({}));
            // 敏感值不回显，只回报是否已配置。
            json_response(
                200,
                json!({
                    "settings": {
                        "webhook_url": settings.get("webhook_url").cloned().unwrap_or_default(),
                        "webhook_format": settings.get("webhook_format").cloned().unwrap_or_default(),
                        "webhook_auth_header_configured": settings
                            .get("webhook_auth_header")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|value| !value.is_empty()),
                        "email_url": settings.get("email_url").cloned().unwrap_or_default(),
                        "email_format": settings.get("email_format").cloned().unwrap_or_default(),
                        "email_from": settings.get("email_from").cloned().unwrap_or_default(),
                        "email_to": settings.get("email_to").cloned().unwrap_or_else(|| json!([])),
                        "email_subject_template": settings
                            .get("email_subject_template")
                            .cloned()
                            .unwrap_or_default(),
                        "email_body_template": settings
                            .get("email_body_template")
                            .cloned()
                            .unwrap_or_default(),
                        "email_auth_header_configured": settings
                            .get("email_auth_header")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|value| !value.is_empty()),
                        "alert_message": settings.get("alert_message").cloned().unwrap_or_default(),
                    },
                }),
            )
        }
        ("POST", "settings") => {
            let incoming: serde_json::Value =
                serde_json::from_slice(&call.payload).unwrap_or_else(|_| json!({}));
            // 未提交的敏感字段保留原值；clear_* 标记用于显式清空。
            let mut settings = store::load_settings(&call.host)
                .await
                .ok()
                .flatten()
                .map(|(value, _)| value)
                .unwrap_or_else(|| json!({}));
            for key in [
                "webhook_url",
                "webhook_format",
                "email_url",
                "email_format",
                "email_from",
                "email_to",
                "email_subject_template",
                "email_body_template",
                "alert_message",
            ] {
                if let Some(value) = incoming.get(key) {
                    settings[key] = value.clone();
                }
            }
            for (key, clear) in [
                ("webhook_auth_header", "clear_webhook_auth"),
                ("email_auth_header", "clear_email_auth"),
            ] {
                if let Some(value) = incoming.get(key).and_then(serde_json::Value::as_str)
                    && !value.trim().is_empty()
                {
                    settings[key] = json!(value.trim());
                }
                if incoming
                    .get(clear)
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    settings[key] = json!("");
                }
            }
            match store::save_settings(&call.host, &settings, None).await {
                Ok(version) => json_response(200, json!({"ok": true, "version": version})),
                Err(error) => json_response(500, json!({"ok": false, "error": error.message})),
            }
        }
        ("GET", "events") => {
            let account = query_param(&request.query, "account").unwrap_or_default();
            let events = match store::load_account_view(&call.host, &account).await {
                Ok(Some(state)) => state.get("events").cloned().unwrap_or_else(|| json!([])),
                _ => json!([]),
            };
            json_response(200, json!({"account": account, "events": events}))
        }
        ("GET", "alerts") => {
            let account = query_param(&request.query, "account").unwrap_or_default();
            let alerts = match store::load_account_view(&call.host, &account).await {
                Ok(Some(state)) => state.get("alerts").cloned().unwrap_or_else(|| json!([])),
                _ => json!([]),
            };
            json_response(200, json!({"account": account, "alerts": alerts}))
        }
        ("POST", "test-notify") => {
            let notice = Notice {
                title: "Codex 降智监控测试通知".to_owned(),
                detail: "这是一条测试消息：配置已保存且通知链路可用。".to_owned(),
                account_id: "test".to_owned(),
                occurred_at_ms: current_ms(),
                verdict: json!({
                    "degraded": false,
                    "signal_kinds": [],
                    "signaled_requests": 0,
                    "last_signal_at_ms": 0,
                }),
            };
            let deliveries = deliver_all(
                &call.host,
                &effective_config(app, &call.host).await,
                &notice,
            )
            .await;
            let body = json!({
                "ok": deliveries.iter().any(|item| item.ok),
                "deliveries": deliveries.iter().map(|item| json!({
                    "channel": item.channel,
                    "ok": item.ok,
                    "detail": item.detail,
                })).collect::<Vec<_>>(),
            });
            json_response(200, body)
        }
        _ => json_response(404, json!({"error": "unknown management route"})),
    }
}

fn json_response(
    status: u16,
    body: serde_json::Value,
) -> Result<TypedReply<gateway_plugin_sdk::call::management::ManagementResponse>, PluginFault> {
    Ok(
        TypedReply::new(gateway_plugin_sdk::call::management::ManagementResponse {
            status,
            content_type: "application/json".to_owned(),
        })
        .with_payload(serde_json::to_vec(&body).unwrap_or_default()),
    )
}

fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if parts.next() == Some(key) {
            return Some(percent_decode(parts.next().unwrap_or_default()));
        }
    }
    None
}

/// 只解码 %XX 与 `+`，管理页查询参数足够使用。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    output.push(value);
                    index += 3;
                } else {
                    output.push(b'%');
                    index += 1;
                }
            }
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn current_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

async fn log(host: &HostClient, event: &str, message: &str) {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("message".to_owned(), json!(message));
    let _ = host
        .call(
            "host.log",
            json!({
                "event": event,
                "level": "warn",
                "fields": fields,
            }),
            Vec::new(),
        )
        .await;
}

fn status_label(status: AccountStatus) -> &'static str {
    match status {
        AccountStatus::Unknown => "unknown",
        AccountStatus::Healthy => "healthy",
        AccountStatus::Suspect => "suspect",
        AccountStatus::Degraded => "degraded",
    }
}

fn management_registration() -> ManagementRegistration {
    ManagementRegistration {
        routes: vec![
            ManagementRoute {
                method: "GET".to_owned(),
                path: "status".to_owned(),
                request_content_types: vec![],
                response_content_types: vec!["application/json".to_owned()],
            },
            ManagementRoute {
                method: "GET".to_owned(),
                path: "config".to_owned(),
                request_content_types: vec![],
                response_content_types: vec!["application/json".to_owned()],
            },
            ManagementRoute {
                method: "GET".to_owned(),
                path: "events".to_owned(),
                request_content_types: vec![],
                response_content_types: vec!["application/json".to_owned()],
            },
            ManagementRoute {
                method: "GET".to_owned(),
                path: "alerts".to_owned(),
                request_content_types: vec![],
                response_content_types: vec!["application/json".to_owned()],
            },
            ManagementRoute {
                method: "GET".to_owned(),
                path: "settings".to_owned(),
                request_content_types: vec![],
                response_content_types: vec!["application/json".to_owned()],
            },
            ManagementRoute {
                method: "POST".to_owned(),
                path: "settings".to_owned(),
                request_content_types: vec!["application/json".to_owned()],
                response_content_types: vec!["application/json".to_owned()],
            },
            ManagementRoute {
                method: "POST".to_owned(),
                path: "test-notify".to_owned(),
                request_content_types: vec!["application/json".to_owned()],
                response_content_types: vec!["application/json".to_owned()],
            },
        ],
        resources: vec![
            ManagementResource {
                path: "ui/index.html".to_owned(),
                public: false,
            },
            ManagementResource {
                path: "ui/app.js".to_owned(),
                public: false,
            },
            ManagementResource {
                path: "ui/style.css".to_owned(),
                public: false,
            },
            ManagementResource {
                path: "ui/icon.svg".to_owned(),
                public: true,
            },
        ],
        pages: vec![ManagementPage {
            id: "status".to_owned(),
            title: "降智监控".to_owned(),
            description: Some("查看账号降智信号、告警历史并发送测试通知".to_owned()),
            entry: "ui/index.html".to_owned(),
            icon: Some("ui/icon.svg".to_owned()),
        }],
        callbacks: vec![],
    }
}

#[cfg(test)]
mod tests {
    use gateway_plugin_sdk::{Capability, Manifest, Permission};

    #[test]
    fn manifest_parses_and_declares_permissions() {
        let manifest = Manifest::from_author_slice(include_bytes!("../plugin.json")).unwrap();
        assert_eq!(manifest.manifest_version, 1);
        assert!(manifest.permissions.contains(&Permission::Network));
        assert!(manifest.permissions.contains(&Permission::Requests));
        assert!(manifest.permissions.contains(&Permission::PublicEndpoints));
        // 清单中保留 request lifecycle、usage、management 三类贡献点。
        for capability in [
            Capability::RequestLifecycle,
            Capability::Usage,
            Capability::Management,
        ] {
            assert!(
                manifest.contributes.contains_key(&capability),
                "{capability:?}"
            );
        }
    }
}
