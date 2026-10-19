//! 以真实 SDK/插件进程和内存宿主回调验证取消、状态及告警，不连接上游或数据库。
use gateway_plugin_sdk::{
    CallContext, Frame, Handshake, Manifest, Message, Stage,
    client::{read_frame, write_frame},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

struct Host {
    child: Child,
    input: ChildStdin,
    frames: mpsc::Receiver<Frame>,
    records: BTreeMap<String, (Value, u64)>,
    accounts: Vec<Value>,
    revision: u64,
    http_calls: usize,
    model_events: Vec<gateway_plugin_sdk::call::model::ExecutionEvent>,
    model_calls: Vec<Value>,
    reject_probe_writes: bool,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Host {
    fn start(configuration: Value) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codex-iq-watch"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut output = child.stdout.take().unwrap();
        let (sender, frames) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let mut header = [0; 12];
                if output.read_exact(&mut header).is_err() {
                    break;
                }
                let metadata = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
                let payload = u64::from_be_bytes(header[4..].try_into().unwrap()) as usize;
                let mut bytes = header.to_vec();
                bytes.resize(12 + metadata + payload, 0);
                if output.read_exact(&mut bytes[12..]).is_err() {
                    break;
                }
                let frame = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap()
                    .block_on(read_frame(&mut bytes.as_slice()))
                    .unwrap();
                if sender.send(frame).is_err() {
                    break;
                }
            }
        });
        let manifest = Manifest::from_author_slice(include_bytes!("../plugin.json")).unwrap();
        let mut host = Self {
            child,
            input,
            frames,
            records: BTreeMap::new(),
            accounts: vec![],
            revision: 0,
            http_calls: 0,
            model_events: vec![],
            model_calls: vec![],
            reject_probe_writes: false,
        };
        host.send(Frame::control(Message::Hello {
            handshake: Handshake {
                protocol_version: gateway_plugin_sdk::PROTOCOL_VERSION,
                artifact_sha256: "a".repeat(64),
                plugin_id: "xunzhimeng.codex-iq-watch".into(),
                instance_id: "test".into(),
                generation: 1,
                incarnation: "test".into(),
                configuration,
                contributes: manifest.contributes,
            },
        }));
        assert!(matches!(host.recv().message, Message::Ready { .. }));
        host
    }
    fn send(&mut self, frame: Frame) {
        let mut bytes = Vec::new();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(write_frame(&mut bytes, &frame))
            .unwrap();
        self.input.write_all(&bytes).unwrap();
        self.input.flush().unwrap();
    }
    fn recv(&self) -> Frame {
        self.frames
            .recv_timeout(Duration::from_secs(10))
            .expect("插件未按时返回")
    }
    fn call(&mut self, id: u64, stage: Stage, method: &str, params: Value, payload: Value) {
        self.send(Frame {
            message: Message::Call {
                id,
                method: method.into(),
                context: CallContext {
                    request: None,
                    call_id: id,
                    instance_id: "test".into(),
                    generation: 1,
                    incarnation: "test".into(),
                    stage,
                    timeout_ms: 5000,
                    resource_stream: false,
                    resource_scope_id: format!("scope-{id}"),
                    request_id: None,
                    attempt_id: None,
                    account_id: None,
                    credential_revision: None,
                },
                params,
            },
            payload: if payload.is_null() {
                vec![]
            } else {
                serde_json::to_vec(&payload).unwrap()
            },
        });
    }
    fn reply(&mut self, id: u64, result: Value, payload: Vec<u8>) {
        self.send(Frame {
            message: Message::Result { id, result },
            payload,
        });
    }
    fn callback(&mut self, frame: Frame) {
        let Message::Callback {
            id, method, params, ..
        } = frame.message
        else {
            panic!("expected callback")
        };
        match method.as_str() {
            "host.state.get" => {
                let key = record_key(&params);
                let record = self.records.get(&key).map(|(value, version)| json!({"value": value, "version": version, "schema_version": 1}));
                self.reply(id, json!({"record": record}), vec![]);
            }
            "host.state.put" => {
                let key = record_key(&params);
                if self.reject_probe_writes && params["namespace"] == "probes" {
                    self.send(Frame::control(Message::Error {
                        id,
                        error: gateway_plugin_sdk::PluginFault::new(
                            gateway_plugin_sdk::ErrorCode::Capacity,
                            "测试空间已满",
                        ),
                    }));
                    return;
                }
                if params["expected_version"].as_u64()
                    != self.records.get(&key).map(|(_, version)| *version)
                {
                    self.send(Frame::control(Message::Error {
                        id,
                        error: gateway_plugin_sdk::PluginFault::new(
                            gateway_plugin_sdk::ErrorCode::Conflict,
                            "CAS conflict",
                        ),
                    }));
                    return;
                }
                assert!(self.records.contains_key(&key) || self.records.len() < 256);
                self.revision += 1;
                self.records
                    .insert(key, (params["value"].clone(), self.revision));
                self.reply(id, json!({"version": self.revision}), vec![]);
            }
            "host.state.delete" => {
                let key = record_key(&params);
                assert_eq!(
                    params["expected_version"].as_u64(),
                    self.records.get(&key).map(|(_, v)| *v)
                );
                let deleted = self.records.remove(&key).is_some();
                self.reply(id, json!({"deleted":deleted}), vec![]);
            }
            "host.keys.list" => self.reply(
                id,
                json!({"keys":[{"id":"key","name":"测试 Key","enabled":true}],"next_cursor":null}),
                vec![],
            ),
            "host.models.list" => self.reply(id, json!({"models":["test-model"]}), vec![]),
            "host.model.execute" => {
                assert_eq!(params["client_key_id"], "key");
                assert_eq!(params["account_id"], "account");
                self.model_calls
                    .push(serde_json::from_slice(&frame.payload).unwrap());
                let payload = gateway_plugin_sdk::call::host::ModelEventBatch {
                    events: self.model_events.clone(),
                }
                .encode()
                .unwrap();
                self.reply(
                    id,
                    json!({"request_id":"response","events":self.model_events.len()}),
                    payload,
                );
            }
            "host.auth.get_runtime" => {
                let request: Value = serde_json::from_slice(&frame.payload).unwrap();
                let account = account(request["account_id"].as_str().unwrap());
                self.reply(id, json!({}), serde_json::to_vec(&account).unwrap());
            }
            "host.auth.list" => self.reply(
                id,
                json!({}),
                serde_json::to_vec(&json!({"accounts": self.accounts, "next_cursor": null}))
                    .unwrap(),
            ),
            "host.log" => self.reply(id, json!({}), vec![]),
            "host.http.do" => {
                self.http_calls += 1;
                self.reply(
                    id,
                    json!({"status": 200, "headers": [], "stream": null}),
                    b"{}".to_vec(),
                );
            }
            _ => panic!("unexpected callback {method}"),
        }
    }
    fn finish(&mut self, id: u64) -> Frame {
        loop {
            let frame = self.recv();
            if matches!(frame.message, Message::Result { id: actual, .. } if actual == id) {
                return frame;
            }
            self.callback(frame);
        }
    }
    fn put(&mut self, key: &str, value: Value) {
        self.revision += 1;
        self.records.insert(key.into(), (value, self.revision));
    }
}
fn record_key(params: &Value) -> String {
    let key = params["key"].as_str().unwrap();
    if params["namespace"] == "watch" {
        key.into()
    } else {
        format!("{}/{}", params["namespace"].as_str().unwrap(), key)
    }
}
fn account(id: &str) -> Value {
    json!({"account_id":id,"provider_id":"openai","credential_revision":1,"name":id,
        "email":null,"upstream_user_id":null,"upstream_account_id":null,"plan_type":null,
        "authentication_kind":"api_key","enabled":true,"credential_state":"ready",
        "has_refresh_token":false,"access_token_expires_at_ms":null,"next_refresh_at_ms":null})
}
fn observe(host: &mut Host, id: u64, at: u64) {
    host.call(id, Stage::Observation, "observer.observe", json!({
        "event":"request_completed",
        "data":{
            "event_id":format!("event-{id}"),"request_id":format!("request-{id}"),"config_revision":1,
            "operation":"generate","account_id":"account","provider":"openai","completed_at_ms":at,
            "terminal":{"outcome":"succeeded","send_state":"not_sent","attempt_count":1},
            "usage":{"timings":{"first_token_ms":20000}},
        }
    }), Value::Null);
}

#[test]
fn cancelled_notification_keeps_unknown_history_and_does_not_resend_in_cooldown() {
    let mut host = Host::start(
        json!({"webhook_url":"https://example.test/hook","min_signal_kinds":1,"consecutive_triggers":1}),
    );
    observe(&mut host, 1, 2_000_000);
    host.finish(1);
    observe(&mut host, 3, 2_000_001);
    loop {
        let frame = host.recv();
        if matches!(&frame.message, Message::Callback { method, .. } if method == "host.http.do") {
            host.http_calls += 1;
            break;
        }
        host.callback(frame);
    }
    let saved = &host.records["acct:account"].0;
    assert_eq!(saved["last_alert_at_ms"], 2_000_001);
    assert_eq!(saved["alerts"].as_array().unwrap().len(), 1);
    assert!(
        saved["alerts"][0]["deliveries"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("未确认")
    );
    host.send(Frame::control(Message::Cancel { id: 3 }));
    assert!(matches!(host.recv().message, Message::Cancelled { id: 3 }));
    observe(&mut host, 5, 2_000_002);
    host.finish(5);
    assert_eq!(host.http_calls, 1);
    assert_eq!(
        host.records["acct:account"].0["alerts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn status_reads_unindexed_degraded_account_and_reports_retention_limit() {
    let mut host = Host::start(json!({}));
    host.accounts = (0..255)
        .map(|index| account(&format!("a-{index}")))
        .collect();
    host.put(
        "accounts",
        json!({"a-0":{"account_id":"a-0","touched_at_ms":1}}),
    );
    host.put("acct:a-100", json!({"account_id":"a-100","provider":"openai","model":null,"last_observed_at_ms":1,"status":"degraded"}));
    host.call(
        1,
        Stage::Management,
        "management.handle",
        json!({"method":"GET","path":"status","query":"","content_type":null}),
        Value::Null,
    );
    let response: Value = serde_json::from_slice(&host.finish(1).payload).unwrap();
    let found = response["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["account_id"] == "a-100")
        .unwrap();
    assert_eq!(found["status"], "degraded");
    assert!(
        response["storage_warning"]
            .as_str()
            .unwrap()
            .contains("254")
    );
    assert_eq!(host.records["acct:a-100"].0["status"], "degraded");
}

#[test]
fn websocket_events_do_not_create_request_observations() {
    let mut host = Host::start(json!({}));
    host.call(
        1,
        Stage::Observation,
        "observer.observe",
        json!({
            "event": "websocket_response",
            "data": {
                "event_id": "ws-1", "request_id": "request-1", "config_revision": 1,
                "operation": "generate", "protocol": "openai", "provider": "openai",
                "attempt_index": 1, "sequence": 1, "payload_included": true,
                "account_id": "account", "event_type": "response.completed"
            }
        }),
        json!({"type": "response.completed"}),
    );
    host.finish(1);
    assert!(host.records.is_empty());
    assert_eq!(host.http_calls, 0);

    observe(&mut host, 3, 2_000_000);
    host.finish(3);
    let state = &host.records["acct:account"].0;
    assert_eq!(state["events"].as_array().unwrap().len(), 1);
    assert_eq!(state["events"][0]["outcome"], "succeeded");
    assert_eq!(state["events"][0]["first_token_ms"], 20_000);
}

#[test]
fn unmatched_provider_completion_does_not_write_state() {
    let mut host = Host::start(json!({"watch_providers": ["xai"]}));
    observe(&mut host, 1, 2_000_000);
    host.finish(1);
    assert!(host.records.is_empty());
    assert_eq!(host.http_calls, 0);
}

fn management(
    host: &mut Host,
    id: u64,
    method: &str,
    path: &str,
    query: &str,
    body: Value,
) -> Value {
    let id = id * 2 - 1;
    host.call(id, Stage::Management, "management.handle", json!({"method":method,"path":path,"query":query,"content_type":if method == "POST" {Some("application/json")} else {None}}), body);
    let frame = host.finish(id);
    let Message::Result { result, .. } = frame.message else {
        panic!("unexpected result")
    };
    let mut body: Value = serde_json::from_slice(&frame.payload).unwrap();
    body["http_status"] = result["status"].clone();
    body
}

fn output_events(output: &str) -> Vec<gateway_plugin_sdk::call::model::ExecutionEvent> {
    use gateway_plugin_sdk::call::model::{CanonicalEvent, ExecutionEvent, FinishReason};
    vec![
        ExecutionEvent::canonical(CanonicalEvent::TextDelta {
            index: 0,
            text: output.into(),
        }),
        ExecutionEvent::canonical(CanonicalEvent::Completed {
            id: "response".into(),
            model: None,
            reason: FinishReason::Stop,
        }),
    ]
}
fn run_probe(host: &mut Host, id: u64, question: &str) -> Value {
    management(
        host,
        id,
        "POST",
        "probe-run",
        "",
        json!({"account":"account","model":"test-model","key":"key","effort":"low","question_id":question}),
    )
}

#[test]
fn question_library_is_maintainable_versioned_and_empty_is_persistent() {
    let mut host = Host::start(json!({}));
    let initial = management(&mut host, 1, "GET", "questions", "", Value::Null);
    assert_eq!(initial["questions"].as_array().unwrap().len(), 2);
    assert!(host.records.is_empty());
    let draft = json!({"id":"candy","title":"新标题","prompt":"只输出 42","enabled":true,"grading":"exact","answers":["42"]});
    let edited = management(
        &mut host,
        2,
        "POST",
        "question-save",
        "",
        json!({"expected_version":null,"question":draft}),
    );
    assert_eq!(edited["questions"][0]["version"], 2);
    let stale = management(
        &mut host,
        3,
        "POST",
        "question-save",
        "",
        json!({"expected_version":null,"question":draft}),
    );
    assert_eq!(stale["http_status"], 409);
    let removed = management(
        &mut host,
        4,
        "POST",
        "question-delete",
        "",
        json!({"expected_version":edited["version"],"id":"candy"}),
    );
    management(
        &mut host,
        5,
        "POST",
        "question-delete",
        "",
        json!({"expected_version":removed["version"],"id":"bicycle-svg"}),
    );
    let empty = management(&mut host, 6, "GET", "questions", "", Value::Null);
    assert!(empty["questions"].as_array().unwrap().is_empty());
}

#[test]
fn tests_record_full_answers_without_mutating_monitoring_and_report_save_failures() {
    let mut host = Host::start(json!({}));
    host.accounts = vec![account("account")];
    host.model_events = output_events("不是 21，答案是 22");
    let wrong = run_probe(&mut host, 1, "candy");
    assert_eq!(wrong["probe"]["result"], "degraded");
    assert_eq!(wrong["recorded"], true);
    assert!(!host.records.contains_key("acct:account"));
    assert!(!host.records.contains_key("accounts"));
    assert_eq!(host.model_calls[0]["reasoning"]["effort"], "low");
    host.model_events = output_events("21");
    let correct = run_probe(&mut host, 2, "candy");
    assert_eq!(correct["probe"]["result"], "correct");
    assert_ne!(correct["probe"]["id"], wrong["probe"]["id"]);
    host.reject_probe_writes = true;
    let failed_save = run_probe(&mut host, 3, "candy");
    assert_eq!(failed_save["recorded"], false);
    assert!(
        failed_save["storage_error"]
            .as_str()
            .unwrap()
            .contains("已满")
    );
    let persisted = management(&mut host, 4, "GET", "probe", "account=account", Value::Null);
    assert_eq!(persisted["probe"]["id"], correct["probe"]["id"]);
}

#[test]
fn manual_svg_verdict_is_cas_protected_and_does_not_exclude_account() {
    let mut host = Host::start(json!({"schedule_exclude_degraded":true}));
    host.accounts = vec![account("account")];
    host.model_events = output_events(
        "```svg\n<svg xmlns=\"http://www.w3.org/2000/svg\"><text>自行车</text></svg>\n```",
    );
    let probe = run_probe(&mut host, 1, "bicycle-svg");
    assert!(probe["preview"]["svg"].as_str().unwrap().contains("自行车"));
    assert!(probe["probe"]["result"].is_null());
    let verdict = json!({"account":"account","id":probe["probe"]["id"],"expected_version":probe["version"],"result":"degraded"});
    let marked = management(&mut host, 2, "POST", "probe-verdict", "", verdict.clone());
    assert_eq!(marked["probe"]["result"], "degraded");
    assert_eq!(
        management(&mut host, 3, "POST", "probe-verdict", "", verdict.clone())["http_status"],
        409
    );
    let status = management(&mut host, 4, "GET", "status", "", Value::Null);
    assert_eq!(status["accounts"][0]["excluded_from_schedule"], false);
    assert_eq!(status["accounts"][0]["status"], "unobserved");
    let newer = run_probe(&mut host, 5, "bicycle-svg");
    let mut stale_id = verdict;
    stale_id["expected_version"] = newer["version"].clone();
    let outdated = management(&mut host, 6, "POST", "probe-verdict", "", stale_id);
    assert_eq!(outdated["http_status"], 409);
    management(
        &mut host,
        7,
        "POST",
        "account-clear",
        "",
        json!({"account":"account"}),
    );
    assert!(!host.records.contains_key("probes/acct:account"));
}

#[test]
fn oversized_and_incomplete_outputs_are_not_marked_degraded() {
    use gateway_plugin_sdk::call::model::{CanonicalEvent, ExecutionEvent};
    let mut host = Host::start(json!({}));
    host.model_events = output_events(&"x".repeat(65536));
    let oversized = run_probe(&mut host, 1, "candy");
    assert!(oversized["probe"]["result"].is_null());
    assert_eq!(oversized["recorded"], false);
    host.model_events = vec![ExecutionEvent::canonical(CanonicalEvent::TextDelta {
        index: 0,
        text: "21".into(),
    })];
    let incomplete = run_probe(&mut host, 2, "candy");
    assert!(incomplete["probe"]["result"].is_null());
    assert_eq!(incomplete["probe"]["execution"], "failed");
}

#[test]
fn cancelling_model_call_releases_account_lock_without_writing_probe() {
    let mut host = Host::start(json!({}));
    host.call(1, Stage::Management, "management.handle", json!({"method":"POST","path":"probe-run","query":"","content_type":"application/json"}), json!({"account":"account","model":"test-model","key":"key","effort":"low","question_id":"candy"}));
    loop {
        let frame = host.recv();
        if matches!(&frame.message, Message::Callback { method, .. } if method == "host.model.execute")
        {
            break;
        }
        host.callback(frame);
    }
    host.send(Frame::control(Message::Cancel { id: 1 }));
    assert!(matches!(host.recv().message, Message::Cancelled { id: 1 }));
    assert!(!host.records.contains_key("probes/acct:account"));
    host.model_events = output_events("21");
    assert_eq!(run_probe(&mut host, 2, "candy")["recorded"], true);
}

#[test]
fn status_and_probe_preserve_legacy_records_until_a_new_test_is_saved() {
    let mut host = Host::start(json!({}));
    host.accounts = vec![account("account")];
    let legacy =
        json!({"at_ms":1,"model":"old-model","result":"correct","detail":"旧测试","effort":null});
    host.put(
        "acct:account",
        json!({"account_id":"account","last_observed_at_ms":1,"status":"degraded","last_probe":legacy}),
    );
    let status = management(&mut host, 1, "GET", "status", "", Value::Null);
    assert_eq!(status["accounts"][0]["last_probe"], legacy);
    let old = management(&mut host, 2, "GET", "probe", "account=account", Value::Null);
    assert_eq!(old["legacy"], legacy);
    host.model_events = output_events("21");
    let new = run_probe(&mut host, 3, "candy");
    assert_eq!(host.records["acct:account"].0["status"], "degraded");
    let status = management(&mut host, 4, "GET", "status", "", Value::Null);
    assert_eq!(
        status["accounts"][0]["last_probe"]["id"],
        new["probe"]["id"]
    );
}
