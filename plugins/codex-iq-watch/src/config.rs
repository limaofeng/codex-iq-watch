//! 实例配置：由握手携带，宿主按 `configurationSchema` 校验，`secretFields` 合并到顶层。

use serde::Deserialize;
use serde_json::Value;

/// 与 `plugin.json` 的 configurationSchema 一一对应；未知字段已在宿主侧被拒。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WatchConfig {
    pub enabled: bool,
    pub watch_providers: Vec<String>,
    pub window_ms: u64,
    pub min_signaled_requests: u32,
    pub min_signal_kinds: u32,
    pub consecutive_triggers: u32,
    pub cooldown_ms: u64,
    pub latency_ms: u64,
    pub first_token_ms: u64,
    pub cache_min_input_tokens: u64,
    pub alert_message: String,
    pub webhook_url: String,
    pub webhook_format: WebhookFormat,
    /// 敏感字段；格式 `Header-Name: value`，只在通知请求中回放。
    pub webhook_auth_header: String,
    pub email_url: String,
    pub email_format: EmailFormat,
    pub email_from: String,
    pub email_to: Vec<String>,
    pub email_subject_template: String,
    pub email_body_template: String,
    /// 敏感字段；格式 `Header-Name: value`。
    pub email_auth_header: String,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            watch_providers: vec!["openai".to_owned()],
            window_ms: 15 * 60 * 1000,
            min_signaled_requests: 2,
            min_signal_kinds: 2,
            consecutive_triggers: 2,
            cooldown_ms: 30 * 60 * 1000,
            latency_ms: 10_000,
            first_token_ms: 10_000,
            cache_min_input_tokens: 1_000,
            alert_message: String::new(),
            webhook_url: String::new(),
            webhook_format: WebhookFormat::Generic,
            webhook_auth_header: String::new(),
            email_url: String::new(),
            email_format: EmailFormat::Generic,
            email_from: String::new(),
            email_to: Vec::new(),
            email_subject_template: "[降智告警] {{account}} 信号 {{count}} 项".to_owned(),
            email_body_template: String::new(),
            email_auth_header: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookFormat {
    #[default]
    Generic,
    Wecom,
    Dingtalk,
    Feishu,
    Slack,
    Bark,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailFormat {
    #[default]
    Generic,
    Resend,
    Postmark,
    Sendgrid,
}

impl WatchConfig {
    /// 配置值是否落入既定范围；schema 已校验，这里只兜底非法组合。
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.window_ms = self.window_ms.clamp(60_000, 3_600_000);
        self.cooldown_ms = self.cooldown_ms.clamp(60_000, 86_400_000);
        self.min_signaled_requests = self.min_signaled_requests.clamp(2, 20);
        self.min_signal_kinds = self.min_signal_kinds.clamp(1, 5);
        self.consecutive_triggers = self.consecutive_triggers.clamp(1, 10);
        self
    }

    /// 该 Provider 是否参与监控；空数组表示全部 Provider。
    #[must_use]
    pub fn watches_provider(&self, provider: Option<&str>) -> bool {
        self.watch_providers.is_empty()
            || provider.is_some_and(|id| self.watch_providers.iter().any(|item| item == id))
    }

    /// 管理页保存的通知设置，覆盖宿主配置中的同名字段；留空的 URL/认证头视为不覆盖。
    pub fn apply_notify_settings(&mut self, settings: &Value) {
        for (key, target) in [
            ("webhook_url", &mut self.webhook_url as &mut String),
            ("webhook_auth_header", &mut self.webhook_auth_header),
            ("email_url", &mut self.email_url),
            ("email_from", &mut self.email_from),
            ("email_subject_template", &mut self.email_subject_template),
            ("email_body_template", &mut self.email_body_template),
            ("email_auth_header", &mut self.email_auth_header),
            ("alert_message", &mut self.alert_message),
        ] {
            if let Some(value) = settings.get(key).and_then(Value::as_str) {
                *target = value.trim().to_owned();
            }
        }
        if let Some(value) = settings.get("webhook_format").and_then(Value::as_str)
            && let Ok(format) =
                serde_json::from_value::<WebhookFormat>(Value::String(value.to_owned()))
        {
            self.webhook_format = format;
        }
        if let Some(value) = settings.get("email_format").and_then(Value::as_str)
            && let Ok(format) =
                serde_json::from_value::<EmailFormat>(Value::String(value.to_owned()))
        {
            self.email_format = format;
        }
        if let Some(recipients) = settings.get("email_to").and_then(Value::as_array) {
            self.email_to = recipients
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .take(8)
                .map(str::to_owned)
                .collect();
        }
        // 检测参数同样允许页面覆盖；越界值由 `normalized()` 收口。
        if let Some(value) = settings.get("enabled").and_then(Value::as_bool) {
            self.enabled = value;
        }
        if let Some(providers) = settings.get("watch_providers").and_then(Value::as_array) {
            self.watch_providers = providers
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .take(16)
                .map(str::to_owned)
                .collect();
        }
        if let Some(value) = settings.get("window_ms").and_then(Value::as_u64) {
            self.window_ms = value;
        }
        if let Some(value) = settings.get("cooldown_ms").and_then(Value::as_u64) {
            self.cooldown_ms = value;
        }
        if let Some(value) = settings.get("first_token_ms").and_then(Value::as_u64) {
            self.first_token_ms = value;
        }
        if let Some(value) = settings
            .get("cache_min_input_tokens")
            .and_then(Value::as_u64)
        {
            self.cache_min_input_tokens = value;
        }
        if let Some(value) = settings
            .get("min_signaled_requests")
            .and_then(Value::as_u64)
        {
            self.min_signaled_requests = value as u32;
        }
        if let Some(value) = settings.get("min_signal_kinds").and_then(Value::as_u64) {
            self.min_signal_kinds = value as u32;
        }
        if let Some(value) = settings.get("consecutive_triggers").and_then(Value::as_u64) {
            self.consecutive_triggers = value as u32;
        }
    }

    /// 把 `Header-Name: value` 形式的敏感配置解析为头对；格式不合法时不发送。
    #[must_use]
    pub fn auth_header(secret: &str) -> Option<(String, String)> {
        let (name, value) = secret.split_once(':')?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || value.is_empty() || value.len() > 2048 {
            return None;
        }
        Some((name.to_owned(), value.to_owned()))
    }

    /// 管理页面展示用的脱敏副本：不回显 URL 之外的秘密，认证头只显示是否已配置。
    #[must_use]
    pub fn redacted(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.enabled,
            "watch_providers": self.watch_providers,
            "window_ms": self.window_ms,
            "min_signaled_requests": self.min_signaled_requests,
            "min_signal_kinds": self.min_signal_kinds,
            "consecutive_triggers": self.consecutive_triggers,
            "cooldown_ms": self.cooldown_ms,
            "latency_ms": self.latency_ms,
            "first_token_ms": self.first_token_ms,
            "cache_min_input_tokens": self.cache_min_input_tokens,
            "alert_message": self.alert_message,
            "webhook_url": self.webhook_url,
            "webhook_format": format!("{:?}", self.webhook_format).to_lowercase(),
            "webhook_auth_header_configured": !self.webhook_auth_header.is_empty(),
            "email_url": self.email_url,
            "email_format": format!("{:?}", self.email_format).to_lowercase(),
            "email_from": self.email_from,
            "email_to": self.email_to,
            "email_subject_template": self.email_subject_template,
            "email_body_template": self.email_body_template,
            "email_auth_header_configured": !self.email_auth_header.is_empty(),
        })
    }
}
