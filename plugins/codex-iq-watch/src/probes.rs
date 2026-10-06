//! 单账号测试执行、完整答案判分与人工评价；不修改监控及调度事实。

use crate::{
    current_ms,
    questions::{self, Grading, Question, invalid},
    store,
};
use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::model::{CanonicalEvent, ExecutionEvent, FinishReason, WirePayload},
    client::HostClient,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

pub const NAMESPACE: &str = "probes";
pub const MAX_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub struct Service {
    active: Mutex<BTreeSet<String>>,
}
pub struct Permit<'a> {
    service: &'a Service,
    account: String,
}
impl Service {
    pub fn acquire(&self, account: &str) -> Result<Permit<'_>, PluginFault> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| PluginFault::new(ErrorCode::Fault, "测试锁不可用"))?;
        if !active.insert(account.into()) {
            return Err(PluginFault::new(
                ErrorCode::Conflict,
                "该账号正在测试，请稍后重试",
            ));
        }
        Ok(Permit {
            service: self,
            account: account.into(),
        })
    }
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.service.active.lock() {
            active.remove(&self.account);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Correct,
    Degraded,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Probe {
    pub id: String,
    pub account: String,
    pub at_ms: u64,
    pub question: Question,
    pub model: String,
    pub key: String,
    pub effort: Option<String>,
    pub execution: String,
    pub result: Option<Verdict>,
    pub output: String,
    pub detail: String,
}

impl Probe {
    fn grade(&mut self) {
        if self.question.grading == Grading::Exact {
            self.result = Some(
                if self
                    .question
                    .answers
                    .iter()
                    .any(|a| a.trim() == self.output.trim())
                {
                    Verdict::Correct
                } else {
                    Verdict::Degraded
                },
            );
        }
    }
    pub fn summary(&self) -> Value {
        json!({"id": self.id, "at_ms": self.at_ms, "title": self.question.title, "model": self.model, "effort": self.effort, "result": self.result, "execution": self.execution, "detail": self.detail})
    }
    pub fn evaluate(&mut self, id: &str, verdict: Verdict) -> Result<(), PluginFault> {
        if self.id != id {
            return Err(PluginFault::new(
                ErrorCode::Conflict,
                "该测试已被新测试覆盖，请刷新",
            ));
        }
        if self.question.grading != Grading::Manual || self.execution != "succeeded" {
            return Err(invalid("该测试不能人工评价"));
        }
        self.result = Some(verdict);
        Ok(())
    }
}

pub async fn load(host: &HostClient, account: &str) -> Result<Option<(Probe, u64)>, PluginFault> {
    store::load_private(host, NAMESPACE, &format!("acct:{account}")).await
}

pub async fn clear(host: &HostClient, account: &str) -> Result<(), PluginFault> {
    if let Some((_, version)) = load(host, account).await? {
        store::delete_private(host, NAMESPACE, &format!("acct:{account}"), version).await?;
    }
    Ok(())
}

fn preview(probe: &Probe) -> Value {
    if probe.question.grading != Grading::Manual || probe.execution != "succeeded" {
        return json!({});
    }
    match extract_svg(&probe.output) {
        Ok(svg) => json!({"svg": svg}),
        Err(error) => json!({"preview_error": error.message}),
    }
}

pub fn response(probe: &Probe, version: Option<u64>, error: Option<&str>) -> Value {
    json!({"probe": probe, "version": version, "recorded": version.is_some(), "storage_error": error, "preview": preview(probe)})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    account: String,
    model: String,
    key: String,
    effort: Option<String>,
    question_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveQuestion {
    expected_version: Option<u64>,
    question: questions::Draft,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteQuestion {
    expected_version: Option<u64>,
    id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Evaluate {
    account: String,
    id: String,
    expected_version: u64,
    result: Verdict,
}

fn decode<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Result<T, PluginFault> {
    serde_json::from_slice(payload).map_err(|_| invalid("请求格式无效"))
}

pub async fn handle(
    host: &HostClient,
    service: &Service,
    method: &str,
    path: &str,
    query: &str,
    payload: &[u8],
) -> Result<Value, PluginFault> {
    match (method, path) {
        ("GET", "questions") => {
            let (library, version) = questions::load(host).await?;
            Ok(json!({"questions": library.questions, "version": version}))
        }
        ("POST", "question-save") => {
            let input: SaveQuestion = decode(payload)?;
            let (mut library, version) = questions::load(host).await?;
            questions::check_version(version, input.expected_version)?;
            library.save(input.question)?;
            let version = questions::persist(host, &library, version).await?;
            Ok(json!({"questions": library.questions, "version": version}))
        }
        ("POST", "question-delete") => {
            let input: DeleteQuestion = decode(payload)?;
            let (mut library, version) = questions::load(host).await?;
            questions::check_version(version, input.expected_version)?;
            library.delete(&input.id)?;
            let version = questions::persist(host, &library, version).await?;
            Ok(json!({"questions": library.questions, "version": version}))
        }
        ("POST", "probe-run") => run(host, service, decode(payload)?).await,
        ("GET", "probe") => {
            let account = crate::query_param(query, "account")
                .filter(|a| !a.is_empty())
                .ok_or_else(|| invalid("请选择账号"))?;
            if let Some((probe, version)) = load(host, &account).await? {
                return Ok(response(&probe, Some(version), None));
            }
            let legacy = store::load_account(host, &account)
                .await?
                .and_then(|(state, _)| state.last_probe);
            Ok(json!({"probe": null, "legacy": legacy}))
        }
        ("POST", "probe-verdict") => {
            let input: Evaluate = decode(payload)?;
            let _permit = service.acquire(&input.account)?;
            let (mut probe, version) = load(host, &input.account)
                .await?
                .ok_or_else(|| invalid("测试不存在"))?;
            questions::check_version(Some(version), Some(input.expected_version))?;
            probe.evaluate(&input.id, input.result)?;
            let version = store::save_private(
                host,
                NAMESPACE,
                &format!("acct:{}", input.account),
                &probe,
                Some(version),
                MAX_BYTES,
            )
            .await?;
            Ok(response(&probe, Some(version), None))
        }
        _ => Err(invalid("未知测试路由")),
    }
}

async fn run(host: &HostClient, service: &Service, input: Run) -> Result<Value, PluginFault> {
    if input.account.is_empty() || input.model.is_empty() || input.key.is_empty() {
        return Err(invalid("请选择账号、Key 和模型"));
    }
    let effort = input.effort.filter(|e| !e.is_empty() && e != "default");
    if effort
        .as_deref()
        .is_some_and(|e| !["low", "medium", "high", "xhigh", "max"].contains(&e))
    {
        return Err(invalid("思考档位无效"));
    }
    let _permit = service.acquire(&input.account)?;
    let account = store::account_runtime(host, &input.account).await?;
    if !account.enabled {
        return Err(invalid("账号已停用"));
    }
    let keys = store::list_client_keys(host).await?;
    if !keys.iter().any(|k| k.id == input.key && k.enabled) {
        return Err(invalid("Key 不存在或已停用"));
    }
    if !store::list_models(host, &input.key)
        .await?
        .iter()
        .any(|m| m == &input.model)
    {
        return Err(invalid("所选 Key 不可见该模型"));
    }
    let (library, _) = questions::load(host).await?;
    let question = library.choose(input.question_id.as_deref())?;
    let previous_version = load(host, &input.account)
        .await?
        .map(|(_, version)| version);
    let mut probe = Probe {
        id: format!("test-{:032x}", fastrand::u128(..)),
        account: input.account,
        at_ms: current_ms(),
        question,
        model: input.model,
        key: input.key,
        effort,
        execution: "failed".into(),
        result: None,
        output: String::new(),
        detail: String::new(),
    };
    if serde_json::to_vec(&probe)
        .map_err(|_| invalid("题目无法编码"))?
        .len()
        > MAX_BYTES / 2
    {
        return Err(invalid("题目过大，无法保存测试记录"));
    }
    let output_instruction = match probe.question.grading {
        Grading::Exact => "只输出最终答案，不要输出解释或代码围栏",
        Grading::Manual => {
            "只输出一份完整、自包含的 SVG，根节点带 SVG 命名空间，不要使用外部资源或脚本"
        }
    };
    let mut body = json!({"model": probe.model, "input": format!("{}\n\n{}", probe.question.prompt, output_instruction), "store": false});
    if let Some(effort) = &probe.effort {
        body["reasoning"] = json!({"effort": effort});
    }
    match store::execute_model(host, &probe.key, &probe.model, &probe.account, &body).await {
        Ok(events) => match collect_output(&events) {
            Ok(output) => {
                probe.output = output;
                if serde_json::to_vec(&probe)
                    .map_err(|_| invalid("结果无法编码"))?
                    .len()
                    > MAX_BYTES
                {
                    // 超限输出不回传到管理桥，避免再次超过响应上限；不会截断后判分。
                    probe.output.clear();
                    probe.detail = "输出超过记录大小限制，未判分也未保存".into();
                    return Ok(response(&probe, None, Some(&probe.detail)));
                }
                probe.execution = "succeeded".into();
                probe.grade();
            }
            Err(error) => probe.detail = error.message,
        },
        // 宿主取消仍向上传播，不能在已结束的父调用下继续写入。
        Err(error) if error.code == ErrorCode::Cancelled => return Err(error),
        Err(_) => probe.detail = "模型调用失败".into(),
    }
    match store::save_private(
        host,
        NAMESPACE,
        &format!("acct:{}", probe.account),
        &probe,
        previous_version,
        MAX_BYTES,
    )
    .await
    {
        Ok(version) => Ok(response(&probe, Some(version), None)),
        Err(error) => Ok(response(&probe, None, Some(&error.message))),
    }
}

/// 按事实源整体选择输出，避免每帧回退造成丢增量或重复完成快照。
pub fn collect_output(events: &[ExecutionEvent]) -> Result<String, PluginFault> {
    let mut canonical = BTreeMap::<u32, String>::new();
    let mut wire_delta = String::new();
    let mut wire_final = String::new();
    let mut completed = false;
    let mut incomplete = false;
    for event in events {
        for fact in &event.facts {
            match fact {
                CanonicalEvent::TextDelta { index, text } => {
                    canonical.entry(*index).or_default().push_str(text)
                }
                CanonicalEvent::Completed { reason, .. } => {
                    completed |= *reason == FinishReason::Stop;
                    incomplete |= *reason != FinishReason::Stop;
                }
                _ => {}
            }
        }
        let Some(wire) = &event.wire else {
            continue;
        };
        let (kind, data) = match &wire.payload {
            WirePayload::Json { event, data, .. } => (
                event.as_deref().unwrap_or_default().to_owned(),
                data.clone(),
            ),
            WirePayload::RawJson { body } => (
                String::new(),
                serde_json::from_slice(body).map_err(|_| invalid("响应格式无效"))?,
            ),
            _ => continue,
        };
        let kind = data.get("type").and_then(Value::as_str).unwrap_or(&kind);
        if kind == "response.output_text.delta"
            && let Some(text) = data.get("delta").and_then(Value::as_str)
        {
            wire_delta.push_str(text);
        }
        let response = data.get("response").unwrap_or(&data);
        let status = response
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if ["response.failed", "response.incomplete", "error"].contains(&kind)
            || ["failed", "incomplete", "cancelled"].contains(&status)
        {
            incomplete = true;
        }
        if kind == "response.completed" || status == "completed" {
            completed = true;
            if let Some(output) = response.get("output").and_then(Value::as_array) {
                wire_final = output
                    .iter()
                    .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
                    .flat_map(|item| {
                        item.get("content")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                    })
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect();
            }
        }
    }
    if incomplete || !completed {
        return Err(invalid("模型输出未完整结束"));
    }
    let output = if !canonical.is_empty() {
        canonical.into_values().collect()
    } else if !wire_delta.is_empty() {
        wire_delta
    } else {
        wire_final
    };
    if output.trim().is_empty() {
        return Err(invalid("模型没有正式输出"));
    }
    Ok(output)
}

/// 只返回单份完整 SVG；图像解码不执行脚本，XML 解析也不允许 DTD。
pub fn extract_svg(output: &str) -> Result<&str, PluginFault> {
    let mut candidates = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = output[cursor..].find("<svg") {
        let start = cursor + offset;
        let after = start + 4;
        if !output[after..].starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/') {
            cursor = after;
            continue;
        }
        // 候选结束位置交给 XML 解析确认，允许合法的嵌套 SVG 和自闭合根节点。
        let first_end = output[after..].find('>').map(|end| after + end + 1);
        let end = first_end
            .into_iter()
            .chain(
                output[after..]
                    .match_indices("</svg>")
                    .map(|(end, _)| after + end + 6),
            )
            .find(|&end| roxmltree::Document::parse(&output[start..end]).is_ok())
            .ok_or_else(|| invalid("SVG 未完整结束或格式无效"))?;
        candidates.push(&output[start..end]);
        cursor = end;
    }
    if candidates.len() != 1 {
        return Err(invalid("需要且只能输出一份完整 SVG"));
    }
    // 不能在提取根节点时把危险的文档声明丢掉后再验证。
    if output.contains("<!DOCTYPE") || output.contains("<!ENTITY") {
        return Err(invalid("SVG 不允许 DTD 或外部实体"));
    }
    let svg = candidates[0];
    let document = roxmltree::Document::parse(svg).map_err(|_| invalid("SVG 格式无效"))?;
    let root = document.root_element();
    if root.tag_name().name() != "svg"
        || root.tag_name().namespace() != Some("http://www.w3.org/2000/svg")
    {
        return Err(invalid("SVG 缺少正确的命名空间"));
    }
    Ok(svg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_plugin_sdk::call::model::WireEvent;

    fn probe(grading: Grading) -> Probe {
        let library = questions::Library::default();
        Probe {
            id: "test-one".into(),
            account: "account".into(),
            at_ms: 1,
            question: library
                .choose(Some(if grading == Grading::Exact {
                    "candy"
                } else {
                    "bicycle-svg"
                }))
                .unwrap(),
            model: "model".into(),
            key: "key".into(),
            effort: None,
            execution: "succeeded".into(),
            result: None,
            output: String::new(),
            detail: String::new(),
        }
    }

    fn done() -> ExecutionEvent {
        ExecutionEvent::canonical(CanonicalEvent::Completed {
            id: "response".into(),
            model: None,
            reason: FinishReason::Stop,
        })
    }

    fn wire(kind: &str, data: Value) -> ExecutionEvent {
        ExecutionEvent::wire(WireEvent {
            protocol: "openai".into(),
            payload: WirePayload::Json {
                event: Some(kind.into()),
                data,
                id: None,
                retry: None,
                raw_sse: None,
            },
        })
    }

    #[test]
    fn exact_grading_does_not_accept_embedded_answer() {
        let mut p = probe(Grading::Exact);
        for output in ["21", " \n21\t"] {
            p.output = output.into();
            p.grade();
            assert_eq!(p.result, Some(Verdict::Correct));
        }
        for output in ["不是 21，答案是 22", "21个", "121", "```\n21\n```"] {
            p.output = output.into();
            p.grade();
            assert_eq!(p.result, Some(Verdict::Degraded));
        }
    }

    #[test]
    fn manual_grading_requires_current_successful_test() {
        let mut p = probe(Grading::Manual);
        p.grade();
        assert_eq!(p.result, None);
        assert_eq!(
            p.evaluate("older-test", Verdict::Degraded)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        p.evaluate("test-one", Verdict::Correct).unwrap();
        p.evaluate("test-one", Verdict::Degraded).unwrap();
        assert_eq!(p.result, Some(Verdict::Degraded));
        p.execution = "failed".into();
        assert!(p.evaluate("test-one", Verdict::Correct).is_err());
    }

    #[test]
    fn lock_is_per_account_and_released_on_drop() {
        let service = Service::default();
        let permit = service.acquire("a").unwrap();
        assert!(service.acquire("a").is_err());
        assert!(service.acquire("b").is_ok());
        drop(permit);
        assert!(service.acquire("a").is_ok());
    }

    #[test]
    fn canonical_output_excludes_reasoning_and_duplicate_wire() {
        let events = vec![
            ExecutionEvent::canonical(CanonicalEvent::ReasoningDelta {
                index: 0,
                text: "错误思考".into(),
            }),
            ExecutionEvent::wire(WireEvent {
                protocol: "openai".into(),
                payload: WirePayload::Json {
                    event: Some("response.output_text.delta".into()),
                    data: json!({"delta":"2"}),
                    id: None,
                    retry: None,
                    raw_sse: None,
                },
            })
            .with_fact(CanonicalEvent::TextDelta {
                index: 1,
                text: "2".into(),
            }),
            ExecutionEvent::canonical(CanonicalEvent::TextDelta {
                index: 1,
                text: "1".into(),
            }),
            wire(
                "response.completed",
                json!({"response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"21"}]}]}}),
            ),
            done(),
        ];
        assert_eq!(collect_output(&events).unwrap(), "21");
    }

    #[test]
    fn wire_fallback_keeps_all_deltas_and_reads_all_final_messages() {
        let events = vec![
            wire("response.output_text.delta", json!({"delta":"2"})),
            wire("response.output_text.delta", json!({"delta":"1"})),
            wire(
                "response.completed",
                json!({"response":{"output":[{"type":"message","content":[{"type":"output_text","text":"21"}]}]}}),
            ),
        ];
        assert_eq!(collect_output(&events).unwrap(), "21");
        let events = vec![wire(
            "response.completed",
            json!({"response":{"output":[{"type":"reasoning","content":[{"type":"reasoning_text","text":"不计入"}]},{"type":"message","content":[{"type":"output_text","text":"2"}]},{"type":"message","content":[{"type":"output_text","text":"1"}]}]}}),
        )];
        assert_eq!(collect_output(&events).unwrap(), "21");
    }

    #[test]
    fn incomplete_and_empty_outputs_are_not_graded() {
        assert!(collect_output(&[done()]).is_err());
        assert!(
            collect_output(&[ExecutionEvent::canonical(CanonicalEvent::TextDelta {
                index: 0,
                text: "21".into()
            })])
            .is_err()
        );
        let truncated = ExecutionEvent::canonical(CanonicalEvent::Completed {
            id: "response".into(),
            model: None,
            reason: FinishReason::Length,
        });
        assert!(
            collect_output(&[
                wire("response.output_text.delta", json!({"delta":"21"})),
                truncated
            ])
            .is_err()
        );
    }

    #[test]
    fn svg_extraction_supports_fences_unicode_and_nested_svg() {
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\"><svg><text>自行车</text></svg></svg>";
        assert_eq!(
            extract_svg(&format!("结果：\n```svg\n{svg}\n```")).unwrap(),
            svg
        );
        assert!(extract_svg("<svg xmlns=\"http://www.w3.org/2000/svg\"/>").is_ok());
        assert!(extract_svg(&format!("{svg}{svg}")).is_err());
        assert!(extract_svg("<svg><text>没有命名空间</text></svg>").is_err());
        assert!(extract_svg("<svg xmlns=\"http://www.w3.org/2000/svg\"><text></svg>").is_err());
        assert!(
            extract_svg(&format!(
                "<!DOCTYPE svg [<!ENTITY x SYSTEM 'file:///etc/passwd'>]>{svg}"
            ))
            .is_err()
        );
    }
}
