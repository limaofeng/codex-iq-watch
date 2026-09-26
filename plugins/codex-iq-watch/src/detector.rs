//! 降智信号判定：对每次终态观察提取信号，按账号滚动窗口聚合，连续命中才告警。

use std::collections::BTreeSet;

use gateway_plugin_sdk::call::policy::{ObserveRequest, RequestOutcome, RequestUsage};
use serde::{Deserialize, Serialize};

use crate::config::WatchConfig;

/// 一次请求可能同时携带的信号；`Serialize` 供状态与管理页面复用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// 上游返回容量类错误（502/503/529 或明确过载错误码）。
    Overload,
    /// 之前该账号缓存持续命中，本次大输入请求命中突然归零。
    CacheCollapse,
    /// 整体耗时或首 token 耗时超过阈值。
    SlowResponse,
}

impl Signal {
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Overload => "上游容量类错误（502/503/529）",
            Self::CacheCollapse => "缓存命中骤降为零",
            Self::SlowResponse => "响应耗时超过阈值",
        }
    }
}

/// 写入私有状态的一次请求事件；字段保持安全摘要，不含正文或凭据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEvent {
    pub request_id: String,
    pub observed_at_ms: u64,
    pub outcome: String,
    pub model: Option<String>,
    pub upstream_status: Option<u16>,
    pub client_status: Option<u16>,
    pub error_code: Option<String>,
    pub latency_ms: Option<u64>,
    pub first_token_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    /// 历史上有缓存命中；用于判定骤降而不是单纯的零命中。
    pub had_cache_hit: bool,
    pub signals: Vec<Signal>,
}

/// 账号滚动状态：窗口内事件、当前连续判定计数与告警历史。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountState {
    pub account_id: String,
    /// 宿主账号显示名（name/email），观察阶段按需填充；不可用时通知回退为内部 ID。
    #[serde(default)]
    pub display_name: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// 账号历史上是否出现过缓存命中，作为骤降基线。
    #[serde(default)]
    pub cache_baseline_hit: bool,
    pub last_observed_at_ms: u64,
    /// 已按窗口裁剪的最近事件；容量同时受上限约束。
    #[serde(default)]
    pub events: Vec<RequestEvent>,
    /// 上一次窗口判定是否为降智结论；连续命中才升级告警。
    #[serde(default)]
    pub verdict_streak: u32,
    #[serde(default)]
    pub status: AccountStatus,
    #[serde(default)]
    pub last_alert_at_ms: u64,
    #[serde(default)]
    pub alerts: Vec<AlertRecord>,
}

/// 一次已发送的降智告警。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRecord {
    pub at_ms: u64,
    pub verdict: Verdict,
    /// 各通知渠道的投递结果。
    pub deliveries: Vec<Delivery>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delivery {
    pub channel: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    #[default]
    Unknown,
    Healthy,
    Suspect,
    Degraded,
}

/// 窗口判定结果：是否满足降智条件、带信号请求数与信号种类。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub degraded: bool,
    pub signaled_requests: u32,
    pub signal_kinds: Vec<Signal>,
    /// 触发判定的最近信号摘要（最多取最后一条带信号事件）。
    pub last_signal_at_ms: u64,
}

pub const EVENT_LIMIT: usize = 40;
pub const ALERT_LIMIT: usize = 50;

impl AccountState {
    /// 界面与通知里的人类可读标识：优先显示名，缺省回退为内部账号 ID。
    #[must_use]
    pub fn display_label(&self) -> &str {
        self.display_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.account_id)
    }

    pub fn new(account_id: &str, now_ms: u64) -> Self {
        Self {
            account_id: account_id.to_owned(),
            display_name: None,
            provider: None,
            model: None,
            cache_baseline_hit: false,
            last_observed_at_ms: now_ms,
            events: Vec::new(),
            verdict_streak: 0,
            status: AccountStatus::Unknown,
            last_alert_at_ms: 0,
            alerts: Vec::new(),
        }
    }

    /// 裁剪到当前窗口与容量上限。
    pub fn prune(&mut self, now_ms: u64, window_ms: u64) {
        let floor = now_ms.saturating_sub(window_ms);
        self.events.retain(|event| event.observed_at_ms >= floor);
        if self.events.len() > EVENT_LIMIT {
            let overflow = self.events.len() - EVENT_LIMIT;
            self.events.drain(0..overflow);
        }
        if self.alerts.len() > ALERT_LIMIT {
            let overflow = self.alerts.len() - ALERT_LIMIT;
            self.alerts.drain(0..overflow);
        }
    }
}

/// 从终态观察提取本次事件与信号；`had_cache_hit` 由调用方传入该账号历史基线。
#[must_use]
pub fn extract_event(
    observation: &ObserveRequest,
    config: &WatchConfig,
    had_cache_hit: bool,
) -> RequestEvent {
    let usage: Option<&RequestUsage> = observation.usage.as_ref();
    let outcome = observation
        .terminal
        .as_ref()
        .map(|terminal| outcome_name(&terminal.outcome))
        .unwrap_or("unknown");
    let upstream_status = usage
        .and_then(|usage| usage.failure.as_ref())
        .and_then(|failure| failure.upstream_status_code)
        .or_else(|| {
            observation
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.client_status_code)
        });
    let client_status = usage
        .and_then(|usage| usage.failure.as_ref())
        .and_then(|failure| failure.client_status_code)
        .or_else(|| {
            observation
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.client_status_code)
        });
    let error_code = usage
        .and_then(|usage| usage.failure.as_ref())
        .and_then(|failure| failure.error_code.clone())
        .or_else(|| {
            observation
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.error_code.clone())
        });
    let latency_ms = usage
        .and_then(|usage| usage.timings.as_ref())
        .and_then(|timings| timings.latency_ms);
    let first_token_ms = usage
        .and_then(|usage| usage.timings.as_ref())
        .and_then(|timings| timings.first_token_ms.or(timings.first_event_ms));
    let input_tokens = usage.and_then(|usage| usage.input_tokens);
    let cached_tokens = usage.and_then(|usage| usage.cached_tokens);

    let mut signals = Vec::new();
    if is_overload(upstream_status, error_code.as_deref()) {
        signals.push(Signal::Overload);
    }
    if is_cache_collapse(had_cache_hit, input_tokens, cached_tokens, config) {
        signals.push(Signal::CacheCollapse);
    }
    if is_slow(latency_ms, first_token_ms, config) {
        signals.push(Signal::SlowResponse);
    }

    RequestEvent {
        request_id: observation.request_id.clone(),
        observed_at_ms: observation.completed_at_ms,
        outcome: outcome.to_owned(),
        model: observation
            .upstream_model
            .clone()
            .or_else(|| observation.requested_model.clone()),
        upstream_status,
        client_status,
        error_code,
        latency_ms,
        first_token_ms,
        input_tokens,
        cached_tokens,
        had_cache_hit,
        signals,
    }
}

fn outcome_name(outcome: &RequestOutcome) -> &'static str {
    match outcome {
        RequestOutcome::Succeeded => "succeeded",
        RequestOutcome::Failed => "failed",
        RequestOutcome::Rejected => "rejected",
        RequestOutcome::Cancelled => "cancelled",
        RequestOutcome::Incomplete => "incomplete",
    }
}

/// 容量类错误：上游明确 502/503/529，或错误码命中过载关键词。
#[must_use]
pub fn is_overload(upstream_status: Option<u16>, error_code: Option<&str>) -> bool {
    if matches!(upstream_status, Some(502 | 503 | 529)) {
        return true;
    }
    error_code.is_some_and(|code| {
        let code = code.to_ascii_lowercase();
        code.contains("overload")
            || code.contains("capacity")
            || code.contains("rate_limit")
            || code.contains("engine_overloaded")
    })
}

/// 缓存骤降：账号历史上命中过缓存，本次大输入请求命中归零。
#[must_use]
pub fn is_cache_collapse(
    had_cache_hit: bool,
    input_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    config: &WatchConfig,
) -> bool {
    had_cache_hit
        && input_tokens.is_some_and(|input| input >= config.cache_min_input_tokens)
        && cached_tokens == Some(0)
}

/// 响应变慢：整体耗时或首 token 耗时超过阈值，只计入有该项事实的请求。
#[must_use]
pub fn is_slow(latency_ms: Option<u64>, first_token_ms: Option<u64>, config: &WatchConfig) -> bool {
    latency_ms.is_some_and(|value| value >= config.latency_ms)
        || first_token_ms.is_some_and(|value| value >= config.first_token_ms)
}

/// 对窗口内事件做判定：带信号请求数与信号种类同时达标才算降智结论。
#[must_use]
pub fn evaluate(events: &[RequestEvent], config: &WatchConfig) -> Verdict {
    let signaled = events.iter().filter(|event| !event.signals.is_empty());
    let signaled_requests = signaled.clone().count() as u32;
    let kinds: BTreeSet<Signal> = signaled
        .flat_map(|event| event.signals.iter().copied())
        .collect();
    let last_signal_at_ms = events
        .iter()
        .rev()
        .find(|event| !event.signals.is_empty())
        .map_or(0, |event| event.observed_at_ms);
    Verdict {
        degraded: signaled_requests >= config.min_signaled_requests
            && kinds.len() as u32 >= config.min_signal_kinds,
        signaled_requests,
        signal_kinds: kinds.into_iter().collect(),
        last_signal_at_ms,
    }
}

/// 更新账号状态：记录事件、刷新基线、裁剪窗口并更新连续判定计数与状态。
/// 返回当前判定结论；是否进入告警由调用方按连续次数与冷却决定。
pub fn apply_event(
    state: &mut AccountState,
    event: RequestEvent,
    observation: &ObserveRequest,
    config: &WatchConfig,
) -> Verdict {
    let now_ms = observation.completed_at_ms;
    state.provider = observation
        .provider
        .clone()
        .or_else(|| state.provider.take());
    state.model = event.model.clone().or_else(|| state.model.take());
    // 基线一旦见过命中就保留，用于区分“骤降”与“从不缓存”。
    if event.cached_tokens.is_some_and(|cached| cached > 0) {
        state.cache_baseline_hit = true;
    }
    state.last_observed_at_ms = now_ms;
    state.events.push(event);
    state.prune(now_ms, config.window_ms);
    let verdict = evaluate(&state.events, config);
    if verdict.degraded {
        state.verdict_streak = state.verdict_streak.saturating_add(1);
        state.status = if state.verdict_streak >= config.consecutive_triggers {
            AccountStatus::Degraded
        } else {
            AccountStatus::Suspect
        };
    } else {
        state.verdict_streak = 0;
        if !state.events.is_empty() {
            state.status = AccountStatus::Healthy;
        }
    }
    verdict
}

/// 是否应当发出新告警：达到连续判定次数且过了冷却期。
#[must_use]
pub fn should_alert(state: &AccountState, verdict: &Verdict, config: &WatchConfig) -> bool {
    verdict.degraded
        && state.verdict_streak >= config.consecutive_triggers
        && state
            .last_observed_at_ms
            .saturating_sub(state.last_alert_at_ms)
            >= config.cooldown_ms
}

/// 渲染通知用的标题与详情；不包含凭据、正文或敏感字段。
#[must_use]
pub fn render_alert(
    state: &AccountState,
    verdict: &Verdict,
    config: &WatchConfig,
) -> (String, String) {
    let kinds = verdict
        .signal_kinds
        .iter()
        .map(|signal| signal.description())
        .collect::<Vec<_>>()
        .join("、");
    let label = state.display_label();
    let title = format!("Codex 降智告警：账号 {label} 疑似降智");
    let mut detail = format!(
        "账号 {}（{}，Provider：{}，模型：{}）在 {} 分钟窗口内出现 {} 个带信号请求，命中信号：{}。判定已持续 {} 次。",
        label,
        state.account_id,
        state.provider.as_deref().unwrap_or("未知"),
        state.model.as_deref().unwrap_or("未知"),
        config.window_ms / 60_000,
        verdict.signaled_requests,
        kinds,
        state.verdict_streak,
    );
    if !config.alert_message.is_empty() {
        detail.push(' ');
        detail.push_str(&config.alert_message);
    }
    (title, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> WatchConfig {
        WatchConfig::default()
    }

    #[test]
    fn overload_detects_status_and_error_code() {
        assert!(is_overload(Some(502), None));
        assert!(is_overload(Some(503), None));
        assert!(is_overload(Some(529), None));
        assert!(is_overload(None, Some("engine_overloaded_error")));
        assert!(is_overload(None, Some("model_at_capacity")));
        assert!(!is_overload(Some(500), None));
        assert!(!is_overload(None, Some("invalid_request")));
    }

    #[test]
    fn cache_collapse_requires_baseline() {
        let config = config();
        // 无历史命中不计骤降，避免冷启动误报。
        assert!(!is_cache_collapse(false, Some(50_000), Some(0), &config));
        assert!(is_cache_collapse(true, Some(50_000), Some(0), &config));
        // 小输入不计。
        assert!(!is_cache_collapse(true, Some(10), Some(0), &config));
        // 命中大于零不算骤降。
        assert!(!is_cache_collapse(true, Some(50_000), Some(10), &config));
    }

    #[test]
    fn slow_response_threshold() {
        let config = config();
        assert!(is_slow(Some(10_001), None, &config));
        assert!(is_slow(None, Some(10_001), &config));
        assert!(!is_slow(Some(9_999), Some(9_999), &config));
        assert!(!is_slow(None, None, &config));
    }

    fn event(at_ms: u64, signals: &[Signal]) -> RequestEvent {
        RequestEvent {
            request_id: format!("r{at_ms}"),
            observed_at_ms: at_ms,
            outcome: "succeeded".to_owned(),
            model: None,
            upstream_status: None,
            client_status: None,
            error_code: None,
            latency_ms: None,
            first_token_ms: None,
            input_tokens: None,
            cached_tokens: None,
            had_cache_hit: false,
            signals: signals.to_vec(),
        }
    }

    #[test]
    fn verdict_needs_requests_and_kinds() {
        let config = config();
        // 两个请求同一种信号：不够。
        let verdict = evaluate(
            &[
                event(1, &[Signal::SlowResponse]),
                event(2, &[Signal::SlowResponse]),
            ],
            &config,
        );
        assert!(!verdict.degraded);
        // 两个请求两种信号：成立。
        let verdict = evaluate(
            &[
                event(1, &[Signal::Overload]),
                event(2, &[Signal::CacheCollapse, Signal::SlowResponse]),
            ],
            &config,
        );
        assert!(verdict.degraded);
        assert_eq!(verdict.signaled_requests, 2);
        assert_eq!(verdict.signal_kinds.len(), 3);
        // 只有一个带信号请求：不够。
        let verdict = evaluate(&[event(1, &[Signal::Overload]), event(2, &[])], &config);
        assert!(!verdict.degraded);
    }

    #[test]
    fn alert_requires_streak_and_cooldown() {
        let config = config();
        // 冷却期默认 15 分钟，观测时间需要大于它。
        let mut state = AccountState::new("acct", 0);
        state.last_observed_at_ms = 2_000_000;
        let verdict = Verdict {
            degraded: true,
            signaled_requests: 2,
            signal_kinds: vec![Signal::Overload, Signal::CacheCollapse],
            last_signal_at_ms: 2_000_000,
        };
        // 连续次数不足不告警。
        state.verdict_streak = 1;
        assert!(!should_alert(&state, &verdict, &config));
        // 冷却期内不重复告警。
        state.verdict_streak = 3;
        state.last_alert_at_ms = 1_900_000;
        assert!(!should_alert(&state, &verdict, &config));
        // 冷却期外允许再告警。
        state.last_alert_at_ms = 0;
        assert!(should_alert(&state, &verdict, &config));
    }
}
