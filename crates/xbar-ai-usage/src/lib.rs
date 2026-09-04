use std::path::Path;
use std::time::Duration;

use ai_usagebar::cache::{Cache, DEFAULT_TTL};
use ai_usagebar::usage::{AnthropicSnapshot, ExtraUsage, OpenAiSnapshot, UsageWindow};

pub mod composition;
pub mod discovery;
pub mod runtime;

pub use composition::{ActiveAgentUsage, CollectorModel};
pub use discovery::{
    ActiveAgentSet, AgentInstance, AgentKind, Discovery, DiscoveryError, DiscoveryEvent,
    ProcessIdentity, ProcessRecord, ProviderKind,
};

const PROVIDER_OPENAI: &str = "openai";
const PROVIDER_ANTHROPIC: &str = "anthropic";

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AccountIdentity {
    Default,
    Named(String),
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Timestamp(pub i64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsageStatus {
    Fresh,
    Stale,
    Unavailable,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsageValue {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageMeter {
    pub id: String,
    pub label: String,
    pub used_pct: Option<u16>,
    pub remaining_pct: Option<u16>,
    pub value: Option<UsageValue>,
    pub reset_at: Option<Timestamp>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageSummary {
    pub primary_meter_id: Option<String>,
    pub remaining_pct: Option<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderUsage {
    pub provider_id: String,
    pub display_name: String,
    pub account_id: AccountIdentity,
    pub meters: Vec<UsageMeter>,
    pub summary: UsageSummary,
    pub status: UsageStatus,
    pub fetched_at: Option<Timestamp>,
    pub cache_age_secs: Option<u64>,
    pub issue: Option<FetchIssue>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FetchIssue {
    Credentials,
    Transport,
    Http,
    Schema,
    Other,
}

impl ProviderUsage {
    fn unavailable(provider_id: &str, account_id: AccountIdentity, issue: FetchIssue) -> Self {
        Self {
            provider_id: provider_id.into(),
            display_name: provider_id.into(),
            account_id,
            meters: Vec::new(),
            summary: UsageSummary {
                primary_meter_id: None,
                remaining_pct: None,
            },
            status: UsageStatus::Unavailable,
            fetched_at: None,
            cache_age_secs: None,
            issue: Some(issue),
        }
    }
}

fn remaining_from_used(used: i32) -> Option<u16> {
    (0..=100).contains(&used).then_some((100 - used) as u16)
}

fn timestamp_from_window(window: &UsageWindow) -> Option<Timestamp> {
    window.resets_at.map(|value| Timestamp(value.timestamp()))
}

fn percentage_meter(id: &str, label: &str, window: &UsageWindow) -> UsageMeter {
    let used_pct = u16::try_from(window.utilization_pct)
        .ok()
        .filter(|value| *value <= 100);
    let remaining_pct = remaining_from_used(window.utilization_pct);
    UsageMeter {
        id: id.into(),
        label: label.into(),
        used_pct,
        remaining_pct,
        value: Some(UsageValue::Percentage {
            used_pct,
            remaining_pct,
        }),
        reset_at: timestamp_from_window(window),
    }
}

fn amount_meter(id: &str, label: &str, value: String, unit: Option<String>) -> UsageMeter {
    UsageMeter {
        id: id.into(),
        label: label.into(),
        used_pct: None,
        remaining_pct: None,
        value: Some(UsageValue::Amount { value, unit }),
        reset_at: None,
    }
}

fn text_meter(id: &str, label: &str, value: String, unit: Option<String>) -> UsageMeter {
    UsageMeter {
        id: id.into(),
        label: label.into(),
        used_pct: None,
        remaining_pct: None,
        value: Some(UsageValue::Text { value, unit }),
        reset_at: None,
    }
}

fn map_issue(error: &ai_usagebar::AppError) -> FetchIssue {
    match error {
        ai_usagebar::AppError::Credentials(_) => FetchIssue::Credentials,
        ai_usagebar::AppError::Transport(_) => FetchIssue::Transport,
        ai_usagebar::AppError::Http { .. } => FetchIssue::Http,
        ai_usagebar::AppError::Schema(_) | ai_usagebar::AppError::Json(_) => FetchIssue::Schema,
        ai_usagebar::AppError::Io { .. }
        | ai_usagebar::AppError::IoBare(_)
        | ai_usagebar::AppError::Toml(_)
        | ai_usagebar::AppError::Other(_) => FetchIssue::Other,
    }
}

fn outcome_metadata(stale: bool, cache_age: Option<Duration>) -> (UsageStatus, Option<u64>) {
    (
        if stale {
            UsageStatus::Stale
        } else {
            UsageStatus::Fresh
        },
        cache_age.map(|age| age.as_secs()),
    )
}

fn adapt_openai_snapshot(
    snapshot: OpenAiSnapshot,
    account_id: AccountIdentity,
    stale: bool,
    cache_age: Option<Duration>,
) -> ProviderUsage {
    let (status, cache_age_secs) = outcome_metadata(stale, cache_age);
    let mut meters = Vec::new();
    if let Some(window) = snapshot.session.as_ref() {
        meters.push(percentage_meter("session", "Codex 5h", window));
    }
    if let Some(window) = snapshot.weekly.as_ref() {
        meters.push(percentage_meter("weekly", "Codex weekly", window));
    }
    if let Some(window) = snapshot.code_review.as_ref() {
        meters.push(percentage_meter("code-review", "Code review", window));
    }
    if let Some(credits) = snapshot.credits.as_ref() {
        meters.push(amount_meter(
            "credits-balance",
            "Credits",
            credits.balance.clone(),
            Some("USD".into()),
        ));
        if let Some((low, high)) = credits.approx_local_messages {
            meters.push(text_meter(
                "credits-local-messages",
                "Local messages",
                format!("{low}-{high}"),
                Some("approximate range".into()),
            ));
        }
        if let Some((low, high)) = credits.approx_cloud_messages {
            meters.push(text_meter(
                "credits-cloud-messages",
                "Cloud messages",
                format!("{low}-{high}"),
                Some("approximate range".into()),
            ));
        }
    }
    meters.sort_by(|left, right| left.id.cmp(&right.id));
    let primary_meter_id = if snapshot.session.is_some() {
        Some("session".into())
    } else if snapshot.weekly.is_some() {
        Some("weekly".into())
    } else {
        None
    };
    let remaining_pct = primary_meter_id.as_deref().and_then(|id| {
        meters
            .iter()
            .find(|meter| meter.id == id)
            .and_then(|meter| meter.remaining_pct)
    });
    ProviderUsage {
        provider_id: PROVIDER_OPENAI.into(),
        display_name: snapshot.plan,
        account_id,
        meters,
        summary: UsageSummary {
            primary_meter_id,
            remaining_pct,
        },
        status,
        fetched_at: None,
        cache_age_secs,
        issue: None,
    }
}

fn add_extra_usage(meters: &mut Vec<UsageMeter>, extra: &ExtraUsage) {
    let unit = extra.currency.clone();
    meters.push(amount_meter(
        "extra-spent",
        "Extra usage spent",
        extra.spent.0.to_string(),
        unit.clone(),
    ));
    if let Some(limit) = extra.limit {
        meters.push(amount_meter(
            "extra-limit",
            "Extra usage limit",
            limit.0.to_string(),
            unit,
        ));
    }
}

fn adapt_anthropic_snapshot(
    snapshot: AnthropicSnapshot,
    account_id: AccountIdentity,
    stale: bool,
    cache_age: Option<Duration>,
) -> ProviderUsage {
    let (status, cache_age_secs) = outcome_metadata(stale, cache_age);
    let mut meters = vec![
        percentage_meter("session", "Session", &snapshot.session),
        percentage_meter("weekly", "Weekly", &snapshot.weekly),
    ];
    if let Some(window) = snapshot.sonnet.as_ref() {
        meters.push(percentage_meter("sonnet", "Sonnet", window));
    }
    for (index, scoped) in snapshot.scoped.iter().enumerate() {
        meters.push(percentage_meter(
            &format!("scoped-{index}"),
            &scoped.label,
            &scoped.window,
        ));
    }
    if let Some(extra) = snapshot.extra.as_ref() {
        add_extra_usage(&mut meters, extra);
    }
    meters.sort_by(|left, right| left.id.cmp(&right.id));
    let primary_meter_id = ["session", "weekly"].into_iter().find(|id| {
        meters
            .iter()
            .find(|meter| meter.id == *id)
            .and_then(|meter| meter.remaining_pct)
            .is_some()
    });
    let remaining_pct = primary_meter_id.and_then(|id| {
        meters
            .iter()
            .find(|meter| meter.id == id)
            .and_then(|meter| meter.remaining_pct)
    });
    ProviderUsage {
        provider_id: PROVIDER_ANTHROPIC.into(),
        display_name: snapshot.plan,
        account_id,
        meters,
        summary: UsageSummary {
            primary_meter_id: primary_meter_id.map(str::to_owned),
            remaining_pct,
        },
        status,
        fetched_at: None,
        cache_age_secs,
        issue: None,
    }
}

/// Fetches Codex usage through the pinned upstream Rust API and returns only
/// local owned DTOs. Upstream errors are reduced to a non-sensitive category.
pub async fn fetch_openai(
    credentials_path: &Path,
    cache_path: &Path,
    account_id: AccountIdentity,
) -> ProviderUsage {
    let client = reqwest::Client::new();
    let cache = Cache::at(cache_path.to_path_buf());
    let outcome = ai_usagebar::openai::fetch::fetch_snapshot(
        &client,
        credentials_path,
        &cache,
        &ai_usagebar::openai::fetch::Endpoints::default(),
        DEFAULT_TTL,
    )
    .await;
    match outcome {
        Ok(outcome) => adapt_openai_snapshot(
            outcome.snapshot,
            account_id,
            outcome.stale,
            outcome.cache_age,
        ),
        Err(error) => ProviderUsage::unavailable(PROVIDER_OPENAI, account_id, map_issue(&error)),
    }
}

/// Fetches Claude usage through the pinned upstream Rust API and returns only
/// local owned DTOs. Upstream errors are reduced to a non-sensitive category.
pub async fn fetch_anthropic(
    credentials_path: &Path,
    cache_path: &Path,
    account_id: AccountIdentity,
) -> ProviderUsage {
    let client = reqwest::Client::new();
    let cache = Cache::at(cache_path.to_path_buf());
    let outcome = ai_usagebar::anthropic::fetch::fetch_snapshot(
        &client,
        &ai_usagebar::anthropic::creds::CredsTarget::Default(credentials_path.to_path_buf()),
        &cache,
        &ai_usagebar::anthropic::fetch::Endpoints::default(),
        DEFAULT_TTL,
    )
    .await;
    match outcome {
        Ok(outcome) => adapt_anthropic_snapshot(
            outcome.snapshot,
            account_id,
            outcome.stale,
            outcome.cache_age,
        ),
        Err(error) => ProviderUsage::unavailable(PROVIDER_ANTHROPIC, account_id, map_issue(&error)),
    }
}

pub fn normalize_unix_seconds(value: Option<i64>) -> Option<Timestamp> {
    value.map(Timestamp)
}

pub fn normalize_rfc3339(value: Option<&str>) -> Option<Timestamp> {
    value
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| Timestamp(value.timestamp()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_usagebar::usage::{AnthropicSnapshot, OpenAiCredits, OpenAiSource, ScopedWindow};
    use chrono::{Duration, TimeZone, Utc};

    fn window(used: i32, reset_at: Option<i64>) -> UsageWindow {
        UsageWindow {
            utilization_pct: used,
            resets_at: reset_at.map(|value| Utc.timestamp_opt(value, 0).single().unwrap()),
            window_duration: Duration::hours(5),
        }
    }

    fn openai(session: Option<UsageWindow>, weekly: Option<UsageWindow>) -> OpenAiSnapshot {
        OpenAiSnapshot {
            plan: "ChatGPT Plus".into(),
            session,
            weekly,
            code_review: None,
            credits: None,
            source: OpenAiSource::CodexOauth,
        }
    }

    fn anthropic(session: UsageWindow, weekly: UsageWindow) -> AnthropicSnapshot {
        AnthropicSnapshot {
            plan: "Claude Pro".into(),
            session,
            weekly,
            sonnet: None,
            scoped: Vec::new(),
            extra: None,
        }
    }

    #[test]
    fn openai_used_percent_is_converted_to_remaining() {
        let usage = adapt_openai_snapshot(
            openai(Some(window(5, None)), Some(window(31, None))),
            AccountIdentity::Default,
            false,
            None,
        );
        assert_eq!(usage.summary.remaining_pct, Some(95));
        assert_eq!(
            usage
                .meters
                .iter()
                .find(|meter| meter.id == "weekly")
                .unwrap()
                .remaining_pct,
            Some(69)
        );
    }

    #[test]
    fn remaining_conversion_covers_boundaries_without_underflow() {
        assert_eq!(remaining_from_used(0), Some(100));
        assert_eq!(remaining_from_used(5), Some(95));
        assert_eq!(remaining_from_used(31), Some(69));
        assert_eq!(remaining_from_used(100), Some(0));
        assert_eq!(remaining_from_used(-1), None);
        assert_eq!(remaining_from_used(101), None);
    }

    #[test]
    fn anthropic_utilization_is_converted_to_remaining() {
        let usage = adapt_anthropic_snapshot(
            anthropic(window(38, None), window(34, None)),
            AccountIdentity::Default,
            false,
            None,
        );
        assert_eq!(usage.summary.remaining_pct, Some(62));
        assert_eq!(
            usage
                .meters
                .iter()
                .find(|meter| meter.id == "weekly")
                .unwrap()
                .remaining_pct,
            Some(66)
        );
    }

    #[test]
    fn invalid_percentages_do_not_invent_remaining() {
        let usage = adapt_openai_snapshot(
            openai(Some(window(101, None)), Some(window(-1, None))),
            AccountIdentity::Default,
            false,
            None,
        );
        assert_eq!(usage.summary.remaining_pct, None);
        assert!(usage
            .meters
            .iter()
            .all(|meter| meter.remaining_pct.is_none()));
    }

    #[test]
    fn non_percentage_values_remain_semantically_typed() {
        let mut snapshot = openai(Some(window(5, None)), None);
        snapshot.credits = Some(OpenAiCredits {
            balance: "$12.00".into(),
            has_credits: true,
            unlimited: false,
            approx_local_messages: Some((1, 4)),
            approx_cloud_messages: Some((2, 5)),
        });
        let usage = adapt_openai_snapshot(snapshot, AccountIdentity::Default, false, None);
        assert!(matches!(
            usage
                .meters
                .iter()
                .find(|meter| meter.id == "credits-balance")
                .unwrap()
                .value,
            Some(UsageValue::Amount { .. })
        ));
        assert!(matches!(
            &usage
                .meters
                .iter()
                .find(|meter| meter.id == "credits-local-messages")
                .unwrap()
                .value,
            Some(UsageValue::Text { value, unit })
                if value == "1-4" && unit.as_deref() == Some("approximate range")
        ));
        let text = UsageValue::Text {
            value: "provider note".into(),
            unit: None,
        };
        assert!(matches!(text, UsageValue::Text { .. }));
        let count = UsageValue::Count {
            value: 3,
            unit: Some("messages".into()),
        };
        assert!(matches!(count, UsageValue::Count { value: 3, .. }));
        assert!(usage.summary.remaining_pct.is_some());
    }

    #[test]
    fn missing_reset_stays_none() {
        let usage = adapt_openai_snapshot(
            openai(Some(window(5, None)), None),
            AccountIdentity::Default,
            false,
            None,
        );
        assert_eq!(usage.meters[0].reset_at, None);
    }

    #[test]
    fn reset_timestamps_normalize_without_timezone_guessing() {
        assert_eq!(
            normalize_unix_seconds(Some(1_700_000_000)),
            Some(Timestamp(1_700_000_000))
        );
        assert_eq!(normalize_unix_seconds(None), None);
        assert_eq!(
            normalize_rfc3339(Some("2026-05-23T14:30:00-03:00")),
            Some(Timestamp(1_779_557_400))
        );
        assert_eq!(
            normalize_rfc3339(Some("2026-05-23T17:30:00Z")),
            Some(Timestamp(1_779_557_400))
        );
        assert_eq!(normalize_rfc3339(Some("not-a-timestamp")), None);
        assert_eq!(normalize_rfc3339(None), None);
    }

    #[test]
    fn fresh_and_stale_statuses_are_preserved() {
        let fresh = adapt_openai_snapshot(
            openai(Some(window(5, None)), None),
            AccountIdentity::Default,
            false,
            None,
        );
        let stale = adapt_openai_snapshot(
            openai(Some(window(5, None)), None),
            AccountIdentity::Default,
            true,
            Some(std::time::Duration::from_secs(42)),
        );
        assert_eq!(fresh.status, UsageStatus::Fresh);
        assert_eq!(stale.status, UsageStatus::Stale);
        assert_eq!(stale.summary.remaining_pct, Some(95));
        assert_eq!(stale.cache_age_secs, Some(42));
    }

    #[test]
    fn unavailable_result_has_no_meters_or_percentage() {
        let usage =
            ProviderUsage::unavailable("openai", AccountIdentity::Unknown, FetchIssue::Credentials);
        assert_eq!(usage.status, UsageStatus::Unavailable);
        assert!(usage.meters.is_empty());
        assert_eq!(usage.summary.remaining_pct, None);
        assert_eq!(usage.account_id, AccountIdentity::Unknown);
    }

    #[test]
    fn meter_order_and_summary_selection_are_deterministic() {
        let mut snapshot = anthropic(window(38, None), window(34, None));
        snapshot.scoped.push(ScopedWindow {
            label: "Fable".into(),
            window: window(10, None),
        });
        snapshot.scoped.push(ScopedWindow {
            label: "Opus".into(),
            window: window(20, None),
        });
        let usage =
            adapt_anthropic_snapshot(snapshot, AccountIdentity::Named("work".into()), false, None);
        let ids = usage
            .meters
            .iter()
            .map(|meter| meter.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["scoped-0", "scoped-1", "session", "weekly"]);
        assert_eq!(usage.summary.primary_meter_id.as_deref(), Some("session"));
        assert_eq!(usage.account_id, AccountIdentity::Named("work".into()));
    }

    #[test]
    fn public_boundary_contains_only_local_dtos() {
        fn assert_local(_: ProviderUsage) {}
        let value = ProviderUsage::unavailable("test", AccountIdentity::Unknown, FetchIssue::Other);
        assert_local(value);
    }

    #[test]
    fn openai_weekly_only_uses_weekly_summary() {
        let usage = adapt_openai_snapshot(
            openai(None, Some(window(31, None))),
            AccountIdentity::Default,
            false,
            None,
        );
        assert_eq!(usage.summary.primary_meter_id.as_deref(), Some("weekly"));
        assert_eq!(usage.summary.remaining_pct, Some(69));
        assert!(usage.meters.iter().all(|meter| meter.id != "session"));
    }

    #[test]
    fn anthropic_summary_falls_back_to_weekly_when_session_is_unusable() {
        let usage = adapt_anthropic_snapshot(
            anthropic(window(101, None), window(35, None)),
            AccountIdentity::Default,
            false,
            None,
        );
        assert_eq!(usage.summary.primary_meter_id.as_deref(), Some("weekly"));
        assert_eq!(usage.summary.remaining_pct, Some(65));
        assert_eq!(
            usage
                .meters
                .iter()
                .find(|meter| meter.id == "session")
                .unwrap()
                .remaining_pct,
            None
        );
    }
}
