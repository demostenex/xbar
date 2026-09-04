//! Pure composition of discovered active agents and supplied quota state.
//!
//! This module owns the collector-side semantic join. It has no process
//! discovery, network, timer, runtime, or UI responsibilities.

use std::collections::{BTreeMap, BTreeSet};

use crate::discovery::{AgentInstance, AgentKind, DiscoveryEvent, ProcessIdentity, ProviderKind};
use crate::{AccountIdentity, ProviderUsage, Timestamp, UsageMeter, UsageStatus, UsageSummary};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveAgentUsage {
    pub agent_id: String,
    pub provider_id: String,
    pub account_id: AccountIdentity,
    pub display_name: String,
    pub active_instances: u32,
    pub meters: Vec<UsageMeter>,
    pub summary: UsageSummary,
    pub status: UsageStatus,
    pub fetched_at: Option<Timestamp>,
    pub cache_age_secs: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct GroupKey {
    provider_id: String,
    agent_id: String,
    account_id: AccountIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct QuotaKey {
    provider_id: String,
    account_id: AccountIdentity,
}

#[derive(Clone, Debug, Default)]
pub struct CollectorModel {
    active_instances: BTreeMap<ProcessIdentity, AgentInstance>,
    provider_usage: Vec<ProviderUsage>,
}

impl CollectorModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_discovery_event(&mut self, event: DiscoveryEvent) {
        match event {
            DiscoveryEvent::AgentStarted(instance) => {
                self.active_instances
                    .entry(instance.process)
                    .or_insert(instance);
            }
            DiscoveryEvent::AgentExited(identity) => {
                self.active_instances.remove(&identity);
            }
        }
    }

    /// Replaces the complete authoritative ProviderUsage snapshot.
    ///
    /// Omitted provider/account identities are absent, not implicitly stale
    /// and are not retained by this composition model.
    pub fn replace_provider_usage(&mut self, usage: impl IntoIterator<Item = ProviderUsage>) {
        self.provider_usage = usage.into_iter().collect();
    }

    pub fn active_usage(&self) -> Vec<ActiveAgentUsage> {
        // Unknown scopes intentionally share one uncertainty group. This
        // counts active instances without claiming that they belong to the
        // same human account or fabricating PID-derived account identities.
        let mut groups = BTreeMap::<GroupKey, BTreeSet<ProcessIdentity>>::new();
        for instance in self.active_instances.values() {
            let key = GroupKey {
                provider_id: provider_id(instance.provider).to_owned(),
                agent_id: agent_id(instance.agent).to_owned(),
                account_id: instance.account_scope.clone(),
            };
            groups.entry(key).or_default().insert(instance.process);
        }

        let quotas = self.provider_usage.iter().fold(
            BTreeMap::<QuotaKey, Vec<&ProviderUsage>>::new(),
            |mut map, usage| {
                map.entry(QuotaKey {
                    provider_id: usage.provider_id.clone(),
                    account_id: usage.account_id.clone(),
                })
                .or_default()
                .push(usage);
                map
            },
        );

        groups
            .into_iter()
            .map(|(key, identities)| {
                let account_id = key.account_id.clone();
                let quota = quotas.get(&QuotaKey {
                    provider_id: key.provider_id.clone(),
                    account_id: account_id.clone(),
                });
                let (meters, summary, status, fetched_at, cache_age_secs) =
                    if account_id == AccountIdentity::Unknown {
                        (
                            Vec::new(),
                            empty_summary(),
                            UsageStatus::Unknown,
                            None,
                            None,
                        )
                    } else {
                        match quota {
                            Some(values) if values.len() == 1 => {
                                let usage = values[0];
                                (
                                    usage.meters.clone(),
                                    usage.summary.clone(),
                                    usage.status.clone(),
                                    usage.fetched_at,
                                    usage.cache_age_secs,
                                )
                            }
                            Some(_) => (
                                Vec::new(),
                                empty_summary(),
                                UsageStatus::Unknown,
                                None,
                                None,
                            ),
                            None => (
                                Vec::new(),
                                empty_summary(),
                                UsageStatus::Unavailable,
                                None,
                                None,
                            ),
                        }
                    };
                let display_name = display_name(&key.agent_id).to_owned();
                ActiveAgentUsage {
                    agent_id: key.agent_id,
                    provider_id: key.provider_id,
                    account_id,
                    display_name,
                    active_instances: identities.len() as u32,
                    meters,
                    summary,
                    status,
                    fetched_at,
                    cache_age_secs,
                }
            })
            .collect()
    }
}

fn provider_id(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::OpenAi => "openai",
        ProviderKind::Anthropic => "anthropic",
    }
}

fn agent_id(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Codex => "codex",
        AgentKind::ClaudeCode => "claude-code",
    }
}

fn display_name(agent_id: &str) -> &'static str {
    match agent_id {
        "codex" => "Codex",
        "claude-code" => "Claude",
        _ => unreachable!("agent_id is produced by agent_id"),
    }
}

fn empty_summary() -> UsageSummary {
    UsageSummary {
        primary_meter_id: None,
        remaining_pct: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FetchIssue, UsageValue};
    use std::path::PathBuf;

    fn identity(pid: u32, starttime: u64) -> ProcessIdentity {
        ProcessIdentity { pid, starttime }
    }

    fn agent(pid: u32, starttime: u64, kind: AgentKind, account: AccountIdentity) -> AgentInstance {
        AgentInstance {
            process: identity(pid, starttime),
            agent: kind,
            provider: match kind {
                AgentKind::Codex => ProviderKind::OpenAi,
                AgentKind::ClaudeCode => ProviderKind::Anthropic,
            },
            account_scope: account,
            executable: PathBuf::from("/safe/executable"),
        }
    }

    fn quota(provider_id: &str, account_id: AccountIdentity, status: UsageStatus) -> ProviderUsage {
        ProviderUsage {
            provider_id: provider_id.into(),
            display_name: provider_id.into(),
            account_id,
            meters: vec![UsageMeter {
                id: "primary".into(),
                label: "Primary".into(),
                used_pct: Some(28),
                remaining_pct: Some(72),
                value: Some(UsageValue::Percentage {
                    used_pct: Some(28),
                    remaining_pct: Some(72),
                }),
                reset_at: Some(Timestamp(42)),
            }],
            summary: UsageSummary {
                primary_meter_id: Some("primary".into()),
                remaining_pct: Some(72),
            },
            status,
            fetched_at: Some(Timestamp(7)),
            cache_age_secs: Some(9),
            issue: None,
        }
    }

    fn start(model: &mut CollectorModel, instance: AgentInstance) {
        model.apply_discovery_event(DiscoveryEvent::AgentStarted(instance));
    }

    #[test]
    fn no_agents_hide_existing_quota() {
        let mut model = CollectorModel::new();
        model.replace_provider_usage([quota(
            "openai",
            AccountIdentity::Default,
            UsageStatus::Fresh,
        )]);
        assert!(model.active_usage().is_empty());
    }

    #[test]
    fn groups_same_semantic_agent_and_deduplicates_exact_identity() {
        let mut model = CollectorModel::new();
        let instance = agent(1, 10, AgentKind::Codex, AccountIdentity::Default);
        start(&mut model, instance.clone());
        start(&mut model, instance);
        start(
            &mut model,
            agent(2, 20, AgentKind::Codex, AccountIdentity::Default),
        );
        start(
            &mut model,
            agent(3, 30, AgentKind::Codex, AccountIdentity::Default),
        );
        assert_eq!(model.active_usage()[0].active_instances, 3);
    }

    #[test]
    fn pid_reuse_with_different_starttime_is_distinct() {
        let mut model = CollectorModel::new();
        start(
            &mut model,
            agent(1, 10, AgentKind::Codex, AccountIdentity::Default),
        );
        start(
            &mut model,
            agent(1, 11, AgentKind::Codex, AccountIdentity::Default),
        );
        assert_eq!(model.active_usage()[0].active_instances, 2);
    }

    #[test]
    fn distinct_accounts_and_agents_are_separate_groups() {
        let mut model = CollectorModel::new();
        start(
            &mut model,
            agent(1, 1, AgentKind::Codex, AccountIdentity::Default),
        );
        start(
            &mut model,
            agent(2, 2, AgentKind::Codex, AccountIdentity::Named("A".into())),
        );
        start(
            &mut model,
            agent(3, 3, AgentKind::ClaudeCode, AccountIdentity::Default),
        );
        assert_eq!(model.active_usage().len(), 3);
    }

    #[test]
    fn unknown_accounts_form_one_uncertainty_group() {
        let mut model = CollectorModel::new();
        start(
            &mut model,
            agent(1, 1, AgentKind::Codex, AccountIdentity::Unknown),
        );
        start(
            &mut model,
            agent(2, 2, AgentKind::Codex, AccountIdentity::Unknown),
        );
        let usage = model.active_usage();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].account_id, AccountIdentity::Unknown);
        assert_eq!(usage[0].status, UsageStatus::Unknown);
        assert_eq!(usage[0].active_instances, 2);
    }

    #[test]
    fn exact_account_matching_is_strict() {
        let mut model = CollectorModel::new();
        start(
            &mut model,
            agent(1, 1, AgentKind::Codex, AccountIdentity::Default),
        );
        start(
            &mut model,
            agent(2, 2, AgentKind::Codex, AccountIdentity::Named("A".into())),
        );
        model.replace_provider_usage([
            quota(
                "openai",
                AccountIdentity::Named("A".into()),
                UsageStatus::Fresh,
            ),
            quota(
                "openai",
                AccountIdentity::Named("B".into()),
                UsageStatus::Fresh,
            ),
        ]);
        let usage = model.active_usage();
        assert_eq!(usage[0].status, UsageStatus::Unavailable);
        assert_eq!(usage[1].status, UsageStatus::Fresh);
    }

    #[test]
    fn named_process_does_not_match_default_quota() {
        let mut model = CollectorModel::new();
        start(
            &mut model,
            agent(1, 1, AgentKind::Codex, AccountIdentity::Named("A".into())),
        );
        model.replace_provider_usage([quota(
            "openai",
            AccountIdentity::Default,
            UsageStatus::Fresh,
        )]);
        assert_eq!(model.active_usage()[0].status, UsageStatus::Unavailable);
    }

    #[test]
    fn provider_display_name_never_affects_join() {
        let mut model = CollectorModel::new();
        start(
            &mut model,
            agent(1, 1, AgentKind::Codex, AccountIdentity::Default),
        );
        let mut usage = quota("openai", AccountIdentity::Default, UsageStatus::Fresh);
        usage.display_name = "unrelated label".into();
        model.replace_provider_usage([usage]);
        assert_eq!(model.active_usage()[0].status, UsageStatus::Fresh);
        assert_eq!(model.active_usage()[0].display_name, "Codex");
    }

    #[test]
    fn unknown_never_borrows_quota() {
        let mut model = CollectorModel::new();
        start(
            &mut model,
            agent(1, 1, AgentKind::Codex, AccountIdentity::Unknown),
        );
        model.replace_provider_usage([quota(
            "openai",
            AccountIdentity::Default,
            UsageStatus::Fresh,
        )]);
        assert_eq!(model.active_usage()[0].status, UsageStatus::Unknown);
    }

    #[test]
    fn statuses_and_stale_data_are_preserved() {
        let mut model = CollectorModel::new();
        start(
            &mut model,
            agent(1, 1, AgentKind::Codex, AccountIdentity::Default),
        );
        for status in [
            UsageStatus::Fresh,
            UsageStatus::Stale,
            UsageStatus::Unavailable,
            UsageStatus::Unknown,
        ] {
            model.replace_provider_usage([quota(
                "openai",
                AccountIdentity::Default,
                status.clone(),
            )]);
            let usage = model.active_usage();
            assert_eq!(usage[0].status, status);
            if usage[0].status == UsageStatus::Stale {
                assert_eq!(usage[0].summary.remaining_pct, Some(72));
                assert_eq!(usage[0].meters.len(), 1);
            }
        }
    }

    #[test]
    fn duplicate_quota_identity_is_deterministic_and_safe() {
        let mut model = CollectorModel::new();
        start(
            &mut model,
            agent(1, 1, AgentKind::Codex, AccountIdentity::Default),
        );
        let first = quota("openai", AccountIdentity::Default, UsageStatus::Fresh);
        let mut second = first.clone();
        second.issue = Some(FetchIssue::Other);
        model.replace_provider_usage([first, second]);
        let usage = model.active_usage();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].status, UsageStatus::Unknown);
        assert!(usage[0].meters.is_empty());
    }

    #[test]
    fn duplicate_quota_order_has_identical_unknown_result() {
        let mut model = CollectorModel::new();
        start(
            &mut model,
            agent(1, 1, AgentKind::Codex, AccountIdentity::Default),
        );
        let first = quota("openai", AccountIdentity::Default, UsageStatus::Fresh);
        let mut second = first.clone();
        second.summary.remaining_pct = Some(71);
        model.replace_provider_usage([first.clone(), second.clone()]);
        let left = model.active_usage();
        model.replace_provider_usage([second, first]);
        let right = model.active_usage();
        assert_eq!(left, right);
        assert_eq!(right[0].status, UsageStatus::Unknown);
        assert!(right[0].meters.is_empty());
    }

    #[test]
    fn quota_removed_from_replacement_does_not_survive() {
        let mut model = CollectorModel::new();
        start(
            &mut model,
            agent(1, 1, AgentKind::Codex, AccountIdentity::Default),
        );
        model.replace_provider_usage([
            quota("openai", AccountIdentity::Default, UsageStatus::Fresh),
            quota("anthropic", AccountIdentity::Default, UsageStatus::Fresh),
        ]);
        assert_eq!(model.active_usage()[0].status, UsageStatus::Fresh);
        model.replace_provider_usage([quota(
            "anthropic",
            AccountIdentity::Default,
            UsageStatus::Fresh,
        )]);
        let usage = model.active_usage();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].status, UsageStatus::Unavailable);
    }

    #[test]
    fn lifecycle_transitions_are_exact_and_active_only() {
        let mut model = CollectorModel::new();
        let first = agent(1, 1, AgentKind::Codex, AccountIdentity::Default);
        let second = agent(2, 2, AgentKind::Codex, AccountIdentity::Default);
        model.apply_discovery_event(DiscoveryEvent::AgentStarted(first.clone()));
        model.apply_discovery_event(DiscoveryEvent::AgentStarted(second.clone()));
        assert_eq!(model.active_usage()[0].active_instances, 2);
        model.apply_discovery_event(DiscoveryEvent::AgentExited(first.process));
        assert_eq!(model.active_usage()[0].active_instances, 1);
        model.apply_discovery_event(DiscoveryEvent::AgentExited(second.process));
        assert!(model.active_usage().is_empty());
    }

    #[test]
    fn quota_arrival_and_change_update_active_output() {
        let mut model = CollectorModel::new();
        start(
            &mut model,
            agent(1, 1, AgentKind::Codex, AccountIdentity::Default),
        );
        assert_eq!(model.active_usage()[0].status, UsageStatus::Unavailable);
        model.replace_provider_usage([quota(
            "openai",
            AccountIdentity::Default,
            UsageStatus::Fresh,
        )]);
        assert_eq!(model.active_usage()[0].summary.remaining_pct, Some(72));
        let mut changed = quota("openai", AccountIdentity::Default, UsageStatus::Fresh);
        changed.summary.remaining_pct = Some(71);
        model.replace_provider_usage([changed]);
        assert_eq!(model.active_usage()[0].summary.remaining_pct, Some(71));
    }

    #[test]
    fn quota_changes_with_no_active_process_remain_hidden() {
        let mut model = CollectorModel::new();
        model.replace_provider_usage([quota(
            "openai",
            AccountIdentity::Default,
            UsageStatus::Fresh,
        )]);
        assert!(model.active_usage().is_empty());
        model.replace_provider_usage([quota(
            "openai",
            AccountIdentity::Default,
            UsageStatus::Stale,
        )]);
        assert!(model.active_usage().is_empty());
    }

    #[test]
    fn active_account_uses_quota_then_becomes_hidden_on_last_exit() {
        let mut model = CollectorModel::new();
        let instance = agent(1, 1, AgentKind::Codex, AccountIdentity::Default);
        model.replace_provider_usage([quota(
            "openai",
            AccountIdentity::Default,
            UsageStatus::Fresh,
        )]);
        assert!(model.active_usage().is_empty());
        start(&mut model, instance.clone());
        assert_eq!(model.active_usage()[0].status, UsageStatus::Fresh);
        model.apply_discovery_event(DiscoveryEvent::AgentExited(instance.process));
        assert!(model.active_usage().is_empty());
    }

    #[test]
    fn agent_ids_and_display_names_are_canonical() {
        let mut model = CollectorModel::new();
        start(
            &mut model,
            agent(1, 1, AgentKind::Codex, AccountIdentity::Default),
        );
        start(
            &mut model,
            agent(2, 2, AgentKind::ClaudeCode, AccountIdentity::Default),
        );
        let usage = model.active_usage();
        assert_eq!(usage[0].agent_id, "claude-code");
        assert_eq!(usage[0].display_name, "Claude");
        assert_eq!(usage[1].agent_id, "codex");
        assert_eq!(usage[1].display_name, "Codex");
    }

    #[test]
    fn output_is_deterministic_for_input_order() {
        let instances = [
            agent(3, 3, AgentKind::ClaudeCode, AccountIdentity::Default),
            agent(1, 1, AgentKind::Codex, AccountIdentity::Named("A".into())),
            agent(2, 2, AgentKind::Codex, AccountIdentity::Default),
        ];
        let quotas = [
            quota(
                "openai",
                AccountIdentity::Named("A".into()),
                UsageStatus::Fresh,
            ),
            quota("openai", AccountIdentity::Default, UsageStatus::Fresh),
        ];
        let mut left = CollectorModel::new();
        for instance in &instances {
            start(&mut left, instance.clone());
        }
        left.replace_provider_usage(quotas.clone());
        let mut right = CollectorModel::new();
        for instance in instances.into_iter().rev() {
            start(&mut right, instance);
        }
        right.replace_provider_usage(quotas.into_iter().rev());
        assert_eq!(left.active_usage(), right.active_usage());
    }
}
