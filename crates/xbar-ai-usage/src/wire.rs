//! Conversion from collector-owned DTOs to the dependency-light wire DTOs.

use sha2::{Digest, Sha256};
use xbar_ai_protocol::{
    encode_snapshot, AccountIdentityWire, ActiveAgentUsageWire, AiUsageSnapshotWire, ProtocolError,
    UsageMeterWire, UsageStatusWire, UsageSummaryWire, UsageValueWire,
};

use crate::{AccountIdentity, ActiveAgentUsage, UsageMeter, UsageStatus, UsageValue};

pub fn encode_active_usage(
    state_revision: u64,
    usage: &[ActiveAgentUsage],
) -> Result<Vec<u8>, ProtocolError> {
    encode_snapshot(&AiUsageSnapshotWire {
        protocol_version: xbar_ai_protocol::PROTOCOL_VERSION,
        state_revision,
        agents: usage.iter().map(active_agent_usage).collect(),
    })
}

pub fn active_agent_usage(usage: &ActiveAgentUsage) -> ActiveAgentUsageWire {
    ActiveAgentUsageWire {
        agent_id: usage.agent_id.clone(),
        provider_id: usage.provider_id.clone(),
        account_id: account_identity(&usage.account_id),
        display_name: usage.display_name.clone(),
        active_instances: usage.active_instances,
        meters: usage.meters.iter().map(usage_meter).collect(),
        summary: UsageSummaryWire {
            primary_meter_id: usage.summary.primary_meter_id.clone(),
            remaining_pct: usage.summary.remaining_pct,
        },
        status: usage_status(&usage.status),
        fetched_at: usage.fetched_at.map(|timestamp| timestamp.0),
        cache_age_secs: usage.cache_age_secs,
    }
}

fn account_identity(account: &AccountIdentity) -> AccountIdentityWire {
    match account {
        AccountIdentity::Default => AccountIdentityWire::Default,
        AccountIdentity::Named(scope) => AccountIdentityWire::Named(opaque_scope_id(scope)),
        AccountIdentity::Unknown => AccountIdentityWire::Unknown,
    }
}

/// Identifies a proven local configuration scope without transporting its
/// filesystem path. The prefix/version keeps this representation explicit at
/// the wire boundary; it is not a human provider-account identifier.
fn opaque_scope_id(scope: &str) -> String {
    let digest = Sha256::digest(scope.as_bytes());
    let mut encoded = String::with_capacity("scope-v1-".len() + digest.len() * 2);
    encoded.push_str("scope-v1-");
    for byte in digest {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

fn usage_status(status: &UsageStatus) -> UsageStatusWire {
    match status {
        UsageStatus::Fresh => UsageStatusWire::Fresh,
        UsageStatus::Stale => UsageStatusWire::Stale,
        UsageStatus::Unavailable => UsageStatusWire::Unavailable,
        UsageStatus::Unknown => UsageStatusWire::Unknown,
    }
}

fn usage_meter(meter: &UsageMeter) -> UsageMeterWire {
    UsageMeterWire {
        id: meter.id.clone(),
        label: meter.label.clone(),
        used_pct: meter.used_pct,
        remaining_pct: meter.remaining_pct,
        value: meter.value.as_ref().map(usage_value),
        reset_at: meter.reset_at.map(|timestamp| timestamp.0),
    }
}

fn usage_value(value: &UsageValue) -> UsageValueWire {
    match value {
        UsageValue::Percentage {
            used_pct,
            remaining_pct,
        } => UsageValueWire::Percentage {
            used_pct: *used_pct,
            remaining_pct: *remaining_pct,
        },
        UsageValue::Amount { value, unit } => UsageValueWire::Amount {
            value: value.clone(),
            unit: unit.clone(),
        },
        UsageValue::Count { value, unit } => UsageValueWire::Count {
            value: *value,
            unit: unit.clone(),
        },
        UsageValue::Text { value, unit } => UsageValueWire::Text {
            value: value.clone(),
            unit: unit.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Timestamp, UsageSummary};

    fn usage(account_id: AccountIdentity) -> ActiveAgentUsage {
        ActiveAgentUsage {
            agent_id: "codex".into(),
            provider_id: "openai".into(),
            account_id,
            display_name: "Codex".into(),
            active_instances: 1,
            meters: Vec::new(),
            summary: UsageSummary {
                primary_meter_id: None,
                remaining_pct: None,
            },
            status: UsageStatus::Unavailable,
            fetched_at: Some(Timestamp(1)),
            cache_age_secs: None,
        }
    }

    #[test]
    fn named_account_scope_is_opaque_and_distinct() {
        let first =
            match active_agent_usage(&usage(AccountIdentity::Named("/home/user/.codex-a".into())))
                .account_id
            {
                AccountIdentityWire::Named(value) => value,
                other => panic!("unexpected account identity: {other:?}"),
            };
        let second =
            match active_agent_usage(&usage(AccountIdentity::Named("/home/user/.codex-b".into())))
                .account_id
            {
                AccountIdentityWire::Named(value) => value,
                other => panic!("unexpected account identity: {other:?}"),
            };
        assert_ne!(first, second);
        assert!(first.starts_with("scope-v1-"));
        assert!(!first.contains("/home/user"));
    }
}
