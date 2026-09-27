//! 降智信号判定：对每次终态观察提取信号，按账号滚动窗口聚合，连续命中才告警。

use std::collections::BTreeSet;

use gateway_plugin_sdk::call::policy::{ObserveRequest, RequestOutcome, RequestUsage};
use serde::{Deserialize, Serialize};

use crate::config::WatchConfig;

/// 一次请求可能同时携带的信号；`Serialize` 供状态与管理页面复用。
/// `CacheCollapse` 不直接出现在 event.signals——它是窗口级结论，由 evaluate 注入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// 上游返回容量类错误（502/503/529 或明确过载错误码）。
    Overload,
    /// 之前该账号缓存持续命中，本次大输入请求命中突然归零。
    CacheCollapse,
    /// 上游首响应耗时超过阈值（降智的典型体感是迟迟不出字，而非整体慢）。
    SlowResponse,
}

impl Signal {
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Overload => "上游容量类错误（502/503/529）",
            Self::CacheCollapse => "缓存命中骤降为零",
            Self::SlowResponse => "上游首响应耗时超过阈值",
        }
    }
}

/// 判定逻辑版本：信号语义变化时递增，落盘状态里的旧信号与连续判定随之作废。
/// v2：引入采样窗口；v3：CacheCollapse 降级为辅助信号；v4：慢响应改用真实上游首响应指标
/// （first_event_ms 在非流式路径是总耗时，不能再用作首 token 回退）。
pub const LOGIC_VERSION: u32 = 4;

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
    /// 上游首响应耗时：流式取首 token，非流式没有首 token 概念，
    /// 回退为上游处理耗时（provider_processing_ms），再回退为响应头耗时（headers_ms）。
    /// 不能用 first_event_ms：非流式路径它在整包接收完成时赋值，等于总耗时。
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
    /// 生成该状态所用判定逻辑版本；低于 LOGIC_VERSION 时信号与连续判定作废重算。
    #[serde(default)]
    pub logic_version: u32,
    #[serde(default)]
    pub status: AccountStatus,
    #[serde(default)]
    pub last_alert_at_ms: u64,
    #[serde(default)]
    pub alerts: Vec<AlertRecord>,
    #[serde(default)]
    pub last_probe: Option<CandyProbe>,
}

/// 一次已发送的降智告警。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRecord {
    pub at_ms: u64,
    pub verdict: Verdict,
    /// 各通知渠道的投递结果。
    pub deliveries: Vec<Delivery>,
}

/// 最近一次糖果题探针结果；管理页测试直接驱动，不进入信号窗口。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandyProbe {
    pub at_ms: u64,
    pub model: String,
    /// `correct` 答对、`wrong` 答错、`failed` 调用或解析失败。
    pub result: String,
    pub detail: String,
    /// 本次请求的 reasoning effort；空表示未指定。
    #[serde(default)]
    pub effort: Option<String>,
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
            logic_version: LOGIC_VERSION,
            status: AccountStatus::Unknown,
            last_alert_at_ms: 0,
            alerts: Vec::new(),
            last_probe: None,
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
        .and_then(|timings| {
            timings
                .first_token_ms
                .or(timings.provider_processing_ms)
                .or(timings.headers_ms)
        });
    let input_tokens = usage.and_then(|usage| usage.input_tokens);
    let cached_tokens = usage.and_then(|usage| usage.cached_tokens);

    let mut signals = Vec::new();
    if is_overload(upstream_status, error_code.as_deref()) {
        signals.push(Signal::Overload);
    }
    // 缓存骤降是窗口级信号，需要事件序列判断，不由单次事件标记。
    if is_slow(first_token_ms, config) {
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
/// 429/rate_limit 是限流而非容量耗尽，不算降智信号。
#[must_use]
pub fn is_overload(upstream_status: Option<u16>, error_code: Option<&str>) -> bool {
    if matches!(upstream_status, Some(502 | 503 | 529)) {
        return true;
    }
    error_code.is_some_and(|code| {
        let code = code.to_ascii_lowercase();
        code.contains("overload") || code.contains("capacity") || code.contains("engine_overloaded")
    })
}

/// 统计窗口内连续大输入且缓存为 0 的事件数；阈值由配置控制，防止单次波动误报。
/// 要求窗口内先有过缓存命中，再出现连续 ≥2 次零命中才算骤降。
#[must_use]
pub fn cache_collapse_streak(events: &[RequestEvent], config: &WatchConfig) -> u32 {
    // 只在窗口内已有缓存命中基线的事件之后计数，避免冷启动噪声。
    let mut seen_hit = false;
    let mut streak = 0u32;
    for event in events {
        // 只有大输入请求参与缓存命中序列；不相关的小请求不重置归零计数。
        let large_input = event
            .input_tokens
            .is_some_and(|input| input >= config.cache_min_input_tokens);
        if !large_input {
            continue;
        }
        if event.cached_tokens.is_some_and(|cached| cached > 0) {
            seen_hit = true;
            streak = 0;
        } else if seen_hit && event.cached_tokens == Some(0) {
            streak = streak.saturating_add(1);
        } else {
            streak = 0;
        }
    }
    streak
}

/// 响应变慢：只看上游首响应耗时——降智的典型体感是迟迟不出字，
/// 整体耗时（含正常的长生成与非流式整包接收）不再计入，避免把慢回答误判成降智。
#[must_use]
pub fn is_slow(first_token_ms: Option<u64>, config: &WatchConfig) -> bool {
    first_token_ms.is_some_and(|value| value >= config.first_token_ms)
}

/// 判定语义升级后迁移落盘状态：旧信号与连续判定不再有效，从零重新累计。
/// 事件序列保留（仍参与缓存骤降序列与界面展示），Degraded/Suspect 回到健康，
/// 让下一批真实观察按新语义重新判定；返回是否发生了迁移。
pub fn migrate_detection_state(state: &mut AccountState) -> bool {
    if state.logic_version >= LOGIC_VERSION {
        return false;
    }
    for event in &mut state.events {
        event.signals.clear();
    }
    state.verdict_streak = 0;
    // 被排除调度的账号按迁移重置回监控池，等价于一次自动恢复调度；
    // 告警历史保留，重新降智时仍受冷却约束。
    if matches!(
        state.status,
        AccountStatus::Degraded | AccountStatus::Suspect
    ) {
        state.status = AccountStatus::Healthy;
    }
    state.logic_version = LOGIC_VERSION;
    true
}

/// 对最近采样事件做判定：带信号请求数与信号种类同时达标才算降智结论。
/// 只在最近 `sample_requests` 个事件内统计，防止老事件稀释当前判定。
/// `CacheCollapse` 是辅助信号：窗口内先命中后连续归零，且同一采样内已有 `SlowResponse`
/// 才计入信号种类；单独的零命中不足以判定降智。
#[must_use]
pub fn evaluate(events: &[RequestEvent], config: &WatchConfig) -> Verdict {
    let sample_size = config.sample_requests.clamp(4, 40) as usize;
    let events = if events.len() > sample_size {
        &events[events.len() - sample_size..]
    } else {
        events
    };
    let signaled: Vec<&RequestEvent> = events
        .iter()
        .filter(|event| !event.signals.is_empty())
        .collect();
    let signaled_requests = signaled.len() as u32;
    let mut kinds: BTreeSet<Signal> = signaled
        .iter()
        .flat_map(|event| event.signals.iter().copied())
        .collect();
    // 缓存骤降需要 0 命中与慢响应同时存在才有意义。
    let has_cache_collapse =
        kinds.contains(&Signal::SlowResponse) && cache_collapse_streak(events, config) >= 2;
    if has_cache_collapse {
        kinds.insert(Signal::CacheCollapse);
    }
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
    // 判定语义变化后旧信号与连续判定作废，按新逻辑从零累计。
    migrate_detection_state(state);
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
        "账号 {}（{}，Provider：{}，模型：{}）在最近 {} 次请求采样内出现 {} 个带信号请求，命中信号：{}。判定已持续 {} 次。",
        label,
        state.account_id,
        state.provider.as_deref().unwrap_or("未知"),
        state.model.as_deref().unwrap_or("未知"),
        config.sample_requests,
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
        assert!(!is_overload(Some(429), None));
        // 限流不算降智：429 / rate_limit_exceeded 属于配额或节奏限制。
        assert!(!is_overload(None, Some("rate_limit_exceeded")));
        assert!(!is_overload(Some(429), Some("rate_limit_exceeded")));
        assert!(!is_overload(None, Some("invalid_request")));
    }

    fn cache_event(at_ms: u64, input_tokens: u64, cached_tokens: u64) -> RequestEvent {
        RequestEvent {
            input_tokens: Some(input_tokens),
            cached_tokens: Some(cached_tokens),
            ..event(at_ms, &[])
        }
    }

    #[test]
    fn cache_collapse_requires_sustained_zero_after_hit() {
        let config = config();
        // 窗口内没有命中基线：零命中不是骤降。
        assert_eq!(
            cache_collapse_streak(
                &[cache_event(1, 50_000, 0), cache_event(2, 50_000, 0)],
                &config
            ),
            0
        );
        // 一次命中后单次归零：不触发。
        assert_eq!(
            cache_collapse_streak(
                &[cache_event(1, 50_000, 300), cache_event(2, 50_000, 0)],
                &config
            ),
            1
        );
        // 一次命中后连续两次归零：触发骤降信号。
        assert_eq!(
            cache_collapse_streak(
                &[
                    cache_event(1, 50_000, 300),
                    cache_event(2, 50_000, 0),
                    cache_event(3, 50_000, 0)
                ],
                &config
            ),
            2
        );
        // 小输入事件被跳过而不是重置：hit,0,small0,0 → 连续 2 次大输入零命中。
        assert_eq!(
            cache_collapse_streak(
                &[
                    cache_event(1, 50_000, 300),
                    cache_event(2, 50_000, 0),
                    cache_event(3, 10, 0),
                    cache_event(4, 50_000, 0)
                ],
                &config
            ),
            2
        );
        // 再次出现命中会重置归零计数。
        assert_eq!(
            cache_collapse_streak(
                &[
                    cache_event(1, 50_000, 300),
                    cache_event(2, 50_000, 0),
                    cache_event(3, 50_000, 100),
                    cache_event(4, 50_000, 0),
                    cache_event(5, 50_000, 0)
                ],
                &config
            ),
            2
        );
    }

    #[test]
    fn slow_response_threshold() {
        let config = config();
        // 只看首 token 耗时：整体耗时再高也不计入。
        assert!(is_slow(Some(10_001), &config));
        assert!(!is_slow(Some(9_999), &config));
        assert!(!is_slow(None, &config));
    }

    fn observation(timings: gateway_plugin_sdk::call::policy::RequestTimings) -> ObserveRequest {
        ObserveRequest {
            event_id: "e1".to_owned(),
            request_id: "r1".to_owned(),
            config_revision: 1,
            operation: "generate".to_owned(),
            client_key_id: None,
            account_id: Some("acct".to_owned()),
            upstream_model: None,
            response_model: None,
            service_tier: None,
            requested_model: None,
            provider: Some("openai".to_owned()),
            completed_at_ms: 1,
            terminal: None,
            usage: Some(RequestUsage {
                timings: Some(timings),
                ..RequestUsage::default()
            }),
        }
    }

    #[test]
    fn slow_response_uses_first_token_then_upstream_processing_then_headers() {
        use gateway_plugin_sdk::call::policy::RequestTimings;
        let config = config();
        // 流式：首 token 优先，哪怕 first_event_ms（结构帧）很大也不覆盖。
        let event = extract_event(
            &observation(RequestTimings {
                first_token_ms: Some(2_000),
                first_event_ms: Some(60_000),
                headers_ms: Some(1_000),
                ..RequestTimings::default()
            }),
            &config,
            false,
        );
        assert_eq!(event.first_token_ms, Some(2_000));
        assert!(!event.signals.contains(&Signal::SlowResponse));
        // 非流式：无首 token 时取上游处理耗时，不用 first_event_ms（整包接收≈总耗时）。
        let event = extract_event(
            &observation(RequestTimings {
                first_event_ms: Some(60_000),
                provider_processing_ms: Some(3_000),
                headers_ms: Some(4_000),
                latency_ms: Some(61_000),
                ..RequestTimings::default()
            }),
            &config,
            false,
        );
        assert_eq!(event.first_token_ms, Some(3_000));
        assert!(!event.signals.contains(&Signal::SlowResponse));
        // 非流式且无处理耗时：回退响应头耗时；总耗时再高也不误报。
        let event = extract_event(
            &observation(RequestTimings {
                first_event_ms: Some(60_000),
                headers_ms: Some(5_000),
                latency_ms: Some(61_000),
                ..RequestTimings::default()
            }),
            &config,
            false,
        );
        assert_eq!(event.first_token_ms, Some(5_000));
        assert!(!event.signals.contains(&Signal::SlowResponse));
        // 上游真的慢（处理耗时超阈值）才计信号。
        let event = extract_event(
            &observation(RequestTimings {
                provider_processing_ms: Some(11_000),
                ..RequestTimings::default()
            }),
            &config,
            false,
        );
        assert!(event.signals.contains(&Signal::SlowResponse));
    }

    #[test]
    fn migration_clears_stale_signals_and_restores_degraded() {
        let mut state = AccountState::new("acct", 0);
        state.logic_version = 0;
        state.status = AccountStatus::Degraded;
        state.verdict_streak = 2;
        state.events.push(event(1, &[Signal::SlowResponse]));
        state.events.push(event(2, &[Signal::CacheCollapse]));
        assert!(migrate_detection_state(&mut state));
        assert_eq!(state.status, AccountStatus::Healthy);
        assert_eq!(state.verdict_streak, 0);
        assert_eq!(state.logic_version, LOGIC_VERSION);
        assert!(state.events.iter().all(|event| event.signals.is_empty()));
        // 事件序列保留，已迁移状态不重复迁移。
        assert_eq!(state.events.len(), 2);
        assert!(!migrate_detection_state(&mut state));
        // 旧版缓存命中基线保留，继续用于骤降判定。
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
        // Overload + SlowResponse = 2 种；CacheCollapse 是窗口级注入信号。
        assert_eq!(verdict.signal_kinds.len(), 3);
        // 只有一个带信号请求：不够。
        let verdict = evaluate(&[event(1, &[Signal::Overload]), event(2, &[])], &config);
        assert!(!verdict.degraded);
    }

    #[test]
    fn evaluate_samples_only_recent_requests() {
        let config = WatchConfig {
            sample_requests: 4,
            min_signaled_requests: 1,
            min_signal_kinds: 1,
            ..config()
        };
        // 6 个事件、采样上限 4：前两个带信号事件被剔除后不判定。
        let events: Vec<RequestEvent> = vec![
            event(1, &[Signal::Overload]),
            event(2, &[Signal::Overload]),
            event(3, &[]),
            event(4, &[]),
            event(5, &[]),
            event(6, &[]),
        ];
        let verdict = evaluate(&events, &config);
        assert!(!verdict.degraded);
        assert_eq!(verdict.signaled_requests, 0);
        // 采样内最后一条带信号即可触发（min_signaled_requests=1）。
        let events = vec![
            event(1, &[]),
            event(2, &[]),
            event(3, &[]),
            event(4, &[]),
            event(5, &[]),
            event(6, &[Signal::SlowResponse]),
        ];
        assert!(evaluate(&events, &config).degraded);
    }

    #[test]
    fn cache_collapse_needs_slow_response_to_count() {
        let config = WatchConfig {
            min_signaled_requests: 1,
            min_signal_kinds: 2,
            ..config()
        };
        let collapse_events = vec![
            cache_event(1, 50_000, 300),
            cache_event(2, 50_000, 0),
            cache_event(3, 50_000, 0),
        ];
        // 只有骤降、没有慢响应：不判定（用户反馈的核心修复点）。
        let verdict = evaluate(&collapse_events, &config);
        assert!(!verdict.degraded);
        assert!(verdict.signal_kinds.is_empty());
        // 骤降 + 慢响应：两种信号，成立。
        let mut events = collapse_events;
        events.push(event(4, &[Signal::SlowResponse]));
        let verdict = evaluate(&events, &config);
        assert!(verdict.degraded);
        assert!(verdict.signal_kinds.contains(&Signal::CacheCollapse));
        assert!(verdict.signal_kinds.contains(&Signal::SlowResponse));
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
