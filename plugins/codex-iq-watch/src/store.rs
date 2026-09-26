//! 账号私有状态存取：`host.state.*`，命名空间 `watch`，键 `acct:<account_id>`。

use std::collections::BTreeMap;

use gateway_plugin_sdk::client::HostClient;
use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::host::{
        AuthGetRequest, AuthListRequest, AuthListResult, AuthRuntimeAccount, ClientKey,
        KeyListRequest, KeyListResult, ModelEventBatch, ModelExecuteRequest, ModelListRequest,
        ModelListResult, ModelOperation, StateDeleteRequest, StateGetRequest, StateGetResult,
        StatePutRequest,
    },
    call::model::ExecutionEvent,
};

use crate::detector::AccountState;

pub const NAMESPACE: &str = "watch";
const INDEX_KEY: &str = "accounts";
const SETTINGS_KEY: &str = "settings";

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("host callback failed: {0}")]
    Callback(String),
    #[error("state record is invalid")]
    Invalid,
}

impl StateError {
    fn fault(&self) -> PluginFault {
        match self {
            Self::Callback(message) => PluginFault::new(ErrorCode::Fault, message.clone()),
            Self::Invalid => PluginFault::new(ErrorCode::Fault, "plugin state record is invalid"),
        }
    }
}

/// 读取账号状态；不存在时返回 `Ok(None)`。
pub async fn load_account(
    host: &HostClient,
    account_id: &str,
) -> Result<Option<(AccountState, u64)>, PluginFault> {
    let request = StateGetRequest {
        namespace: NAMESPACE.to_owned(),
        key: account_key(account_id),
    };
    let reply = host
        .call(
            "host.state.get",
            serde_json::to_value(request).unwrap_or_else(|_| serde_json::json!({})),
            Vec::new(),
        )
        .await
        .map_err(|error| error.into_plugin_fault())?;
    let result: StateGetResult =
        serde_json::from_value(reply.result).map_err(|_| StateError::Invalid.fault())?;
    let Some(record) = result.record else {
        return Ok(None);
    };
    let state: AccountState =
        serde_json::from_value(record.value).map_err(|_| StateError::Invalid.fault())?;
    Ok(Some((state, record.version)))
}

/// CAS 写入账号状态；`version=None` 表示仅当键不存在时创建。
pub async fn save_account(
    host: &HostClient,
    state: &AccountState,
    version: Option<u64>,
) -> Result<u64, PluginFault> {
    let request = StatePutRequest {
        namespace: NAMESPACE.to_owned(),
        key: account_key(&state.account_id),
        value: serde_json::to_value(state).map_err(|_| StateError::Invalid.fault())?,
        expected_version: version,
    };
    let reply = host
        .call(
            "host.state.put",
            serde_json::to_value(request).unwrap_or_else(|_| serde_json::json!({})),
            Vec::new(),
        )
        .await
        .map_err(|error| error.into_plugin_fault())?;
    serde_json::from_value::<gateway_plugin_sdk::call::host::StatePutResult>(reply.result)
        .map(|result| result.version)
        .map_err(|_| StateError::Invalid.fault())
}

/// 读取单个账号的宿主投影；需要 `accounts` 权限，无阶段限制。
/// 观察阶段用它把内部账号 ID 解析成显示名，失败时回退为内部 ID。
pub async fn account_runtime(
    host: &HostClient,
    account_id: &str,
) -> Result<AuthRuntimeAccount, PluginFault> {
    let request = AuthGetRequest {
        account_id: account_id.to_owned(),
    };
    let reply = host
        .call(
            "host.auth.get_runtime",
            serde_json::json!({}),
            serde_json::to_vec(&request).map_err(|_| StateError::Invalid.fault())?,
        )
        .await
        .map_err(|error| error.into_plugin_fault())?;
    serde_json::from_slice(&reply.payload).map_err(|_| StateError::Invalid.fault())
}

/// 解析账号显示名，与宿主账号列表一致：api_key 凭据用用户填的账号名称，
/// 其余（OAuth 等）优先邮箱，其次凭据名；都为空时调用方回退为内部 ID。
pub fn runtime_label(account: &AuthRuntimeAccount) -> Option<String> {
    let name = account.name.trim();
    let email = account
        .email
        .as_ref()
        .map(|email| email.trim())
        .filter(|email| !email.is_empty());
    if account.authentication_kind == "api_key" {
        (!name.is_empty())
            .then(|| name.to_owned())
            .or_else(|| email.map(str::to_owned))
    } else {
        email
            .map(str::to_owned)
            .or_else(|| (!name.is_empty()).then(|| name.to_owned()))
    }
}

/// 管理阶段批量列出账号投影：`account_id` -> 运行视图（含显示名/邮箱）。
pub async fn list_runtime_accounts(
    host: &HostClient,
) -> Result<BTreeMap<String, AuthRuntimeAccount>, PluginFault> {
    let mut accounts = BTreeMap::new();
    let mut cursor = None;
    for _ in 0..10 {
        let request = AuthListRequest {
            provider_id: None,
            cursor,
            limit: 200,
        };
        let reply = host
            .call(
                "host.auth.list",
                serde_json::json!({}),
                serde_json::to_vec(&request).map_err(|_| StateError::Invalid.fault())?,
            )
            .await
            .map_err(|error| error.into_plugin_fault())?;
        let page: AuthListResult =
            serde_json::from_slice(&reply.payload).map_err(|_| StateError::Invalid.fault())?;
        for account in page.accounts {
            accounts.insert(account.account_id.clone(), account);
        }
        let Some(next) = page.next_cursor.filter(|value| !value.is_empty()) else {
            break;
        };
        cursor = Some(next);
    }
    Ok(accounts)
}

/// 删除账号私有状态并清理索引；状态不存在时视为成功（已清空）。
pub async fn delete_account(host: &HostClient, account_id: &str) -> Result<bool, PluginFault> {
    let mut deleted = false;
    if let Some((_, version)) = load_account(host, account_id).await? {
        let request = StateDeleteRequest {
            namespace: NAMESPACE.to_owned(),
            key: account_key(account_id),
            expected_version: version,
        };
        let reply = host
            .call(
                "host.state.delete",
                serde_json::to_value(request).unwrap_or_else(|_| serde_json::json!({})),
                Vec::new(),
            )
            .await
            .map_err(|error| error.into_plugin_fault())?;
        let result: serde_json::Value = reply.result;
        deleted = result
            .get("deleted")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
    }
    let _ = remove_index(host, account_id).await;
    Ok(deleted)
}

/// 从账号索引中移除条目；索引损坏时直接重建，不影响清理结果。
async fn remove_index(host: &HostClient, account_id: &str) -> Result<(), PluginFault> {
    for _ in 0..3 {
        let get = StateGetRequest {
            namespace: NAMESPACE.to_owned(),
            key: INDEX_KEY.to_owned(),
        };
        let reply = host
            .call(
                "host.state.get",
                serde_json::to_value(get).unwrap_or_else(|_| serde_json::json!({})),
                Vec::new(),
            )
            .await
            .map_err(|error| error.into_plugin_fault())?;
        let result: StateGetResult =
            serde_json::from_value(reply.result).map_err(|_| StateError::Invalid.fault())?;
        let (mut accounts, version) = match result.record {
            Some(record) => (
                serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(record.value)
                    .unwrap_or_default(),
                Some(record.version),
            ),
            None => (serde_json::Map::new(), None),
        };
        if accounts.remove(account_id).is_none() {
            return Ok(());
        }
        let put = StatePutRequest {
            namespace: NAMESPACE.to_owned(),
            key: INDEX_KEY.to_owned(),
            value: serde_json::Value::Object(accounts),
            expected_version: version,
        };
        match host
            .call(
                "host.state.put",
                serde_json::to_value(put).unwrap_or_else(|_| serde_json::json!({})),
                Vec::new(),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(error) => {
                let fault = error.into_plugin_fault();
                if fault.code == ErrorCode::Conflict {
                    continue;
                }
                return Err(fault);
            }
        }
    }
    Ok(())
}

/// 维护账号索引键，供管理页列举状态；失败不影响主流程，由调用方降级处理。
pub async fn touch_index(
    host: &HostClient,
    account_id: &str,
    now_ms: u64,
) -> Result<u64, PluginFault> {
    for _ in 0..3 {
        let get = StateGetRequest {
            namespace: NAMESPACE.to_owned(),
            key: INDEX_KEY.to_owned(),
        };
        let reply = host
            .call(
                "host.state.get",
                serde_json::to_value(get).unwrap_or_else(|_| serde_json::json!({})),
                Vec::new(),
            )
            .await
            .map_err(|error| error.into_plugin_fault())?;
        let result: StateGetResult =
            serde_json::from_value(reply.result).map_err(|_| StateError::Invalid.fault())?;
        let (mut accounts, version) = match result.record {
            Some(record) => (
                serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(record.value)
                    .unwrap_or_default(),
                Some(record.version),
            ),
            None => (serde_json::Map::new(), None),
        };
        // 索引是对象：`schema.type=object`；键即 account_id，值为触达摘要。
        let entry = serde_json::json!({"account_id": account_id, "touched_at_ms": now_ms});
        accounts.insert(account_id.to_owned(), entry);
        if accounts.len() > 64 {
            let mut ordered: Vec<(u64, String)> = accounts
                .iter()
                .map(|(id, item)| {
                    (
                        item.get("touched_at_ms")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0),
                        id.clone(),
                    )
                })
                .collect();
            ordered.sort();
            for (_, id) in ordered.drain(0..ordered.len() - 64) {
                accounts.remove(&id);
            }
        }
        let put = StatePutRequest {
            namespace: NAMESPACE.to_owned(),
            key: INDEX_KEY.to_owned(),
            value: serde_json::Value::Object(accounts),
            expected_version: version,
        };
        match host
            .call(
                "host.state.put",
                serde_json::to_value(put).unwrap_or_else(|_| serde_json::json!({})),
                Vec::new(),
            )
            .await
        {
            Ok(reply) => {
                return serde_json::from_value::<gateway_plugin_sdk::call::host::StatePutResult>(
                    reply.result,
                )
                .map(|result| result.version)
                .map_err(|_| StateError::Invalid.fault());
            }
            Err(error) => {
                let fault = error.into_plugin_fault();
                if fault.code == ErrorCode::Conflict {
                    continue;
                }
                return Err(fault);
            }
        }
    }
    Err(StateError::Callback("index write conflict".to_owned()).fault())
}

/// 读取账号索引；不存在时返回空列表。索引存储为 `{account_id: {account_id,touched_at_ms}}` 对象。
pub async fn load_index(host: &HostClient) -> Result<Vec<serde_json::Value>, PluginFault> {
    let get = StateGetRequest {
        namespace: NAMESPACE.to_owned(),
        key: INDEX_KEY.to_owned(),
    };
    let reply = host
        .call(
            "host.state.get",
            serde_json::to_value(get).unwrap_or_else(|_| serde_json::json!({})),
            Vec::new(),
        )
        .await
        .map_err(|error| error.into_plugin_fault())?;
    let result: StateGetResult =
        serde_json::from_value(reply.result).map_err(|_| StateError::Invalid.fault())?;
    Ok(match result.record {
        Some(record) => {
            // 新格式是对象映射；兼容早期未生效的数组数据。
            match serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(
                record.value.clone(),
            ) {
                Ok(map) => map.into_values().collect(),
                Err(_) => serde_json::from_value::<Vec<serde_json::Value>>(record.value)
                    .unwrap_or_default(),
            }
        }
        None => Vec::new(),
    })
}

/// 分页列出客户端 Key；需要 `models` 权限。测试执行借用这些 Key 的身份。
pub async fn list_client_keys(host: &HostClient) -> Result<Vec<ClientKey>, PluginFault> {
    let mut keys = Vec::new();
    let mut cursor = None;
    for _ in 0..10 {
        let request = KeyListRequest { cursor, limit: 100 };
        let reply = host
            .call(
                "host.keys.list",
                serde_json::to_value(request).unwrap_or_else(|_| serde_json::json!({})),
                Vec::new(),
            )
            .await
            .map_err(|error| error.into_plugin_fault())?;
        let page: KeyListResult =
            serde_json::from_value(reply.result).map_err(|_| StateError::Invalid.fault())?;
        keys.extend(page.keys);
        let Some(next) = page.next_cursor.filter(|value| !value.is_empty()) else {
            break;
        };
        cursor = Some(next);
    }
    Ok(keys)
}

/// 查询某个 Key 可见的模型目录；`protocol` 与执行请求一致（openai）。
pub async fn list_models(host: &HostClient, key_id: &str) -> Result<Vec<String>, PluginFault> {
    let request = ModelListRequest {
        client_key_id: key_id.to_owned(),
        protocol: "openai".to_owned(),
        client_version: String::new(),
    };
    let reply = host
        .call(
            "host.models.list",
            serde_json::to_value(request).unwrap_or_else(|_| serde_json::json!({})),
            Vec::new(),
        )
        .await
        .map_err(|error| error.into_plugin_fault())?;
    let result: ModelListResult =
        serde_json::from_value(reply.result).map_err(|_| StateError::Invalid.fault())?;
    Ok(result.models)
}

/// 执行一次嵌套模型调用并返回解码后的执行事件；正文为所选协议的原始 JSON。
pub async fn execute_model(
    host: &HostClient,
    key_id: &str,
    model: &str,
    account_id: &str,
    body: &serde_json::Value,
) -> Result<Vec<ExecutionEvent>, PluginFault> {
    let request = ModelExecuteRequest {
        client_key_id: Some(key_id.to_owned()),
        model: model.to_owned(),
        protocol: "openai".to_owned(),
        operation: ModelOperation::Generate,
        provider: None,
        account_id: Some(account_id.to_owned()),
        previous_response_id: None,
    };
    let reply = host
        .call(
            "host.model.execute",
            serde_json::to_value(request).unwrap_or_else(|_| serde_json::json!({})),
            serde_json::to_vec(body).map_err(|_| StateError::Invalid.fault())?,
        )
        .await
        .map_err(|error| error.into_plugin_fault())?;
    let batch = ModelEventBatch::decode(&reply.payload).map_err(|_| StateError::Invalid.fault())?;
    Ok(batch.events)
}

fn account_key(account_id: &str) -> String {
    format!("acct:{account_id}")
}

/// 管理页保存的通知设置；返回原始值与 CAS 版本。
pub async fn load_settings(
    host: &HostClient,
) -> Result<Option<(serde_json::Value, u64)>, PluginFault> {
    let request = StateGetRequest {
        namespace: NAMESPACE.to_owned(),
        key: SETTINGS_KEY.to_owned(),
    };
    let reply = host
        .call(
            "host.state.get",
            serde_json::to_value(request).unwrap_or_else(|_| serde_json::json!({})),
            Vec::new(),
        )
        .await
        .map_err(|error| error.into_plugin_fault())?;
    let result: StateGetResult =
        serde_json::from_value(reply.result).map_err(|_| StateError::Invalid.fault())?;
    Ok(result.record.map(|record| (record.value, record.version)))
}

/// CAS 写入通知设置；遇版本冲突重读并重试，仍失败才报错。
pub async fn save_settings(
    host: &HostClient,
    value: &serde_json::Value,
) -> Result<u64, PluginFault> {
    for _ in 0..4 {
        let version = load_settings(host)
            .await?
            .map(|(_, version)| version)
            .filter(|version| *version != 0);
        let request = StatePutRequest {
            namespace: NAMESPACE.to_owned(),
            key: SETTINGS_KEY.to_owned(),
            value: value.clone(),
            expected_version: version,
        };
        match host
            .call(
                "host.state.put",
                serde_json::to_value(request).unwrap_or_else(|_| serde_json::json!({})),
                Vec::new(),
            )
            .await
        {
            Ok(reply) => {
                return serde_json::from_value::<gateway_plugin_sdk::call::host::StatePutResult>(
                    reply.result,
                )
                .map(|result| result.version)
                .map_err(|_| StateError::Invalid.fault());
            }
            Err(error) => {
                let fault = error.into_plugin_fault();
                if fault.code != ErrorCode::Conflict {
                    return Err(fault);
                }
            }
        }
    }
    Err(PluginFault::new(
        ErrorCode::Conflict,
        "settings write kept conflicting",
    ))
}

/// 清空通知设置，恢复为宿主配置；键不存在时视为成功。
pub async fn delete_settings(host: &HostClient) -> Result<(), PluginFault> {
    let Some((_, version)) = load_settings(host).await? else {
        return Ok(());
    };
    let request = gateway_plugin_sdk::call::host::StateDeleteRequest {
        namespace: NAMESPACE.to_owned(),
        key: SETTINGS_KEY.to_owned(),
        expected_version: version,
    };
    let reply = host
        .call(
            "host.state.delete",
            serde_json::to_value(request).unwrap_or_else(|_| serde_json::json!({})),
            Vec::new(),
        )
        .await
        .map_err(|error| error.into_plugin_fault())?;
    serde_json::from_value::<serde_json::Value>(reply.result)
        .map(|_| ())
        .map_err(|_| StateError::Invalid.fault())
}

/// 供管理页读取单个账号状态。
pub async fn load_account_view(
    host: &HostClient,
    account_id: &str,
) -> Result<Option<serde_json::Value>, PluginFault> {
    Ok(load_account(host, account_id)
        .await?
        .map(|(state, _)| serde_json::to_value(state).unwrap_or_default()))
}
