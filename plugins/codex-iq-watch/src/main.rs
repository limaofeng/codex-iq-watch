//! Codex 降智监控插件入口：观察请求终态信号、判定降智并发送 Webhook/邮件通知，附带管理页面 API。

mod config;
mod detector;
mod notify;
mod scheduler;
mod store;
mod time_util;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

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
    /// 宿主没有配额查询接口；本进程见到容量错误后持续提示，成功手动清理才解除。
    storage_capacity_failed: AtomicBool,
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
    let app = Arc::new(App {
        config,
        storage_capacity_failed: AtomicBool::new(false),
    });

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
        if error.code == ErrorCode::Capacity {
            app.storage_capacity_failed.store(true, Ordering::Relaxed);
        }
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

/// 告警与冷却位一起落盘后才发送，父调用超时也保留「结果未确认」记录。
/// 宿主不提供独立后台回调；未知结果不能自动重发，否则可能重复通知。
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
    let mut claimed = None;
    for _ in 0..3 {
        let Ok(Some((fresh, version))) = store::load_account(host, &state.account_id).await else {
            return;
        };
        // 冷却位已被更新（其他链路已告警），跳过本次发送。
        if fresh.last_alert_at_ms != state.last_alert_at_ms
            || fresh.status != AccountStatus::Degraded
        {
            return;
        }
        let mut candidate = fresh;
        candidate.last_alert_at_ms = now_ms;
        candidate.alerts.push(AlertRecord {
            at_ms: now_ms,
            verdict: verdict.clone(),
            deliveries: pending_deliveries(config),
        });
        candidate.prune(candidate.last_observed_at_ms, config.window_ms);
        let expected = (version != 0).then_some(version);
        match store::save_account(host, &candidate, expected).await {
            Ok(_) => {
                claimed = Some(candidate);
                break;
            }
            Err(error) if error.code == ErrorCode::Conflict => continue,
            Err(_) => return,
        }
    }
    let Some(claimed) = claimed else {
        return;
    };

    let (title, detail) = render_alert(&claimed, verdict, config);
    let notice = Notice {
        title,
        detail,
        account_id: state.account_id.clone(),
        occurred_at_ms: now_ms,
        verdict: serde_json::to_value(verdict).unwrap_or_default(),
    };
    let deliveries = deliver_all(host, config, &notice).await;
    // 只补写原告警的投递结果，不覆盖期间已更新的账号判定，也不复活已清除的告警。
    for _ in 0..3 {
        let Ok(Some((mut fresh, version))) = store::load_account(host, &state.account_id).await
        else {
            break;
        };
        if !complete_deliveries(&mut fresh, now_ms, &deliveries) {
            break;
        }
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

fn pending_deliveries(config: &WatchConfig) -> Vec<Delivery> {
    let mut channels = Vec::new();
    if !config.webhook_url.is_empty() {
        channels.push("webhook");
    }
    if !config.email_url.is_empty() && !config.email_to.is_empty() {
        channels.push("email");
    }
    channels
        .into_iter()
        .map(|channel| Delivery {
            channel: channel.to_owned(),
            ok: false,
            detail: "投递结果未确认：可能尚未发送、仍在发送或观察调用已超时，不自动重发".to_owned(),
        })
        .collect()
}

fn complete_deliveries(
    state: &mut AccountState,
    at_ms: u64,
    deliveries: &[notify::DeliveryOutcome],
) -> bool {
    let Some(alert) = state.alerts.iter_mut().find(|alert| alert.at_ms == at_ms) else {
        return false;
    };
    alert.deliveries = deliveries
        .iter()
        .map(|item| Delivery {
            channel: item.channel.clone(),
            ok: item.ok,
            detail: item.detail.clone(),
        })
        .collect();
    true
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
            let mut storage_warnings = Vec::new();
            let index = match store::load_index(&call.host).await {
                Ok(index) => index,
                Err(_) => {
                    storage_warnings.push("读取状态索引失败，仍尝试读取宿主现有账号的状态");
                    Vec::new()
                }
            };
            let indexed_ids: std::collections::BTreeSet<String> = index
                .iter()
                .filter_map(|item| item.get("account_id").and_then(|id| id.as_str()))
                .map(str::to_owned)
                .collect();
            // 旧版索引曾只保留 64 个账号；按宿主全集补查，不能把仍被隔离的账号显示为未观察。
            let ids: std::collections::BTreeSet<String> =
                runtime.keys().chain(indexed_ids.iter()).cloned().collect();
            let mut states = std::collections::BTreeMap::new();
            let mut unreadable = std::collections::BTreeSet::new();
            for id in &ids {
                match store::load_account(&call.host, id).await {
                    Ok(Some((state, _))) => {
                        states.insert(id.clone(), state);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        unreadable.insert(id.clone());
                    }
                }
            }
            if !unreadable.is_empty() {
                storage_warnings.push("部分账号状态读取失败，显示为未知，不代表账号正常");
            }
            if indexed_ids.len() >= store::MAX_ACCOUNTS
                || states.len() >= store::MAX_ACCOUNTS
                || runtime.len() > store::MAX_ACCOUNTS
            {
                storage_warnings.push("最多保存 254 个账号状态，容量满后新账号无法记录；保留既有隔离状态，请手动清除不再需要的记录");
            }
            if app.storage_capacity_failed.load(Ordering::Relaxed) {
                storage_warnings
                    .push("状态写入遇到记录或字节配额不足，部分观察未保存；请清理不再需要的记录");
            }
            let storage_warning =
                (!storage_warnings.is_empty()).then(|| storage_warnings.join(" / "));
            let mut accounts = Vec::new();
            let mut alerts = Vec::new();
            for id in ids {
                let name = runtime
                    .get(&id)
                    .and_then(store::runtime_label)
                    .or_else(|| states.get(&id).and_then(|state| state.display_name.clone()));
                let (status, state) = match states.get(&id) {
                    Some(state) => (status_label(state.status), Some(state)),
                    None if unreadable.contains(&id) => ("unknown", None),
                    None => ("unobserved", None),
                };
                let enabled = runtime.get(&id).map(|account| account.enabled);
                // 排除生效：已开启排除调度且该账号当前为 degraded（被过滤出候选）。
                let excluded = app.config.schedule_exclude_degraded
                    && states
                        .get(&id)
                        .is_some_and(|s| s.status == AccountStatus::Degraded);
                accounts.push(json!({
                    "account_id": id,
                    "name": name,
                    "provider": state.and_then(|s| s.provider.clone())
                        .or_else(|| runtime.get(&id).map(|a| a.provider_id.clone())),
                    "model": state.and_then(|s| s.model.clone()),
                    "status": status,
                    "enabled": enabled,
                    "excluded_from_schedule": excluded,
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
                    "storage_warning": storage_warning,
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
                Ok(deleted) => {
                    app.storage_capacity_failed.store(false, Ordering::Relaxed);
                    json_response(200, json!({"ok": true, "deleted": deleted}))
                }
                Err(error) => json_response(500, json!({"ok": false, "error": error.message})),
            }
        }
        // 恢复调度：清除降智/疑似标记但不删除观察历史，账号回到监控池继续被观察。
        ("POST", "account-resume") => {
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
            for _ in 0..3 {
                let Some((mut state, version)) = (match store::load_account(&call.host, &account)
                    .await
                {
                    Ok(found) => found,
                    Err(error) => {
                        return json_response(500, json!({"ok": false, "error": error.message}));
                    }
                }) else {
                    return json_response(
                        404,
                        json!({"ok": false, "error": "no state for this account"}),
                    );
                };
                if state.status != AccountStatus::Degraded && state.status != AccountStatus::Suspect
                {
                    return json_response(
                        200,
                        json!({"ok": true, "status": status_label(state.status)}),
                    );
                }
                state.status = AccountStatus::Healthy;
                state.verdict_streak = 0;
                let expected = (version != 0).then_some(version);
                match store::save_account(&call.host, &state, expected).await {
                    Ok(_) => {
                        return json_response(200, json!({"ok": true, "status": "healthy"}));
                    }
                    Err(error) if error.code == ErrorCode::Conflict => continue,
                    Err(error) => {
                        return json_response(500, json!({"ok": false, "error": error.message}));
                    }
                }
            }
            json_response(
                409,
                json!({"ok": false, "error": "status write conflicted, retry"}),
            )
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
            let incoming: serde_json::Value = match serde_json::from_slice(&call.payload) {
                Ok(value) => value,
                Err(_) => return json_response(400, json!({"error": "invalid JSON body"})),
            };
            if !incoming.is_object() {
                return json_response(400, json!({"error": "settings body must be a JSON object"}));
            }
            if let Err(message) = validate_settings(&incoming) {
                return json_response(400, json!({"error": message}));
            }
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
                match incoming.get(key) {
                    // 与检测参数一致：null 表示移除覆盖、回到宿主配置。
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
            // 检测参数覆盖：JSON null 表示清除覆盖、回到宿主配置。
            for key in [
                "enabled",
                "watch_providers",
                "window_ms",
                "sample_requests",
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

/// 保存前校验管理页设置：非法值返回 400，不静默覆盖已有配置。
fn validate_settings(incoming: &serde_json::Value) -> Result<(), String> {
    // 字符串与数组字段的类型检查。
    let string_fields = [
        "webhook_url",
        "webhook_format",
        "webhook_auth_header",
        "email_url",
        "email_format",
        "email_from",
        "email_subject_template",
        "email_body_template",
        "email_auth_header",
        "alert_message",
        "schedule_all_degraded",
    ];
    for key in string_fields {
        match incoming.get(key) {
            None | Some(serde_json::Value::Null) => {}
            Some(value) if !value.is_string() => {
                return Err(format!("{key} must be a string"));
            }
            Some(value) => {
                let limit = match key {
                    "alert_message" => 1024,
                    "email_subject_template" | "email_body_template" => 4096,
                    "email_from" => 256,
                    "webhook_url" | "email_url" => 2048,
                    "webhook_auth_header" | "email_auth_header" => 4096,
                    _ => 64,
                };
                if value.as_str().unwrap_or_default().len() > limit {
                    return Err(format!("{key} must be at most {limit} bytes"));
                }
            }
        }
    }
    let enum_fields: &[(&str, &[&str])] = &[
        (
            "webhook_format",
            &["generic", "wecom", "dingtalk", "feishu", "slack", "bark"],
        ),
        (
            "email_format",
            &["generic", "resend", "postmark", "sendgrid"],
        ),
        ("schedule_all_degraded", &["delegate", "reject"]),
    ];
    for (key, allowed) in enum_fields {
        if let Some(value) = incoming.get(*key).and_then(|v| v.as_str())
            && !value.is_empty()
            && !allowed.contains(&value)
        {
            return Err(format!("{key} must be one of {}", allowed.join("/")));
        }
    }
    for key in ["webhook_url", "email_url"] {
        if let Some(url) = incoming.get(key).and_then(|v| v.as_str()) {
            let url = url.trim();
            if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(format!("{key} must be an http(s) URL"));
            }
        }
    }
    for key in ["webhook_auth_header", "email_auth_header"] {
        if let Some(value) = incoming.get(key).and_then(|v| v.as_str()) {
            let value = value.trim();
            if !value.is_empty() && WatchConfig::auth_header(value).is_none() {
                return Err(format!("{key} must use 'Header-Name: value' format"));
            }
        }
    }
    // 数字字段：类型检查 + 与 normalized() 一致的范围校验。
    let numeric_fields: &[(&str, u64, u64)] = &[
        ("window_ms", 60_000, 3_600_000),
        ("sample_requests", 4, 40),
        ("cooldown_ms", 60_000, 86_400_000),
        ("first_token_ms", 500, 120_000),
        ("latency_ms", 500, 120_000),
        ("cache_min_input_tokens", 0, 1_000_000),
        ("min_signaled_requests", 2, 20),
        ("min_signal_kinds", 1, 5),
        ("consecutive_triggers", 1, 10),
    ];
    for (key, min, max) in numeric_fields {
        match incoming.get(*key) {
            None | Some(serde_json::Value::Null) => {}
            Some(value) if !value.is_u64() => {
                return Err(format!("{key} must be a non-negative integer"));
            }
            Some(value) => {
                let number = value.as_u64().unwrap_or(0);
                if number < *min || number > *max {
                    return Err(format!("{key} must be in [{min}, {max}]"));
                }
            }
        }
    }
    for key in [
        "enabled",
        "schedule_exclude_degraded",
        "clear_webhook_auth",
        "clear_email_auth",
    ] {
        match incoming.get(key) {
            None | Some(serde_json::Value::Null) => {}
            Some(value) if !value.is_boolean() => {
                return Err(format!("{key} must be a boolean"));
            }
            _ => {}
        }
    }
    for key in ["watch_providers", "email_to"] {
        match incoming.get(key) {
            None | Some(serde_json::Value::Null) => {}
            Some(value) => {
                let Some(items) = value.as_array() else {
                    return Err(format!("{key} must be an array"));
                };
                if items.iter().any(|item| !item.is_string()) {
                    return Err(format!("{key} must contain only strings"));
                }
                if items.len() > 16 {
                    return Err(format!("{key} must have at most 16 items"));
                }
                let item_limit = if key == "email_to" { 256 } else { 64 };
                if items
                    .iter()
                    .any(|item| item.as_str().unwrap_or_default().len() > item_limit)
                {
                    return Err(format!("{key} items must be at most {item_limit} bytes"));
                }
            }
        }
    }
    Ok(())
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
    use super::*;
    use gateway_plugin_sdk::{Capability, Manifest, Permission};

    #[test]
    fn unconfirmed_deliveries_are_persistable_for_all_configured_channels() {
        let config = WatchConfig {
            webhook_url: "https://example.test/webhook".to_owned(),
            email_url: "https://example.test/mail".to_owned(),
            email_to: vec!["test@example.test".to_owned()],
            ..WatchConfig::default()
        };
        let deliveries = pending_deliveries(&config);
        assert_eq!(deliveries.len(), 2);
        assert!(
            deliveries
                .iter()
                .all(|item| !item.ok && item.detail.contains("未确认"))
        );
        assert!(pending_deliveries(&WatchConfig::default()).is_empty());
    }

    #[test]
    fn delivery_completion_does_not_change_newer_health_or_recreate_cleared_alert() {
        let mut state = AccountState::new("account", 1);
        state.status = AccountStatus::Healthy;
        state.alerts.push(AlertRecord {
            at_ms: 1,
            verdict: detector::Verdict {
                degraded: true,
                signaled_requests: 2,
                signal_kinds: vec![detector::Signal::Overload],
                last_signal_at_ms: 1,
            },
            deliveries: vec![],
        });
        let deliveries = [notify::DeliveryOutcome {
            channel: "webhook".to_owned(),
            ok: true,
            detail: "HTTP 200".to_owned(),
        }];
        assert!(complete_deliveries(&mut state, 1, &deliveries));
        assert_eq!(state.status, AccountStatus::Healthy);
        assert!(state.alerts[0].deliveries[0].ok);
        state.alerts.clear();
        assert!(!complete_deliveries(&mut state, 1, &deliveries));
        assert!(state.alerts.is_empty());
    }

    #[test]
    fn settings_validation_rejects_bad_types_ranges_and_formats() {
        assert!(
            validate_settings(&json!({"window_ms": 10_000}))
                .unwrap_err()
                .contains("window_ms")
        );
        assert!(
            validate_settings(&json!({"sample_requests": 3}))
                .unwrap_err()
                .contains("sample_requests")
        );
        assert!(
            validate_settings(&json!({"enabled": "yes"}))
                .unwrap_err()
                .contains("boolean")
        );
        assert!(
            validate_settings(&json!({"webhook_url": "ftp://x"}))
                .unwrap_err()
                .contains("http")
        );
        assert!(
            validate_settings(&json!({"webhook_auth_header": "no-colon"}))
                .unwrap_err()
                .contains("Header-Name")
        );
        assert!(
            validate_settings(&json!({"email_to": "ops@example.com"}))
                .unwrap_err()
                .contains("array")
        );
        assert!(
            validate_settings(&json!({"schedule_all_degraded": "noop"}))
                .unwrap_err()
                .contains("delegate")
        );
        assert!(
            validate_settings(&json!({"min_signal_kinds": 4294967297u64}))
                .unwrap_err()
                .contains("min_signal_kinds")
        );
        assert!(
            validate_settings(&json!({"alert_message": "x".repeat(2000)}))
                .unwrap_err()
                .contains("1024")
        );
        assert!(
            validate_settings(&json!({"webhook_url": null, "enabled": null, "email_to": null}))
                .is_ok()
        );
        assert!(
            validate_settings(&json!({
                "webhook_url": "https://example.test/hook",
                "webhook_format": "dingtalk",
                "email_to": ["ops@example.com"],
                "min_signaled_requests": 4
            }))
            .is_ok()
        );
    }

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
