//! 通知渲染与发送：经宿主 `host.http.do` 发 Webhook／邮件 HTTP API；失败只记结果，不影响观察链路。

use gateway_plugin_sdk::client::HostClient;
use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::host::{HttpRequest, HttpResponse},
};
use serde_json::{Value, json};

use crate::config::{EmailFormat, WatchConfig, WebhookFormat};

/// 一次通知的公共事实；标题与详情已去除敏感数据。
pub struct Notice {
    pub title: String,
    pub detail: String,
    pub account_id: String,
    pub occurred_at_ms: u64,
    pub verdict: serde_json::Value,
}

/// 单渠道投递结果。
pub struct DeliveryOutcome {
    pub channel: String,
    pub ok: bool,
    pub detail: String,
}

/// 发送全部已配置渠道；每个渠道失败独立记录，不中断其余渠道。
pub async fn deliver_all(
    host: &HostClient,
    config: &WatchConfig,
    notice: &Notice,
) -> Vec<DeliveryOutcome> {
    let mut results = Vec::new();
    if !config.webhook_url.is_empty() {
        results.push(deliver_webhook(host, config, notice).await);
    }
    if !config.email_url.is_empty() && !config.email_to.is_empty() {
        results.push(deliver_email(host, config, notice).await);
    }
    results
}

async fn deliver_webhook(
    host: &HostClient,
    config: &WatchConfig,
    notice: &Notice,
) -> DeliveryOutcome {
    let body = webhook_body(config, notice);
    send(
        host,
        &config.webhook_url,
        &config.webhook_auth_header,
        &body,
    )
    .await
    .map(|(status, response)| {
        // 企微/钉钉/飞书在 HTTP 200 下也用 errcode/code 表示失败，仅看状态码会误报成功。
        let (ok, detail) = match config.webhook_format {
            WebhookFormat::Wecom | WebhookFormat::Dingtalk => {
                check_errcode(status, &response, "errcode")
            }
            WebhookFormat::Feishu => check_feishu(status, &response),
            _ => status_outcome(status, &response),
        };
        DeliveryOutcome {
            channel: "webhook".to_owned(),
            ok,
            detail,
        }
    })
    .unwrap_or_else(|error| DeliveryOutcome {
        channel: "webhook".to_owned(),
        ok: false,
        detail: error.message.clone(),
    })
}

/// HTTP 状态码与响应摘录组成的判定；失败时保留目标方返回的前 200 字符便于定位。
fn status_outcome(status: u16, body: &[u8]) -> (bool, String) {
    if (200..300).contains(&status) {
        (true, format!("HTTP {status}"))
    } else {
        (false, format!("HTTP {status}: {}", body_excerpt(body)))
    }
}

/// 企微／钉钉风格：`errcode` 为 0 才算成功；响应不是 JSON 时退回状态码判定。
fn check_errcode(status: u16, body: &[u8], field: &str) -> (bool, String) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return status_outcome(status, body);
    };
    match value.get(field).and_then(Value::as_i64) {
        Some(0) => (true, format!("HTTP {status}")),
        Some(code) => {
            let message = value
                .get("errmsg")
                .or_else(|| value.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or("");
            (false, format!("HTTP {status}: {field} {code} {message}"))
        }
        None => status_outcome(status, body),
    }
}

/// 飞书机器人用 `code`（新格式）或 `StatusCode`（旧格式）字段。
fn check_feishu(status: u16, body: &[u8]) -> (bool, String) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return status_outcome(status, body);
    };
    let code = value
        .get("code")
        .or_else(|| value.get("StatusCode"))
        .and_then(Value::as_i64);
    match code {
        Some(0) => (true, format!("HTTP {status}")),
        Some(code) => {
            let message = value
                .get("msg")
                .or_else(|| value.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("");
            (false, format!("HTTP {status}: code {code} {message}"))
        }
        None => status_outcome(status, body),
    }
}

fn body_excerpt(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    text.chars().take(200).collect()
}

async fn deliver_email(
    host: &HostClient,
    config: &WatchConfig,
    notice: &Notice,
) -> DeliveryOutcome {
    let subject = render_template(&config.email_subject_template, notice, false);
    let body = email_body(config, notice, &subject);
    send(host, &config.email_url, &config.email_auth_header, &body)
        .await
        .map(|(status, response)| {
            let (ok, detail) = status_outcome(status, &response);
            DeliveryOutcome {
                channel: "email".to_owned(),
                ok,
                detail,
            }
        })
        .unwrap_or_else(|error| DeliveryOutcome {
            channel: "email".to_owned(),
            ok: false,
            detail: error.message.clone(),
        })
}

async fn send(
    host: &HostClient,
    url: &str,
    auth_header: &str,
    body: &Value,
) -> Result<(u16, Vec<u8>), PluginFault> {
    let mut headers = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        ("accept".to_owned(), "*/*".to_owned()),
    ];
    if let Some((name, value)) = WatchConfig::auth_header(auth_header) {
        headers.push((name, value));
    }
    let request = HttpRequest {
        method: "POST".to_owned(),
        url: url.to_owned(),
        headers,
    };
    let reply = host
        .call(
            "host.http.do",
            serde_json::to_value(request).map_err(|_| {
                PluginFault::new(ErrorCode::InvalidInput, "invalid notification request")
            })?,
            serde_json::to_vec(body).unwrap_or_default(),
        )
        .await
        .map_err(|error| error.into_plugin_fault())?;
    let response: HttpResponse = serde_json::from_value(reply.result)
        .map_err(|_| PluginFault::new(ErrorCode::Fault, "host http reply is invalid"))?;
    Ok((response.status, reply.payload))
}

/// Webhook 载荷；generic 为结构化 JSON，其余适配常见 IM 机器人。
#[must_use]
pub fn webhook_body(config: &WatchConfig, notice: &Notice) -> Value {
    let text = format!("{}\n{}", notice.title, notice.detail);
    match config.webhook_format {
        WebhookFormat::Generic => json!({
            "event": "codex_iq_watch.degraded",
            "title": notice.title,
            "detail": notice.detail,
            "text": text,
            "account": {
                "account_id": notice.account_id,
            },
            "verdict": notice.verdict,
            "occurred_at": crate::time_util::to_rfc3339(notice.occurred_at_ms),
            "source": "codex-iq-watch",
        }),
        WebhookFormat::Wecom => json!({"msgtype": "text", "text": {"content": text}}),
        WebhookFormat::Dingtalk => json!({"msgtype": "text", "text": {"content": text}}),
        WebhookFormat::Feishu => json!({"msg_type": "text", "content": {"text": text}}),
        WebhookFormat::Slack => json!({"text": text}),
        WebhookFormat::Bark => json!({"title": notice.title, "body": notice.detail}),
    }
}

/// 邮件载荷；generic 走可配置模板，其余对应常用邮件 API。
#[must_use]
pub fn email_body(config: &WatchConfig, notice: &Notice, subject: &str) -> Value {
    match config.email_format {
        EmailFormat::Resend => json!({
            "from": config.email_from,
            "to": config.email_to,
            "subject": subject,
            "text": format!("{}\n{}", notice.title, notice.detail),
        }),
        EmailFormat::Postmark => json!({
            "From": config.email_from,
            "To": config.email_to.join(","),
            "Subject": subject,
            "TextBody": format!("{}\n{}", notice.title, notice.detail),
        }),
        EmailFormat::Sendgrid => json!({
            "personalizations": [{"to": config.email_to.iter().map(|to| json!({"email": to})).collect::<Vec<_>>()}],
            "from": {"email": config.email_from},
            "subject": subject,
            "content": [{"type": "text/plain", "value": format!("{}\n{}", notice.title, notice.detail)}],
        }),
        EmailFormat::Generic => {
            let template = if config.email_body_template.is_empty() {
                "{\"title\":\"{{title}}\",\"detail\":\"{{detail}}\",\"account\":\"{{account}}\",\"occurred_at\":\"{{occurred_at}}\"}"
            } else {
                &config.email_body_template
            };
            let rendered = render_template(template, notice, true);
            serde_json::from_str(&rendered)
                .unwrap_or_else(|_| json!({"title": notice.title, "detail": notice.detail}))
        }
    }
}

/// 占位替换：`json_escape=true` 时按 JSON 字符串内容转义，供邮件模板嵌入。
#[must_use]
pub fn render_template(template: &str, notice: &Notice, json_escape: bool) -> String {
    let occurred_at = crate::time_util::to_rfc3339(notice.occurred_at_ms);
    let values: [(&str, String); 5] = [
        ("title", notice.title.clone()),
        ("detail", notice.detail.clone()),
        ("account", notice.account_id.clone()),
        ("occurred_at", occurred_at),
        (
            "count",
            notice
                .verdict
                .get("signal_kinds")
                .and_then(Value::as_array)
                .map_or_else(|| "0".to_owned(), |kinds| kinds.len().to_string()),
        ),
    ];
    let mut output = template.to_owned();
    for (key, value) in values {
        let rendered = if json_escape {
            // 模板嵌入 JSON 字符串内容；只剥掉序列化结果最外层的一对引号，
            // 不能用 trim_*，否则值首尾恰好为引号时会被误删导致 JSON 未闭合。
            let encoded = serde_json::to_string(&value).unwrap_or_else(|_| "\"\"".to_owned());
            encoded
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .unwrap_or(encoded.as_str())
                .to_owned()
        } else {
            value
        };
        output = output.replace(&format!("{{{{{key}}}}}"), &rendered);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EmailFormat, WebhookFormat};

    fn notice() -> Notice {
        Notice {
            title: "标题".to_owned(),
            detail: "详情 \"引号\"".to_owned(),
            account_id: "acct-1".to_owned(),
            occurred_at_ms: 1_790_380_800_000,
            verdict: serde_json::json!({"signal_kinds": ["upstream_overload", "cache_collapse"]}),
        }
    }

    #[test]
    fn generic_webhook_is_structured() {
        let config = WatchConfig::default();
        let body = webhook_body(&config, &notice());
        assert_eq!(body["event"], "codex_iq_watch.degraded");
        assert_eq!(body["account"]["account_id"], "acct-1");
        assert_eq!(body["occurred_at"], "2026-09-26T00:00:00.000Z");
    }

    #[test]
    fn wecom_webhook_uses_text() {
        let config = WatchConfig {
            webhook_format: WebhookFormat::Wecom,
            ..WatchConfig::default()
        };
        let body = webhook_body(&config, &notice());
        assert_eq!(body["msgtype"], "text");
        assert!(body["text"]["content"].as_str().unwrap().contains("标题"));
    }

    #[test]
    fn resend_email_maps_fields() {
        let config = WatchConfig {
            email_format: EmailFormat::Resend,
            email_from: "watch@example.com".to_owned(),
            email_to: vec!["ops@example.com".to_owned()],
            ..WatchConfig::default()
        };
        let body = email_body(&config, &notice(), "主题");
        assert_eq!(body["from"], "watch@example.com");
        assert_eq!(body["to"][0], "ops@example.com");
        assert_eq!(body["subject"], "主题");
        assert!(body["text"].as_str().unwrap().contains("详情"));
    }

    #[test]
    fn errcode_channels_read_error_fields() {
        let body = br#"{"errcode":310000,"errmsg":"keywords not in content"}"#;
        let (ok, detail) = check_errcode(200, body, "errcode");
        assert!(!ok);
        assert!(detail.contains("310000"), "{detail}");
        assert!(detail.contains("keywords"), "{detail}");

        let body = br#"{"errcode":0,"errmsg":"ok"}"#;
        assert!(check_errcode(200, body, "errcode").0);
        // 飞书用 code 字段。
        let body = br#"{"code":11247,"msg":"param invalid"}"#;
        let (ok, detail) = check_feishu(200, body);
        assert!(!ok);
        assert!(detail.contains("11247"), "{detail}");
        let body = br#"{"code":0,"msg":"success"}"#;
        assert!(check_feishu(200, body).0);
        // 非 JSON 响应退回状态码判定。
        let (ok, detail) = status_outcome(429, b"slow down");
        assert!(!ok);
        assert!(detail.contains("slow down"), "{detail}");
    }

    #[test]
    fn template_escapes_json() {
        let rendered = render_template(
            "{\"t\":\"{{title}}\",\"d\":\"{{detail}}\",\"c\":\"{{count}}\"}",
            &notice(),
            true,
        );
        let value: serde_json::Value =
            serde_json::from_str(&rendered).unwrap_or_else(|e| panic!("parse {rendered:?}: {e}"));
        assert_eq!(value["t"], "标题");
        assert_eq!(value["d"], "详情 \"引号\"");
        assert_eq!(value["c"], "2");
    }
}
