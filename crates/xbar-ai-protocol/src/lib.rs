//! Versioned, dependency-light wire types for the collector/xbar boundary.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;
const MAX_AGENTS: usize = 256;
const MAX_METERS_PER_AGENT: usize = 64;
const MAX_STRING_BYTES: usize = 4096;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AiUsageSnapshotWire {
    pub protocol_version: u16,
    pub state_revision: u64,
    pub agents: Vec<ActiveAgentUsageWire>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActiveAgentUsageWire {
    pub agent_id: String,
    pub provider_id: String,
    pub account_id: AccountIdentityWire,
    pub display_name: String,
    pub active_instances: u32,
    pub meters: Vec<UsageMeterWire>,
    pub summary: UsageSummaryWire,
    pub status: UsageStatusWire,
    pub fetched_at: Option<i64>,
    pub cache_age_secs: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value")]
pub enum AccountIdentityWire {
    Default,
    Named(String),
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageMeterWire {
    pub id: String,
    pub label: String,
    pub used_pct: Option<u16>,
    pub remaining_pct: Option<u16>,
    pub value: Option<UsageValueWire>,
    pub reset_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageSummaryWire {
    pub primary_meter_id: Option<String>,
    pub remaining_pct: Option<u16>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value")]
pub enum UsageStatusWire {
    Fresh,
    Stale,
    Unavailable,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value")]
pub enum UsageValueWire {
    Percentage {
        used_pct: Option<u16>,
        remaining_pct: Option<u16>,
    },
    Amount {
        value: String,
        unit: Option<String>,
    },
    Count {
        value: u64,
        unit: Option<String>,
    },
    Text {
        value: String,
        unit: Option<String>,
    },
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u16),
    #[error("snapshot exceeds {MAX_SNAPSHOT_BYTES} bytes")]
    Oversized,
    #[error("malformed snapshot: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("snapshot exceeds structural bounds: {0}")]
    InvalidStructure(&'static str),
}

pub fn encode_snapshot(snapshot: &AiUsageSnapshotWire) -> Result<Vec<u8>, ProtocolError> {
    validate(snapshot)?;
    let encoded = serde_json::to_vec(snapshot)?;
    if encoded.len() > MAX_SNAPSHOT_BYTES {
        return Err(ProtocolError::Oversized);
    }
    Ok(encoded)
}

pub fn decode_snapshot(payload: &[u8]) -> Result<AiUsageSnapshotWire, ProtocolError> {
    if payload.len() > MAX_SNAPSHOT_BYTES {
        return Err(ProtocolError::Oversized);
    }
    let snapshot: AiUsageSnapshotWire = serde_json::from_slice(payload)?;
    validate(&snapshot)?;
    Ok(snapshot)
}

fn validate(snapshot: &AiUsageSnapshotWire) -> Result<(), ProtocolError> {
    if snapshot.protocol_version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(snapshot.protocol_version));
    }
    if snapshot.agents.len() > MAX_AGENTS {
        return Err(ProtocolError::InvalidStructure("too many agents"));
    }
    for agent in &snapshot.agents {
        check_string(&agent.agent_id)?;
        check_string(&agent.provider_id)?;
        check_string(&agent.display_name)?;
        if let AccountIdentityWire::Named(scope) = &agent.account_id {
            check_string(scope)?;
        }
        if agent.meters.len() > MAX_METERS_PER_AGENT {
            return Err(ProtocolError::InvalidStructure("too many meters"));
        }
        check_string(agent.summary.primary_meter_id.as_deref().unwrap_or(""))?;
        for meter in &agent.meters {
            check_string(&meter.id)?;
            check_string(&meter.label)?;
            if let Some(value) = &meter.value {
                match value {
                    UsageValueWire::Amount { value, unit }
                    | UsageValueWire::Text { value, unit } => {
                        check_string(value)?;
                        check_string(unit.as_deref().unwrap_or(""))?;
                    }
                    UsageValueWire::Count { unit, .. } => {
                        check_string(unit.as_deref().unwrap_or(""))?;
                    }
                    UsageValueWire::Percentage { .. } => {}
                }
            }
        }
    }
    Ok(())
}

fn check_string(value: &str) -> Result<(), ProtocolError> {
    if value.len() > MAX_STRING_BYTES {
        Err(ProtocolError::InvalidStructure("string too long"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(agents: Vec<ActiveAgentUsageWire>) -> AiUsageSnapshotWire {
        AiUsageSnapshotWire {
            protocol_version: PROTOCOL_VERSION,
            state_revision: 7,
            agents,
        }
    }

    fn codex(status: UsageStatusWire) -> ActiveAgentUsageWire {
        ActiveAgentUsageWire {
            agent_id: "codex".into(),
            provider_id: "openai".into(),
            account_id: AccountIdentityWire::Default,
            display_name: "Codex".into(),
            active_instances: 2,
            meters: vec![UsageMeterWire {
                id: "session".into(),
                label: "Codex 5h".into(),
                used_pct: Some(9),
                remaining_pct: Some(91),
                value: Some(UsageValueWire::Percentage {
                    used_pct: Some(9),
                    remaining_pct: Some(91),
                }),
                reset_at: None,
            }],
            summary: UsageSummaryWire {
                primary_meter_id: Some("session".into()),
                remaining_pct: Some(91),
            },
            status,
            fetched_at: Some(123),
            cache_age_secs: Some(4),
        }
    }

    #[test]
    fn empty_snapshot_roundtrip() {
        let value = snapshot(Vec::new());
        assert_eq!(
            decode_snapshot(&encode_snapshot(&value).unwrap()).unwrap(),
            value
        );
    }

    #[test]
    fn fresh_codex_roundtrip_preserves_instances_and_percentage() {
        let value = snapshot(vec![codex(UsageStatusWire::Fresh)]);
        let decoded = decode_snapshot(&encode_snapshot(&value).unwrap()).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn account_variants_roundtrip() {
        for account_id in [
            AccountIdentityWire::Unknown,
            AccountIdentityWire::Named("work".into()),
        ] {
            let mut agent = codex(UsageStatusWire::Fresh);
            agent.account_id = account_id;
            let value = snapshot(vec![agent]);
            assert_eq!(
                decode_snapshot(&encode_snapshot(&value).unwrap()).unwrap(),
                value
            );
        }
    }

    #[test]
    fn status_variants_roundtrip() {
        for status in [
            UsageStatusWire::Stale,
            UsageStatusWire::Unavailable,
            UsageStatusWire::Unknown,
        ] {
            let value = snapshot(vec![codex(status)]);
            assert_eq!(
                decode_snapshot(&encode_snapshot(&value).unwrap()).unwrap(),
                value
            );
        }
    }

    #[test]
    fn value_variants_and_optional_fields_roundtrip() {
        let mut agent = codex(UsageStatusWire::Fresh);
        agent.fetched_at = None;
        agent.cache_age_secs = None;
        agent.meters = vec![
            UsageMeterWire {
                id: "amount".into(),
                label: "Amount".into(),
                used_pct: None,
                remaining_pct: None,
                value: Some(UsageValueWire::Amount {
                    value: "1.25".into(),
                    unit: Some("USD".into()),
                }),
                reset_at: Some(42),
            },
            UsageMeterWire {
                id: "count".into(),
                label: "Count".into(),
                used_pct: None,
                remaining_pct: None,
                value: Some(UsageValueWire::Count {
                    value: 3,
                    unit: None,
                }),
                reset_at: None,
            },
            UsageMeterWire {
                id: "text".into(),
                label: "Text".into(),
                used_pct: None,
                remaining_pct: None,
                value: Some(UsageValueWire::Text {
                    value: "unlimited".into(),
                    unit: None,
                }),
                reset_at: None,
            },
        ];
        agent.summary = UsageSummaryWire {
            primary_meter_id: None,
            remaining_pct: None,
        };
        let value = snapshot(vec![agent]);
        assert_eq!(
            decode_snapshot(&encode_snapshot(&value).unwrap()).unwrap(),
            value
        );
    }

    #[test]
    fn multiple_agents_roundtrip_deterministically() {
        let mut claude = codex(UsageStatusWire::Fresh);
        claude.agent_id = "claude-code".into();
        claude.provider_id = "anthropic".into();
        claude.display_name = "Claude".into();
        let value = snapshot(vec![codex(UsageStatusWire::Fresh), claude]);
        let first = encode_snapshot(&value).unwrap();
        let second = encode_snapshot(&value).unwrap();
        assert_eq!(first, second);
        assert_eq!(decode_snapshot(&first).unwrap(), value);
    }

    #[test]
    fn incompatible_protocol_is_rejected() {
        let mut value = snapshot(Vec::new());
        value.protocol_version = 99;
        assert!(matches!(
            encode_snapshot(&value),
            Err(ProtocolError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(matches!(
            decode_snapshot(b"{"),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn oversized_payload_is_rejected_before_decode() {
        assert!(matches!(
            decode_snapshot(&vec![b' '; MAX_SNAPSHOT_BYTES + 1]),
            Err(ProtocolError::Oversized)
        ));
    }

    #[test]
    fn oversized_snapshot_is_rejected_before_encode() {
        let mut agent = codex(UsageStatusWire::Fresh);
        agent.display_name = "x".repeat(MAX_STRING_BYTES + 1);
        assert!(matches!(
            encode_snapshot(&snapshot(vec![agent])),
            Err(ProtocolError::InvalidStructure("string too long"))
        ));
    }
}
