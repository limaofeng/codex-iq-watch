//! 账号私有状态存取：`host.state.*`，命名空间 `watch`，键 `acct:<account_id>`。

use gateway_plugin_sdk::client::HostClient;
use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::host::{StateGetRequest, StateGetResult, StatePutRequest},
};

use crate::detector::AccountState;

pub const NAMESPACE: &str = "watch";
const INDEX_KEY: &str = "accounts";

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
                serde_json::from_value::<Vec<serde_json::Value>>(record.value).unwrap_or_default(),
                Some(record.version),
            ),
            None => (Vec::new(), None),
        };
        if !accounts
            .iter()
            .any(|item| item.get("account_id").and_then(|id| id.as_str()) == Some(account_id))
        {
            accounts.push(serde_json::json!({"account_id": account_id, "touched_at_ms": now_ms}));
        } else {
            for item in &mut accounts {
                if item.get("account_id").and_then(|id| id.as_str()) == Some(account_id) {
                    item["touched_at_ms"] = serde_json::json!(now_ms);
                }
            }
        }
        if accounts.len() > 64 {
            accounts.sort_by_key(|item| {
                item.get("touched_at_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            });
            accounts.drain(0..accounts.len() - 64);
        }
        let put = StatePutRequest {
            namespace: NAMESPACE.to_owned(),
            key: INDEX_KEY.to_owned(),
            value: serde_json::json!(accounts),
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

/// 读取账号索引；不存在时返回空列表。
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
        Some(record) => serde_json::from_value(record.value).unwrap_or_default(),
        None => Vec::new(),
    })
}

fn account_key(account_id: &str) -> String {
    format!("acct:{account_id}")
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
