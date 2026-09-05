//! Event-driven client for the optional xbar-ai-usage Session-D-Bus service.

use crate::core::{
    AccountIdentity, ActiveAgentUsage, UsageMeter, UsageStatus, UsageSummary, UsageValue,
};
use async_channel::Sender;
use futures_lite::StreamExt;
use xbar_ai_protocol::{
    decode_snapshot, AccountIdentityWire, ActiveAgentUsageWire, UsageStatusWire, UsageValueWire,
};

pub const BUS_NAME: &str = "org.xbar.AiUsage1";
pub const OBJECT_PATH: &str = "/org/xbar/AiUsage1";
pub const INTERFACE: &str = "org.xbar.AiUsage1";

#[derive(Debug, Default)]
pub(crate) struct ActivationGate {
    requested: bool,
}

impl ActivationGate {
    pub(crate) fn request_once(&mut self) -> bool {
        if self.requested {
            false
        } else {
            self.requested = true;
            true
        }
    }
}

pub(crate) async fn subscribe_signal_watcher(
    connection: &zbus::Connection,
    requests: &Sender<super::Request>,
) -> zbus::Result<()> {
    let connection = connection.clone();
    let requests = requests.clone();
    let proxy = zbus::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE).await?;
    let mut signals = proxy.receive_signal("StateChanged").await?;
    let executor = connection.executor().clone();
    executor
        .spawn(
            async move {
                while let Some(signal) = signals.next().await {
                    let Some(owner) = signal.header().sender().map(ToString::to_string) else {
                        continue;
                    };
                    let Ok(payload) = signal.body().deserialize::<Vec<u8>>() else {
                        continue;
                    };
                    if requests
                        .send(super::Request::AiUsageSnapshot { owner, payload })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            },
            "xbar-ai-usage-signals",
        )
        .detach();
    Ok(())
}

pub(crate) fn spawn_get_state(
    connection: &zbus::Connection,
    requests: &Sender<super::Request>,
    owner: String,
) {
    let connection = connection.clone();
    let requests = requests.clone();
    let Some(destination) = unique_owner_destination(&owner) else {
        return;
    };
    let Ok(path) = zbus::zvariant::OwnedObjectPath::try_from(OBJECT_PATH) else {
        return;
    };
    let executor = connection.executor().clone();
    executor
        .spawn(
            async move {
                if std::env::var_os("XBAR_TRACE").is_some() {
                    eprintln!("xbar trace: AI_GETSTATE_STARTED owner={owner}");
                }
                let Ok(proxy) =
                    zbus::Proxy::new_owned(connection, destination, path, INTERFACE).await
                else {
                    return;
                };
                let Ok(payload): Result<Vec<u8>, _> = proxy.call("GetState", &()).await else {
                    return;
                };
                if std::env::var_os("XBAR_TRACE").is_some() {
                    let revision = decode_snapshot(&payload)
                        .ok()
                        .map(|snapshot| snapshot.state_revision);
                    eprintln!(
                        "xbar trace: AI_GETSTATE_COMPLETED owner={owner} revision={revision:?}"
                    );
                }
                let _ = requests
                    .send(super::Request::AiUsageSnapshot { owner, payload })
                    .await;
            },
            "xbar-ai-usage-get-state",
        )
        .detach();
}

fn unique_owner_destination(owner: &str) -> Option<zbus::names::OwnedUniqueName> {
    zbus::names::OwnedUniqueName::try_from(owner.to_owned()).ok()
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AiUsageSubscription {
    pub(crate) current_unique_owner: Option<String>,
    pub(crate) last_revision: Option<u64>,
    current_usage: Vec<ActiveAgentUsage>,
}

impl AiUsageSubscription {
    pub(crate) fn owner_appeared(&mut self, owner: String) -> bool {
        if self.current_unique_owner.as_deref() == Some(owner.as_str()) {
            return false;
        }
        if std::env::var_os("XBAR_TRACE").is_some() {
            eprintln!("xbar trace: AI_OWNER_CHANGED owner={owner}");
        }
        self.current_unique_owner = Some(owner);
        self.last_revision = None;
        let changed = !self.current_usage.is_empty();
        self.current_usage.clear();
        changed
    }

    pub(crate) fn owner_disappeared(&mut self, owner: &str) -> bool {
        if self.current_unique_owner.as_deref() != Some(owner) {
            return false;
        }
        self.current_unique_owner = None;
        self.last_revision = None;
        let changed = !self.current_usage.is_empty();
        self.current_usage.clear();
        if std::env::var_os("XBAR_TRACE").is_some() {
            eprintln!("xbar trace: AI_OWNER_CHANGED owner=");
        }
        changed
    }

    pub(crate) fn accept_snapshot(
        &mut self,
        owner: &str,
        payload: &[u8],
    ) -> Result<SnapshotDisposition, SnapshotError> {
        if self.current_unique_owner.as_deref() != Some(owner) {
            return Ok(SnapshotDisposition::Rejected);
        }
        let snapshot = decode_snapshot(payload).map_err(SnapshotError::Protocol)?;
        if self
            .last_revision
            .is_some_and(|revision| snapshot.state_revision <= revision)
        {
            return Ok(SnapshotDisposition::Rejected);
        }
        let usage = snapshot
            .agents
            .iter()
            .map(wire_to_core)
            .collect::<Result<Vec<_>, _>>()?;
        if std::env::var_os("XBAR_TRACE").is_some() {
            eprintln!(
                "xbar trace: AI_SNAPSHOT_ACCEPTED revision={} agents={}",
                snapshot.state_revision,
                usage.len()
            );
        }
        self.last_revision = Some(snapshot.state_revision);
        self.current_usage = usage;
        Ok(SnapshotDisposition::Accepted(self.current_usage.clone()))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SnapshotDisposition {
    Accepted(Vec<ActiveAgentUsage>),
    Rejected,
}

#[derive(Debug)]
pub(crate) enum SnapshotError {
    Protocol(xbar_ai_protocol::ProtocolError),
    InvalidValue(&'static str),
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(error) => write!(formatter, "{error}"),
            Self::InvalidValue(error) => write!(formatter, "{error}"),
        }
    }
}

fn wire_to_core(agent: &ActiveAgentUsageWire) -> Result<ActiveAgentUsage, SnapshotError> {
    Ok(ActiveAgentUsage {
        agent_id: agent.agent_id.clone(),
        provider_id: agent.provider_id.clone(),
        account_id: account_to_core(&agent.account_id),
        display_name: agent.display_name.clone(),
        active_instances: agent.active_instances,
        meters: agent
            .meters
            .iter()
            .map(|meter| {
                Ok(UsageMeter {
                    id: meter.id.clone(),
                    label: meter.label.clone(),
                    remaining_pct: meter.remaining_pct,
                    used_pct: meter.used_pct,
                    value: meter.value.as_ref().map(value_to_core),
                    reset_at: meter.reset_at.map(nonnegative_timestamp).transpose()?,
                })
            })
            .collect::<Result<Vec<_>, SnapshotError>>()?,
        summary: UsageSummary {
            label: agent.summary.primary_meter_id.clone().unwrap_or_default(),
            remaining_pct: agent.summary.remaining_pct,
        },
        status: status_to_core(&agent.status),
        fetched_at: agent.fetched_at.map(nonnegative_timestamp).transpose()?,
        cache_age_secs: agent.cache_age_secs,
    })
}

fn account_to_core(account: &AccountIdentityWire) -> AccountIdentity {
    match account {
        AccountIdentityWire::Default => AccountIdentity::Default,
        AccountIdentityWire::Named(scope) => AccountIdentity::Named(scope.clone()),
        AccountIdentityWire::Unknown => AccountIdentity::Unknown,
    }
}

fn status_to_core(status: &UsageStatusWire) -> UsageStatus {
    match status {
        UsageStatusWire::Fresh => UsageStatus::Fresh,
        UsageStatusWire::Stale => UsageStatus::Stale,
        UsageStatusWire::Unavailable => UsageStatus::Unavailable,
        UsageStatusWire::Unknown => UsageStatus::Unknown,
    }
}

fn value_to_core(value: &UsageValueWire) -> UsageValue {
    match value {
        UsageValueWire::Percentage {
            used_pct,
            remaining_pct,
        } => UsageValue::Percentage {
            used_pct: *used_pct,
            remaining_pct: *remaining_pct,
        },
        UsageValueWire::Amount { value, unit } => UsageValue::Amount {
            value: value.clone(),
            unit: unit.clone(),
        },
        UsageValueWire::Count { value, unit } => UsageValue::Count {
            value: *value,
            unit: unit.clone(),
        },
        UsageValueWire::Text { value, unit } => UsageValue::Text {
            value: value.clone(),
            unit: unit.clone(),
        },
    }
}

fn nonnegative_timestamp(value: i64) -> Result<u64, SnapshotError> {
    u64::try_from(value).map_err(|_| SnapshotError::InvalidValue("negative timestamp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use xbar_ai_protocol::{
        ActiveAgentUsageWire, AiUsageSnapshotWire, UsageMeterWire, UsageSummaryWire,
        PROTOCOL_VERSION,
    };

    #[test]
    fn activation_is_requested_at_most_once() {
        let mut gate = ActivationGate::default();
        assert!(gate.request_once());
        assert!(!gate.request_once());
    }

    #[test]
    fn activation_reply_is_not_state() {
        let mut gate = ActivationGate::default();
        assert!(gate.request_once());
        assert!(!gate.request_once());
    }
    fn payload(revision: u64, account: AccountIdentityWire) -> Vec<u8> {
        xbar_ai_protocol::encode_snapshot(&AiUsageSnapshotWire {
            protocol_version: PROTOCOL_VERSION,
            state_revision: revision,
            agents: vec![ActiveAgentUsageWire {
                agent_id: "codex".into(),
                provider_id: "openai".into(),
                account_id: account,
                display_name: "Codex".into(),
                active_instances: 1,
                meters: vec![UsageMeterWire {
                    id: "primary".into(),
                    label: "Primary".into(),
                    used_pct: Some(1),
                    remaining_pct: Some(99),
                    value: Some(UsageValueWire::Percentage {
                        used_pct: Some(1),
                        remaining_pct: Some(99),
                    }),
                    reset_at: None,
                }],
                summary: UsageSummaryWire {
                    primary_meter_id: Some("primary".into()),
                    remaining_pct: Some(99),
                },
                status: UsageStatusWire::Fresh,
                fetched_at: None,
                cache_age_secs: None,
            }],
        })
        .unwrap()
    }

    #[test]
    fn owner_generation_resets_revision_and_clears_state() {
        let mut subscription = AiUsageSubscription::default();
        assert!(!subscription.owner_appeared(":1.1".into()));
        assert!(matches!(
            subscription.accept_snapshot(":1.1", &payload(4, AccountIdentityWire::Default)),
            Ok(SnapshotDisposition::Accepted(_))
        ));
        assert!(subscription.owner_appeared(":1.2".into()));
        assert_eq!(subscription.last_revision, None);
        assert!(subscription.current_usage.is_empty());
        assert!(matches!(
            subscription.accept_snapshot(":1.2", &payload(1, AccountIdentityWire::Default)),
            Ok(SnapshotDisposition::Accepted(_))
        ));
    }

    #[test]
    fn stale_and_non_current_snapshots_are_rejected() {
        let mut subscription = AiUsageSubscription::default();
        subscription.owner_appeared(":1.1".into());
        assert!(matches!(
            subscription.accept_snapshot(":1.1", &payload(4, AccountIdentityWire::Default)),
            Ok(SnapshotDisposition::Accepted(_))
        ));
        assert!(matches!(
            subscription.accept_snapshot(":1.1", &payload(4, AccountIdentityWire::Default)),
            Ok(SnapshotDisposition::Rejected)
        ));
        assert!(matches!(
            subscription.accept_snapshot(":1.1", &payload(3, AccountIdentityWire::Default)),
            Ok(SnapshotDisposition::Rejected)
        ));
        assert!(matches!(
            subscription.accept_snapshot(":1.9", &payload(9, AccountIdentityWire::Default)),
            Ok(SnapshotDisposition::Rejected)
        ));
    }

    #[test]
    fn owner_loss_clears_canonical_usage() {
        let mut subscription = AiUsageSubscription::default();
        subscription.owner_appeared(":1.1".into());
        assert!(matches!(
            subscription.accept_snapshot(":1.1", &payload(1, AccountIdentityWire::Default)),
            Ok(SnapshotDisposition::Accepted(_))
        ));
        assert!(subscription.owner_disappeared(":1.1"));
        assert_eq!(subscription.current_unique_owner, None);
        assert_eq!(subscription.last_revision, None);
        assert!(subscription.current_usage.is_empty());
        assert!(!subscription.owner_disappeared(":1.1"));
    }

    #[test]
    fn initial_and_higher_revisions_are_accepted_once() {
        let mut subscription = AiUsageSubscription::default();
        subscription.owner_appeared(":1.1".into());
        assert!(matches!(
            subscription.accept_snapshot(":1.1", &payload(1, AccountIdentityWire::Default)),
            Ok(SnapshotDisposition::Accepted(_))
        ));
        assert!(matches!(
            subscription.accept_snapshot(":1.1", &payload(2, AccountIdentityWire::Default)),
            Ok(SnapshotDisposition::Accepted(_))
        ));
        assert_eq!(subscription.last_revision, Some(2));
    }

    #[test]
    fn invalid_snapshot_does_not_replace_last_valid_state() {
        let mut subscription = AiUsageSubscription::default();
        subscription.owner_appeared(":1.1".into());
        let valid = payload(1, AccountIdentityWire::Default);
        assert!(matches!(
            subscription.accept_snapshot(":1.1", &valid),
            Ok(SnapshotDisposition::Accepted(_))
        ));
        let before = subscription.current_usage.clone();
        assert!(subscription.accept_snapshot(":1.1", b"not-json").is_err());
        assert_eq!(subscription.last_revision, Some(1));
        assert_eq!(subscription.current_usage, before);
    }

    #[test]
    fn named_and_unknown_accounts_are_preserved_without_path_logic() {
        let mut subscription = AiUsageSubscription::default();
        subscription.owner_appeared(":1.1".into());
        let named = match subscription.accept_snapshot(
            ":1.1",
            &payload(1, AccountIdentityWire::Named("scope-v1-abcd".into())),
        ) {
            Ok(SnapshotDisposition::Accepted(usage)) => usage,
            other => panic!("unexpected snapshot result: {other:?}"),
        };
        assert_eq!(
            named[0].account_id,
            AccountIdentity::Named("scope-v1-abcd".into())
        );
        subscription.owner_appeared(":1.2".into());
        let unknown =
            match subscription.accept_snapshot(":1.2", &payload(1, AccountIdentityWire::Unknown)) {
                Ok(SnapshotDisposition::Accepted(usage)) => usage,
                other => panic!("unexpected snapshot result: {other:?}"),
            };
        assert_eq!(unknown[0].account_id, AccountIdentity::Unknown);
    }

    #[test]
    fn initial_owner_lookup_cannot_overwrite_newer_owner_event() {
        let mut subscription = AiUsageSubscription::default();
        subscription.owner_appeared(":1.10".into());
        subscription.owner_appeared(":1.11".into());
        assert_eq!(subscription.current_unique_owner.as_deref(), Some(":1.11"));
        assert!(matches!(
            subscription.accept_snapshot(":1.10", &payload(99, AccountIdentityWire::Default)),
            Ok(SnapshotDisposition::Rejected)
        ));
        assert_eq!(subscription.current_unique_owner.as_deref(), Some(":1.11"));
        assert_eq!(subscription.last_revision, None);
    }

    #[test]
    fn get_state_and_signal_race_keeps_higher_revision() {
        let mut subscription = AiUsageSubscription::default();
        subscription.owner_appeared(":1.57".into());
        assert!(matches!(
            subscription.accept_snapshot(":1.57", &payload(5, AccountIdentityWire::Default)),
            Ok(SnapshotDisposition::Accepted(_))
        ));
        assert!(matches!(
            subscription.accept_snapshot(":1.57", &payload(4, AccountIdentityWire::Default)),
            Ok(SnapshotDisposition::Rejected)
        ));
        assert_eq!(subscription.last_revision, Some(5));
    }

    #[test]
    fn old_owner_loss_cannot_clear_new_owner_generation() {
        let mut subscription = AiUsageSubscription::default();
        subscription.owner_appeared(":1.20".into());
        subscription
            .accept_snapshot(":1.20", &payload(1, AccountIdentityWire::Default))
            .unwrap();
        subscription.owner_appeared(":1.21".into());
        assert!(!subscription.owner_disappeared(":1.20"));
        assert_eq!(subscription.current_unique_owner.as_deref(), Some(":1.21"));
        assert_eq!(subscription.last_revision, None);
    }

    #[test]
    fn direct_owner_replacement_clears_before_new_snapshot() {
        let mut subscription = AiUsageSubscription::default();
        subscription.owner_appeared(":1.30".into());
        subscription
            .accept_snapshot(":1.30", &payload(8, AccountIdentityWire::Default))
            .unwrap();
        assert!(subscription.owner_appeared(":1.31".into()));
        assert!(subscription.current_usage.is_empty());
        assert_eq!(subscription.last_revision, None);
        assert!(matches!(
            subscription.accept_snapshot(":1.31", &payload(1, AccountIdentityWire::Default)),
            Ok(SnapshotDisposition::Accepted(_))
        ));
    }

    #[test]
    fn failed_new_owner_state_leaves_empty_generation() {
        let mut subscription = AiUsageSubscription::default();
        subscription.owner_appeared(":1.40".into());
        subscription
            .accept_snapshot(":1.40", &payload(3, AccountIdentityWire::Default))
            .unwrap();
        subscription.owner_appeared(":1.41".into());
        assert_eq!(subscription.current_unique_owner.as_deref(), Some(":1.41"));
        assert_eq!(subscription.last_revision, None);
        assert!(subscription.current_usage.is_empty());
    }

    #[test]
    fn get_state_target_is_the_unique_owner() {
        assert_eq!(
            unique_owner_destination(":1.57")
                .expect("valid unique name")
                .as_str(),
            ":1.57"
        );
        assert!(unique_owner_destination("org.xbar.AiUsage1").is_none());
    }
}
