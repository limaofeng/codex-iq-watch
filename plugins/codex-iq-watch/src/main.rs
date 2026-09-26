//! Codex 降智监控插件入口：观察请求终态信号、判定降智并发送 Webhook/邮件通知，附带管理页面 API。

mod config;
mod detector;
mod notify;
mod scheduler;
mod store;
mod time_util;

use std::sync::Arc;

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        management::{ManagementPage, ManagementRegistration, ManagementResource, ManagementRoute},
        policy::{AccountScheduleRequest, ObserveRequest},
    },
    client::{Empty, HostClient, PluginBuilder, SessionConfig, TypedCall, TypedReply, methods},
};
use serde_json::json;

use crate::{
    config::WatchConfig,
    detector::{
        AccountState, AccountStatus, AlertRecord, CandyProbe, Delivery, apply_event, extract_event,
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
        .and_then(|builder| builder.on(methods::SCHEDULE_ACCOUNT, schedule(app.clone())))
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
type ScheduleResult =
    Result<TypedReply<gateway_plugin_sdk::call::policy::AccountScheduleDecision>, PluginFault>;
type ManagementCall = TypedCall<gateway_plugin_sdk::call::management::ManagementRequest>;
type ManagementResult =
    Result<TypedReply<gateway_plugin_sdk::call::management::ManagementResponse>, PluginFault>;

fn observe(app: Arc<App>) -> impl Fn(TypedCall<ObserveRequest>) -> BoxFuture<ObserveResult> {
    move |call: TypedCall<ObserveRequest>| {
        let app = Arc::clone(&app);
        Box::pin(async move { observe_request(&app, call).await })
    }
}

/// 调度回调入口：绑定「账号调度」阶段后宿主每次选号都会先问插件。
fn schedule(
    app: Arc<App>,
) -> impl Fn(TypedCall<AccountScheduleRequest>) -> BoxFuture<ScheduleResult> {
    move |call: TypedCall<AccountScheduleRequest>| {
        let app = Arc::clone(&app);
        Box::pin(async move { scheduler::schedule_account(&app, call).await })
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
        // 每次观察都按宿主最新投影刷新显示名（改名/补邮箱即时生效）；
        // 解析失败保留旧值，仍由通知与管理页回退为内部 ID。
        if let Ok(account) = store::account_runtime(host, account_id).await {
            let resolved = store::runtime_label(&account);
            if resolved != state.display_name {
                state.display_name = resolved;
            }
        }
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
/// 先用 CAS 抢占冷却位再发送：并发观察下只有一个链路会真正发出告警，
/// `last_alert_at_ms` 丢失曾导致同账号在冷却期内重复推送。
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
    // 阶段一：抢占告警位。回写失败（含状态被清空）视为另一条链路已接手，直接放弃。
    let mut claimed = state.clone();
    for _ in 0..3 {
        let Ok(Some((fresh, version))) = store::load_account(host, &state.account_id).await else {
            return;
        };
        // 冷却位已被更新（其他链路已告警），跳过本次发送。
        if fresh.last_alert_at_ms != state.last_alert_at_ms {
            return;
        }
        let mut candidate = fresh.clone();
        candidate.last_alert_at_ms = now_ms;
        let expected = (version != 0).then_some(version);
        match store::save_account(host, &candidate, expected).await {
            Ok(_) => {
                claimed = candidate;
                break;
            }
            Err(error) if error.code == ErrorCode::Conflict => continue,
            Err(_) => return,
        }
    }
    if claimed.last_alert_at_ms != now_ms {
        return;
    }

    let (title, detail) = render_alert(&claimed, verdict, config);
    let notice = Notice {
        title,
        detail,
        account_id: state.account_id.clone(),
        occurred_at_ms: now_ms,
        verdict: serde_json::to_value(verdict).unwrap_or_default(),
    };
    let deliveries = deliver_all(host, config, &notice).await;
    // 阶段二：把告警与投递明细追加进历史；失败只丢记录，冷却位已生效，不重复发送。
    for _ in 0..3 {
        let Ok(Some((mut fresh, version))) = store::load_account(host, &state.account_id).await
        else {
            break;
        };
        fresh.status = AccountStatus::Degraded;
        fresh.alerts.push(AlertRecord {
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
        fresh.prune(now_ms, config.window_ms);
        let expected = (version != 0).then_some(version);
        match store::save_account(host, &fresh, expected).await {
            Ok(_) => break,
            Err(error) if error.code == ErrorCode::Conflict => continue,
            Err(_) => break,
        }
    }
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
            // 账号列表以宿主账号全集为基底（不依赖历史索引，索引曾为空导致列表不可见），
            // 观察到的窗口状态与告警叠加其上；name/email 用最新投影覆盖存储值。
            let (runtime, accounts_error) = match store::list_runtime_accounts(&call.host).await {
                Ok(accounts) => (accounts, serde_json::Value::Null),
                Err(error) => (
                    std::collections::BTreeMap::new(),
                    json!(format!(
                        "无法列举宿主账号（{}），仅显示已观察到的账号",
                        error.message
                    )),
                ),
            };
            let index = store::load_index(&call.host).await.unwrap_or_default();
            let mut states = std::collections::BTreeMap::new();
            for item in index {
                let Some(id) = item.get("account_id").and_then(|id| id.as_str()) else {
                    continue;
                };
                if let Ok(Some((state, _))) = store::load_account(&call.host, id).await {
                    states.insert(id.to_owned(), state);
                }
            }
            let mut ids: Vec<String> = runtime.keys().cloned().collect();
            for id in states.keys() {
                if !runtime.contains_key(id) {
                    ids.push(id.clone());
                }
            }
            let mut accounts = Vec::new();
            let mut alerts = Vec::new();
            for id in ids {
                let name = runtime
                    .get(&id)
                    .and_then(store::runtime_label)
                    .or_else(|| states.get(&id).and_then(|state| state.display_name.clone()));
                let (status, state) = match states.get(&id) {
                    Some(state) => (status_label(state.status), Some(state)),
                    None => ("unobserved", None),
                };
                let enabled = runtime.get(&id).map(|account| account.enabled);
                accounts.push(json!({
                    "account_id": id,
                    "name": name,
                    "provider": state.and_then(|s| s.provider.clone())
                        .or_else(|| runtime.get(&id).map(|a| a.provider_id.clone())),
                    "model": state.and_then(|s| s.model.clone()),
                    "status": status,
                    "enabled": enabled,
                    "verdict_streak": state.map_or(0, |s| s.verdict_streak),
                    "last_observed_at_ms": state.map_or(0, |s| s.last_observed_at_ms),
                    "last_alert_at_ms": state.map_or(0, |s| s.last_alert_at_ms),
                    "window_events": state.map_or(0, |s| s.events.len()),
                    "signaled_events": state.map_or(0, |s| s.events.iter().filter(|e| !e.signals.is_empty()).count()),
                    "last_probe": state.and_then(|s| s.last_probe.clone()),
                }));
                if let Some(state) = state {
                    for alert in &state.alerts {
                        alerts.push(json!({
                            "account_id": state.account_id,
                            "name": name,
                            "at_ms": alert.at_ms,
                            "verdict": alert.verdict,
                            "deliveries": alert.deliveries,
                        }));
                    }
                }
            }
            // 严重程度优先，其次最近观察时间，便于一眼定位问题账号。
            accounts.sort_by(|left, right| {
                let rank = |item: &serde_json::Value| match item
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                {
                    Some("degraded") => 0,
                    Some("suspect") => 1,
                    Some("healthy") => 2,
                    _ => 3,
                };
                rank(left).cmp(&rank(right)).then_with(|| {
                    right
                        .get("last_observed_at_ms")
                        .and_then(serde_json::Value::as_u64)
                        .cmp(
                            &left
                                .get("last_observed_at_ms")
                                .and_then(serde_json::Value::as_u64),
                        )
                })
            });
            alerts.sort_by_key(|item| {
                item.get("at_ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
            });
            json_response(
                200,
                json!({
                    "accounts": accounts,
                    "alerts": alerts,
                    "accounts_error": accounts_error,
                    "config": effective_config(app, &call.host).await.redacted(),
                }),
            )
        }
        ("POST", "account-clear") => {
            let incoming: serde_json::Value =
                serde_json::from_slice(&call.payload).unwrap_or_else(|_| json!({}));
            let account = incoming
                .get("account")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if account.is_empty() {
                return json_response(400, json!({"ok": false, "error": "missing account"}));
            }
            match store::delete_account(&call.host, &account).await {
                Ok(deleted) => json_response(200, json!({"ok": true, "deleted": deleted})),
                Err(error) => json_response(500, json!({"ok": false, "error": error.message})),
            }
        }
        ("GET", "models") => {
            // 汇总所有启用 Key 可见的模型；同时返回 Key 列表供管理页选择。
            let Ok(keys) = store::list_client_keys(&call.host).await else {
                return json_response(
                    200,
                    json!({"models": [], "keys": [], "error": "无法读取客户端 Key 列表（需要 models 权限）"}),
                );
            };
            let mut models = Vec::<serde_json::Value>::new();
            let mut key_list = Vec::<serde_json::Value>::new();
            let mut seen = std::collections::BTreeSet::new();
            for key in keys.iter().filter(|key| key.enabled) {
                let Ok(names) = store::list_models(&call.host, &key.id).await else {
                    continue;
                };
                let display = if key.name.trim().is_empty() {
                    key.id.clone()
                } else {
                    key.name.clone()
                };
                key_list.push(json!({
                    "id": key.id,
                    "name": display,
                    "models": names,
                }));
                for name in names {
                    if seen.insert(name.clone()) {
                        models.push(json!({"model": name, "key": key.id}));
                    }
                }
            }
            json_response(200, json!({"models": models, "keys": key_list}))
        }
        ("POST", "candy-test") => {
            let incoming: serde_json::Value =
                serde_json::from_slice(&call.payload).unwrap_or_else(|_| json!({}));
            let account = incoming
                .get("account")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let model = incoming
                .get("model")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let key = incoming
                .get("key")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let effort = incoming
                .get("effort")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .filter(|value| !value.is_empty() && value != "default");
            if account.is_empty() || model.is_empty() {
                return json_response(
                    400,
                    json!({"ok": false, "error": "missing account or model"}),
                );
            }
            json_response(
                200,
                run_candy_test(&call.host, &account, &model, key, effort).await,
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
            // 敏感值不回显，只回报是否已配置；检测参数回填生效值供编辑。
            let effective = effective_config(app, &call.host).await;
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
                    "effective": effective.redacted(),
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
            // 检测参数覆盖：JSON null 表示清除覆盖、回到宿主配置。
            for key in [
                "enabled",
                "watch_providers",
                "window_ms",
                "cooldown_ms",
                "first_token_ms",
                "cache_min_input_tokens",
                "min_signaled_requests",
                "min_signal_kinds",
                "consecutive_triggers",
                "schedule_exclude_degraded",
                "schedule_all_degraded",
            ] {
                match incoming.get(key) {
                    Some(serde_json::Value::Null) => {
                        if let Some(map) = settings.as_object_mut() {
                            map.remove(key);
                        }
                    }
                    Some(value) => {
                        settings[key] = value.clone();
                    }
                    None => {}
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
            match store::save_settings(&call.host, &settings).await {
                Ok(version) => json_response(200, json!({"ok": true, "version": version})),
                Err(error) => json_response(500, json!({"ok": false, "error": error.message})),
            }
        }
        ("POST", "settings-reset") => match store::delete_settings(&call.host).await {
            Ok(()) => json_response(200, json!({"ok": true})),
            Err(error) => json_response(500, json!({"ok": false, "error": error.message})),
        },
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

/// 糖果题：要求模型直接回答摸球/取球类保证性问题的最小数量，正确答案是 21。
/// 侧重判断模型是否给出完整且正确的推理结果，属于经验性探针，不等价于官方降智结论。
const CANDY_QUESTION: &str = "不使用任何外部工具回答以下问题：在一个黑色的袋子里放有三种口味的糖果，每种糖果有两种不同的形状（圆形和五角星形，不同的形状靠手感可以分辨）。数量如下：苹果味圆形7、桃子味圆形9、西瓜味圆形8；苹果味五角星7、桃子味五角星6、西瓜味五角星4。参赛者需在活动前决定摸出的糖果数目。问：最少取出多少个糖果，才能保证手中同时拥有不同形状的苹果味和桃子味的糖？\n直接回答最终结果";
const CANDY_ANSWER: u64 = 21;

/// 选择测试用 Key：优先指定，否则取第一个启用 Key。
async fn pick_test_key(host: &HostClient, want: Option<String>) -> Option<String> {
    if let Some(key) = want.filter(|key| !key.is_empty()) {
        return Some(key);
    }
    store::list_client_keys(host)
        .await
        .ok()?
        .into_iter()
        .find(|key| key.enabled)
        .map(|key| key.id)
}

/// 发送糖果题并判定答案；结果写回账号状态的 `last_probe`，供管理页标记。
async fn run_candy_test(
    host: &HostClient,
    account_id: &str,
    model: &str,
    key: Option<String>,
    effort: Option<String>,
) -> serde_json::Value {
    let Some(key_id) = pick_test_key(host, key).await else {
        return json!({
            "ok": false,
            "result": "failed",
            "detail": "没有可用的客户端 Key；先在宿主创建至少一个启用的 API Key",
        });
    };
    // reasoning effort 仅在显式选择时透传；default 交给宿主/模型默认值。
    let body = match &effort {
        Some(effort) => json!({
            "model": model,
            "input": CANDY_QUESTION,
            "store": false,
            "reasoning": {"effort": effort},
        }),
        None => json!({
            "model": model,
            "input": CANDY_QUESTION,
            "store": false,
        }),
    };
    let outcome = match store::execute_model(host, &key_id, model, account_id, &body).await {
        Ok(events) => {
            let text = collect_text(&events);
            let digits = all_digits(&text);
            if digits.is_empty() {
                (
                    "failed".to_owned(),
                    "响应中没有可解析的数字".to_owned(),
                    text,
                )
            } else if digits.contains(&CANDY_ANSWER) {
                (
                    "correct".to_owned(),
                    format!("命中正确答案 {CANDY_ANSWER}"),
                    text,
                )
            } else {
                (
                    "wrong".to_owned(),
                    format!(
                        "回答 {}，正确答案应为 {}",
                        digits
                            .iter()
                            .map(u64::to_string)
                            .collect::<Vec<_>>()
                            .join("/"),
                        CANDY_ANSWER
                    ),
                    text,
                )
            }
        }
        Err(error) => ("failed".to_owned(), error.message.clone(), String::new()),
    };
    let probe = CandyProbe {
        at_ms: current_ms(),
        model: model.to_owned(),
        result: outcome.0.clone(),
        detail: outcome.1.clone(),
        effort: effort.clone(),
    };
    // 测试记录允许覆盖写入：保存失败只影响展示，不影响判定结果返回。
    for _ in 0..3 {
        let Ok(existing) = store::load_account(host, account_id).await else {
            break;
        };
        let (mut state, version) =
            existing.unwrap_or_else(|| (AccountState::new(account_id, probe.at_ms), 0));
        state.last_probe = Some(probe.clone());
        let expected = (version != 0).then_some(version);
        match store::save_account(host, &state, expected).await {
            Ok(_) => break,
            Err(error) if error.code == ErrorCode::Conflict => continue,
            Err(_) => break,
        }
    }
    let _ = store::touch_index(host, account_id, probe.at_ms).await;
    json!({
        "ok": outcome.0 != "failed",
        "result": outcome.0,
        "detail": outcome.1,
        "answer": CANDY_ANSWER,
        "model": model,
        "key": key_id,
        "effort": effort,
        "excerpt": truncate_chars(&outcome.2, 600),
        "recorded": true,
    })
}

/// 从执行事件合并文本增量；同时取最大用量摘要供展示。
fn collect_text(events: &[gateway_plugin_sdk::call::model::ExecutionEvent]) -> String {
    use gateway_plugin_sdk::call::model::CanonicalEvent;
    let mut text = String::new();
    for event in events {
        for fact in &event.facts {
            match fact {
                CanonicalEvent::TextDelta { text: delta, .. }
                | CanonicalEvent::ReasoningDelta { text: delta, .. } => {
                    // 推理过程不计入答案；只收集正式输出文本。
                    if matches!(fact, CanonicalEvent::TextDelta { .. }) {
                        text.push_str(delta);
                    }
                }
                _ => {}
            }
        }
        // wire 回退：没有 canonical 增量时，从 openai 协议事件里取 output_text。
        if text.is_empty()
            && let Some(wire) = &event.wire
            && let gateway_plugin_sdk::call::model::WirePayload::Json { event, data, .. } =
                &wire.payload
        {
            let kind = event.as_deref().unwrap_or_default();
            if kind == "response.output_text.delta"
                && let Some(delta) = data.get("delta").and_then(serde_json::Value::as_str)
            {
                text.push_str(delta);
            }
            if kind == "response.completed"
                && let Some(output_text) = data
                    .pointer("/response/output/0/content/0/text")
                    .and_then(serde_json::Value::as_str)
            {
                text.push_str(output_text);
            }
        }
    }
    text
}

/// 提取文本中的数字序列：答案可能是 "21"、"21个"、"最少21"。
fn all_digits(text: &str) -> Vec<u64> {
    let mut values = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if !current.is_empty() {
            if let Ok(value) = current.parse::<u64>() {
                values.push(value);
            }
            current.clear();
        }
    }
    if !current.is_empty()
        && let Ok(value) = current.parse::<u64>()
    {
        values.push(value);
    }
    values
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    text.chars().take(max).collect::<String>() + "…"
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
                path: "settings-reset".to_owned(),
                request_content_types: vec!["application/json".to_owned()],
                response_content_types: vec!["application/json".to_owned()],
            },
            ManagementRoute {
                method: "POST".to_owned(),
                path: "account-clear".to_owned(),
                request_content_types: vec!["application/json".to_owned()],
                response_content_types: vec!["application/json".to_owned()],
            },
            ManagementRoute {
                method: "GET".to_owned(),
                path: "models".to_owned(),
                request_content_types: vec![],
                response_content_types: vec!["application/json".to_owned()],
            },
            ManagementRoute {
                method: "POST".to_owned(),
                path: "candy-test".to_owned(),
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
        assert!(manifest.permissions.contains(&Permission::Accounts));
        assert!(manifest.permissions.contains(&Permission::Models));
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
