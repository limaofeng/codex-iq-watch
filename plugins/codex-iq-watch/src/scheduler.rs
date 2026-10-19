//! 调度阶段避让：读取候选账号的观察状态，把已判定「降智」的账号从候选中排除。
//!
//! 只有实例配置了「账号调度」绑定后宿主才会在调度阶段回调本处理器；
//! `host.state.get` 与 `host.log` 不受阶段限制，可安全调用。

use std::collections::BTreeSet;

use gateway_plugin_sdk::{
    PluginFault,
    call::policy::{AccountScheduleCandidate, AccountScheduleDecision, AccountScheduleRequest},
    client::{TypedCall, TypedReply},
};

use crate::{App, config::ScheduleFallback, detector::AccountStatus, effective_config, log, store};

/// 调度回调：候选里有降智账号时用 `DelegateFiltered` 排除它们，其余交给宿主调度；
/// 未启用、无需排除、读状态失败时一律 `Delegate`，保证关闭/故障时行为与内置调度一致。
pub(crate) async fn schedule_account(
    app: &App,
    call: TypedCall<AccountScheduleRequest>,
) -> Result<TypedReply<AccountScheduleDecision>, PluginFault> {
    let config = effective_config(app, &call.host).await;
    let request = call.request;
    if !config.schedule_exclude_degraded
        || !config.watches_provider(Some(request.provider.as_str()))
    {
        return Ok(TypedReply::new(AccountScheduleDecision::Delegate));
    }
    let mut degraded: BTreeSet<String> = BTreeSet::new();
    for candidate in &request.candidates {
        match store::load_account(&call.host, &candidate.account_id).await {
            Ok(Some((state, _))) if state.status == AccountStatus::Degraded => {
                degraded.insert(candidate.account_id.clone());
            }
            Ok(_) => {}
            // 读状态失败视为「无法判断」，回退内置调度而不是把正常账号误排除。
            Err(error) => {
                log(&call.host, "watch_schedule_state_failed", &error.message).await;
                return Ok(TypedReply::new(AccountScheduleDecision::Delegate));
            }
        }
    }
    Ok(TypedReply::new(decide(
        &request.candidates,
        &degraded,
        config.schedule_all_degraded,
    )))
}

/// 排除决策：无降智账号时交回内置调度；仍有正常候选时只排除降智账号
/// （`DelegateFiltered`），选号与等待由宿主原生调度完成，忙碌的正常账号会被等待而不是报错；
/// 全部被排除时按配置回落（delegate 仍可用降智账号）或拒绝请求。
fn decide(
    candidates: &[AccountScheduleCandidate],
    degraded: &BTreeSet<String>,
    fallback: ScheduleFallback,
) -> AccountScheduleDecision {
    if degraded.is_empty() {
        return AccountScheduleDecision::Delegate;
    }
    if candidates
        .iter()
        .all(|candidate| degraded.contains(&candidate.account_id))
    {
        return match fallback {
            ScheduleFallback::Delegate => AccountScheduleDecision::Delegate,
            ScheduleFallback::Reject => AccountScheduleDecision::Reject,
        };
    }
    AccountScheduleDecision::DelegateFiltered {
        exclude_account_ids: candidates
            .iter()
            .filter(|candidate| degraded.contains(&candidate.account_id))
            .map(|candidate| candidate.account_id.clone())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_plugin_sdk::call::policy::AccountScheduleCandidate;

    fn candidate(account_id: &str, available: bool) -> AccountScheduleCandidate {
        AccountScheduleCandidate {
            account_id: account_id.to_owned(),
            available,
            weight: 100,
            in_flight: 0,
            maximum_concurrency: 0,
            last_started_at_ms: None,
            quota_reset_at_ms: None,
            quota_remaining_rank: None,
            failure_rate_basis_points: None,
            first_output_latency_ms: None,
        }
    }

    fn filtered(ids: &[&str]) -> AccountScheduleDecision {
        AccountScheduleDecision::DelegateFiltered {
            exclude_account_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    #[test]
    fn no_degraded_delegates() {
        let candidates = vec![candidate("a", true), candidate("b", true)];
        let decision = decide(&candidates, &BTreeSet::new(), ScheduleFallback::Delegate);
        assert_eq!(decision, AccountScheduleDecision::Delegate);
    }

    #[test]
    fn degraded_are_excluded_and_rest_left_to_host() {
        let candidates = vec![
            candidate("busy", false),
            candidate("degraded", true),
            candidate("idle", true),
        ];
        let degraded = BTreeSet::from(["degraded".to_owned()]);
        let decision = decide(&candidates, &degraded, ScheduleFallback::Delegate);
        assert_eq!(decision, filtered(&["degraded"]));
    }

    #[test]
    fn waiting_only_clean_account_is_kept_for_host_to_wait_on() {
        let candidates = vec![candidate("waiting", false), candidate("degraded", true)];
        let degraded = BTreeSet::from(["degraded".to_owned()]);
        let decision = decide(&candidates, &degraded, ScheduleFallback::Reject);
        assert_eq!(decision, filtered(&["degraded"]));
    }

    #[test]
    fn all_degraded_follows_fallback() {
        let candidates = vec![candidate("a", true), candidate("b", false)];
        let degraded = BTreeSet::from(["a".to_owned(), "b".to_owned()]);
        assert_eq!(
            decide(&candidates, &degraded, ScheduleFallback::Delegate),
            AccountScheduleDecision::Delegate
        );
        assert_eq!(
            decide(&candidates, &degraded, ScheduleFallback::Reject),
            AccountScheduleDecision::Reject
        );
    }
}
