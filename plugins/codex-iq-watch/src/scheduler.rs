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

/// 调度回调：候选里有降智账号时，在剩余候选中按宿主同款次序挑一个 `Pick`；
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

/// 挑选决策：无排除项时交回内置调度；有排除项且仍有正常候选时按宿主内置同款
/// 次序（in_flight 最少、失败率最低、权重最大、ID 字典序）挑一个；全部被排除时
/// 按配置回落（delegate 仍可用降智账号）或拒绝请求。
fn decide(
    candidates: &[AccountScheduleCandidate],
    degraded: &BTreeSet<String>,
    fallback: ScheduleFallback,
) -> AccountScheduleDecision {
    if degraded.is_empty() {
        return AccountScheduleDecision::Delegate;
    }
    let selected = candidates
        .iter()
        .filter(|candidate| !degraded.contains(&candidate.account_id))
        .min_by_key(|candidate| {
            (
                candidate.in_flight,
                candidate.failure_rate_basis_points.unwrap_or_default(),
                std::cmp::Reverse(candidate.weight),
                candidate.account_id.as_str(),
            )
        });
    match selected {
        Some(candidate) => AccountScheduleDecision::Pick {
            account_id: candidate.account_id.clone(),
        },
        None => match fallback {
            ScheduleFallback::Delegate => AccountScheduleDecision::Delegate,
            ScheduleFallback::Reject => AccountScheduleDecision::Reject,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_plugin_sdk::call::policy::AccountScheduleCandidate;

    fn candidate(account_id: &str, in_flight: u32, weight: u16) -> AccountScheduleCandidate {
        AccountScheduleCandidate {
            account_id: account_id.to_owned(),
            weight,
            in_flight,
            maximum_concurrency: 0,
            last_started_at_ms: None,
            quota_reset_at_ms: None,
            quota_remaining_rank: None,
            failure_rate_basis_points: None,
            first_output_latency_ms: None,
        }
    }

    #[test]
    fn no_degraded_delegates() {
        let candidates = vec![candidate("a", 0, 100), candidate("b", 0, 100)];
        let decision = decide(&candidates, &BTreeSet::new(), ScheduleFallback::Delegate);
        assert_eq!(decision, AccountScheduleDecision::Delegate);
    }

    #[test]
    fn degraded_filtered_picks_least_busy_clean_account() {
        let candidates = vec![
            candidate("busy", 3, 100),
            candidate("degraded", 0, 100),
            candidate("idle", 1, 100),
        ];
        let degraded = BTreeSet::from(["degraded".to_owned()]);
        let decision = decide(&candidates, &degraded, ScheduleFallback::Delegate);
        assert_eq!(
            decision,
            AccountScheduleDecision::Pick {
                account_id: "idle".to_owned()
            }
        );
    }

    #[test]
    fn all_degraded_follows_fallback() {
        let candidates = vec![candidate("a", 0, 100), candidate("b", 0, 100)];
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

    #[test]
    fn pick_order_prefers_failure_rate_then_weight() {
        let candidates = vec![
            candidate("worse", 0, 50),
            candidate("better", 0, 100),
            candidate("degraded", 0, 100),
        ];
        let degraded = BTreeSet::from(["degraded".to_owned()]);
        let decision = decide(&candidates, &degraded, ScheduleFallback::Delegate);
        assert_eq!(
            decision,
            AccountScheduleDecision::Pick {
                account_id: "better".to_owned()
            }
        );
    }
}
