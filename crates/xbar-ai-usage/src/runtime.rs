//! Standalone orchestration for discovery and canonical quota refresh.
//!
//! The runtime owns scheduling and the accepted ProviderUsage snapshot. It
//! delegates semantic composition to [`crate::CollectorModel`] and delegates
//! provider semantics to the existing fetch adapters.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;

use crate::discovery::{AccountScopeResolution, CnProcError, DiscoveryEvent};
use crate::{
    fetch_anthropic, fetch_openai, AccountIdentity, ActiveAgentUsage, CollectorModel, Discovery,
    DiscoveryError, ProviderKind, ProviderUsage,
};

pub const DEFAULT_USAGE_REFRESH: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub openai_credentials_path: PathBuf,
    pub openai_cache_path: PathBuf,
    pub anthropic_credentials_path: PathBuf,
    pub anthropic_cache_path: PathBuf,
    pub usage_refresh: Duration,
    pub debug: bool,
}

impl RuntimeConfig {
    pub fn from_environment(debug: bool) -> Result<Self, RuntimeError> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| RuntimeError::Configuration("HOME is required".into()))?;
        Ok(Self {
            openai_credentials_path: home.join(".codex/auth.json"),
            openai_cache_path: home.join(".cache/xbar-ai-usage/openai"),
            anthropic_credentials_path: home.join(".claude/.credentials.json"),
            anthropic_cache_path: home.join(".cache/xbar-ai-usage/anthropic"),
            usage_refresh: DEFAULT_USAGE_REFRESH,
            debug,
        })
    }
}

#[derive(Debug)]
pub enum RuntimeError {
    Configuration(String),
    Discovery(DiscoveryError),
    Io(io::Error),
    TaskJoin(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(error) => write!(f, "configuration: {error}"),
            Self::Discovery(error) => write!(f, "discovery: {error}"),
            Self::Io(error) => write!(f, "runtime I/O: {error}"),
            Self::TaskJoin(error) => write!(f, "refresh task: {error}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RefreshKey {
    provider: ProviderKind,
    account: AccountIdentity,
}

#[derive(Clone, Debug, Default)]
struct RefreshScheduler {
    relevant: BTreeSet<RefreshKey>,
    in_flight: BTreeSet<RefreshKey>,
    pending: BTreeSet<RefreshKey>,
    stopped: bool,
}

impl RefreshScheduler {
    fn reconcile_relevance(&mut self, relevant: impl IntoIterator<Item = RefreshKey>) {
        self.relevant = relevant.into_iter().collect();
        self.pending.retain(|key| self.relevant.contains(key));
    }

    fn demand(&mut self, key: RefreshKey) -> bool {
        if self.stopped {
            return false;
        }
        if !self.relevant.contains(&key) {
            return false;
        }
        if self.in_flight.contains(&key) {
            self.pending.insert(key);
            false
        } else {
            self.in_flight.insert(key);
            true
        }
    }

    fn complete(&mut self, key: &RefreshKey) -> bool {
        self.in_flight.remove(key);
        if !self.stopped && self.pending.remove(key) && self.relevant.contains(key) {
            self.in_flight.insert(key.clone());
            true
        } else {
            false
        }
    }

    fn shutdown(&mut self) {
        self.stopped = true;
        self.pending.clear();
        self.relevant.clear();
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ProviderUsageKey {
    provider_id: String,
    account_id: AccountIdentity,
}

#[derive(Clone, Debug, Default)]
struct QuotaSnapshot {
    values: BTreeMap<ProviderUsageKey, ProviderUsage>,
}

impl QuotaSnapshot {
    fn update(&mut self, usage: ProviderUsage) {
        self.values.insert(provider_usage_key(&usage), usage);
    }

    fn records(&self) -> impl Iterator<Item = ProviderUsage> + '_ {
        self.values.values().cloned()
    }
}

struct RefreshResult {
    key: RefreshKey,
    usage: ProviderUsage,
}

pub async fn run(config: RuntimeConfig) -> Result<(), RuntimeError> {
    let (discovery, initial_events) = Discovery::start().map_err(RuntimeError::Discovery)?;
    set_nonblocking(discovery.as_raw_fd()).map_err(RuntimeError::Io)?;
    let mut discovery = AsyncFd::new(discovery).map_err(RuntimeError::Io)?;
    let mut model = CollectorModel::new();
    for event in initial_events {
        debug_discovery(&config, &event);
        model.apply_discovery_event(event);
    }

    let mut provider_usage = QuotaSnapshot::default();
    let mut canonical = model.active_usage();
    emit_state(&config, "STARTUP", &canonical);

    let (result_tx, mut result_rx) = mpsc::unbounded_channel();
    let mut scheduler = RefreshScheduler::default();
    let initial_scopes = refresh_scopes(&canonical);
    scheduler.reconcile_relevance(initial_scopes.iter().cloned());
    for key in initial_scopes {
        if scheduler.demand(key.clone()) {
            spawn_refresh(&config, key, result_tx.clone());
        }
    }

    let mut next_refresh = Instant::now() + config.usage_refresh;
    let mut shutdown = std::pin::pin!(tokio::signal::ctrl_c());

    loop {
        let wait = tokio::time::sleep_until(tokio::time::Instant::from_std(next_refresh));
        tokio::pin!(wait);
        tokio::select! {
            signal = &mut shutdown => {
                signal.map_err(RuntimeError::Io)?;
                scheduler.shutdown();
                return Ok(());
            }
            result = result_rx.recv() => {
                let Some(result) = result else { return Err(RuntimeError::TaskJoin("refresh channel closed".into())); };
                provider_usage.update(result.usage);
                model.replace_provider_usage(provider_usage.records());
                let updated = model.active_usage();
                if updated != canonical {
                    canonical = updated;
                    emit_state(&config, "STATE_CHANGED", &canonical);
                }
                if scheduler.complete(&result.key) {
                    spawn_refresh(&config, result.key, result_tx.clone());
                }
            }
            _ = &mut wait => {
                let relevant = refresh_scopes(&canonical);
                scheduler.reconcile_relevance(relevant.iter().cloned());
                for key in relevant {
                    if scheduler.demand(key.clone()) {
                        spawn_refresh(&config, key, result_tx.clone());
                    }
                }
                next_refresh = Instant::now() + config.usage_refresh;
            }
            readiness = discovery.readable_mut() => {
                let mut guard = readiness.map_err(RuntimeError::Io)?;
                let mut events = Vec::new();
                loop {
                    let mut discovery_error = None;
                    let result = guard.try_io(|inner| {
                        match inner.get_mut().next_event() {
                            Ok(events) => Ok(events),
                            Err(DiscoveryError::CnProc(CnProcError::Io(error)))
                                if error.kind() == io::ErrorKind::WouldBlock => Err(error),
                            Err(error) => {
                                discovery_error = Some(error);
                                Err(io::Error::other("CN_PROC event processing failed"))
                            }
                        }
                    });
                    match result {
                        Ok(Ok(batch)) => events.extend(batch),
                        Ok(Err(error)) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Ok(Err(error)) => {
                            if let Some(error) = discovery_error {
                                return Err(RuntimeError::Discovery(error));
                            }
                            return Err(RuntimeError::Io(error));
                        }
                        Err(_would_block) => break,
                    }
                    if let Some(error) = discovery_error {
                        return Err(RuntimeError::Discovery(error));
                    }
                }
                for event in events {
                    let before = refresh_scopes(&canonical);
                    debug_discovery(&config, &event);
                    model.apply_discovery_event(event);
                    let updated = model.active_usage();
                    let after = refresh_scopes(&updated);
                    if updated != canonical {
                        canonical = updated;
                        emit_state(&config, "STATE_CHANGED", &canonical);
                    }
                    scheduler.reconcile_relevance(after.iter().cloned());
                    for key in after.into_iter().filter(|key| !before.contains(key)) {
                        if scheduler.demand(key.clone()) {
                            spawn_refresh(&config, key, result_tx.clone());
                        }
                    }
                }
            }
        }
    }
}

fn spawn_refresh(
    config: &RuntimeConfig,
    key: RefreshKey,
    tx: mpsc::UnboundedSender<RefreshResult>,
) {
    let config = config.clone();
    tokio::spawn(async move {
        debug_refresh(&config, "REFRESH_STARTED", &key);
        let usage = match key.provider {
            ProviderKind::OpenAi => {
                fetch_openai(
                    &config.openai_credentials_path,
                    &config.openai_cache_path,
                    key.account.clone(),
                )
                .await
            }
            ProviderKind::Anthropic => {
                fetch_anthropic(
                    &config.anthropic_credentials_path,
                    &config.anthropic_cache_path,
                    key.account.clone(),
                )
                .await
            }
        };
        debug_refresh(&config, "REFRESH_FINISHED", &key);
        let _ = tx.send(RefreshResult { key, usage });
    });
}

fn refresh_scopes(usage: &[ActiveAgentUsage]) -> BTreeSet<RefreshKey> {
    usage
        .iter()
        .filter_map(|entry| {
            if entry.account_id != AccountIdentity::Default {
                return None;
            }
            let provider = match entry.provider_id.as_str() {
                "openai" => ProviderKind::OpenAi,
                "anthropic" => ProviderKind::Anthropic,
                _ => return None,
            };
            Some(RefreshKey {
                provider,
                account: AccountIdentity::Default,
            })
        })
        .collect()
}

fn provider_usage_key(usage: &ProviderUsage) -> ProviderUsageKey {
    ProviderUsageKey {
        provider_id: usage.provider_id.clone(),
        account_id: usage.account_id.clone(),
    }
}

fn debug_discovery(config: &RuntimeConfig, event: &DiscoveryEvent) {
    if !config.debug {
        return;
    }
    match event {
        DiscoveryEvent::AgentStarted(instance) => {
            println!(
                "AGENT_STARTED agent={:?} pid={} starttime={}",
                instance.agent, instance.process.pid, instance.process.starttime
            );
            match &instance.account_scope_resolution {
                AccountScopeResolution::EnvironmentUnreadable { errno } => println!(
                    "ACCOUNT_SCOPE agent={:?} pid={} result={:?} reason=EnvironmentUnreadable errno={:?}",
                    instance.agent, instance.process.pid, instance.account_scope, errno
                ),
                reason => println!(
                    "ACCOUNT_SCOPE agent={:?} pid={} result={:?} reason={reason:?}",
                    instance.agent, instance.process.pid, instance.account_scope
                ),
            }
        }
        DiscoveryEvent::AgentExited(identity) => println!(
            "AGENT_EXITED pid={} starttime={}",
            identity.pid, identity.starttime
        ),
    }
}

fn debug_refresh(config: &RuntimeConfig, event: &str, key: &RefreshKey) {
    if config.debug {
        println!(
            "{event} provider={} account=default",
            provider_id(key.provider)
        );
    }
}

fn emit_state(config: &RuntimeConfig, event: &str, usage: &[ActiveAgentUsage]) {
    if !config.debug {
        return;
    }
    println!("{event} groups={}", usage.len());
    for entry in usage {
        println!(
            "AGENT_USAGE provider={} agent={} account={} active_instances={} status={:?} remaining_pct={:?}",
            entry.provider_id,
            entry.agent_id,
            account_label(&entry.account_id),
            entry.active_instances,
            entry.status,
            entry.summary.remaining_pct
        );
    }
}

fn account_label(account: &AccountIdentity) -> &'static str {
    match account {
        AccountIdentity::Default => "default",
        AccountIdentity::Named(_) => "named",
        AccountIdentity::Unknown => "unknown",
    }
}

fn provider_id(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::OpenAi => "openai",
        ProviderKind::Anthropic => "anthropic",
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AgentInstance, AgentKind, DiscoveryEvent, FetchIssue, ProcessIdentity, UsageStatus,
        UsageSummary,
    };

    fn key(provider: ProviderKind) -> RefreshKey {
        RefreshKey {
            provider,
            account: AccountIdentity::Default,
        }
    }

    fn instance(pid: u32) -> AgentInstance {
        AgentInstance {
            process: ProcessIdentity { pid, starttime: 42 },
            agent: AgentKind::Codex,
            provider: ProviderKind::OpenAi,
            account_scope: AccountIdentity::Default,
            account_scope_resolution: AccountScopeResolution::DefaultVariableAbsent,
            executable: "/usr/bin/codex".into(),
        }
    }

    fn usage(provider_id: &str, account_id: AccountIdentity, status: UsageStatus) -> ProviderUsage {
        ProviderUsage {
            provider_id: provider_id.into(),
            display_name: provider_id.into(),
            account_id,
            meters: Vec::new(),
            summary: UsageSummary {
                primary_meter_id: None,
                remaining_pct: None,
            },
            status,
            fetched_at: None,
            cache_age_secs: None,
            issue: Some(FetchIssue::Other),
        }
    }

    #[test]
    fn no_active_scopes_create_no_refresh_demand() {
        let mut scheduler = RefreshScheduler::default();
        scheduler.reconcile_relevance([]);
        assert!(!scheduler.demand(key(ProviderKind::OpenAi)));
    }

    #[test]
    fn first_scope_starts_one_in_flight_refresh() {
        let mut scheduler = RefreshScheduler::default();
        let openai = key(ProviderKind::OpenAi);
        scheduler.reconcile_relevance([openai.clone()]);
        assert!(scheduler.demand(openai));
    }

    #[test]
    fn duplicate_same_scope_demand_is_coalesced() {
        let mut scheduler = RefreshScheduler::default();
        let openai = key(ProviderKind::OpenAi);
        scheduler.reconcile_relevance([openai.clone()]);
        assert!(scheduler.demand(openai.clone()));
        assert!(!scheduler.demand(openai));
    }

    #[test]
    fn demand_during_flight_allows_one_follow_up() {
        let mut scheduler = RefreshScheduler::default();
        let openai = key(ProviderKind::OpenAi);
        scheduler.reconcile_relevance([openai.clone()]);
        assert!(scheduler.demand(openai.clone()));
        assert!(!scheduler.demand(openai.clone()));
        assert!(scheduler.complete(&openai));
        assert!(!scheduler.complete(&openai));
    }

    #[test]
    fn irrelevant_scope_is_not_refreshed_after_exit() {
        let mut scheduler = RefreshScheduler::default();
        let openai = key(ProviderKind::OpenAi);
        scheduler.reconcile_relevance([openai.clone()]);
        assert!(scheduler.demand(openai.clone()));
        scheduler.reconcile_relevance([]);
        assert!(!scheduler.complete(&openai));
    }

    #[test]
    fn shutdown_prevents_new_refresh_demand() {
        let mut scheduler = RefreshScheduler::default();
        let openai = key(ProviderKind::OpenAi);
        scheduler.reconcile_relevance([openai.clone()]);
        scheduler.shutdown();
        assert!(!scheduler.demand(openai));
    }

    #[test]
    fn provider_scopes_are_independent() {
        let mut scheduler = RefreshScheduler::default();
        let openai = key(ProviderKind::OpenAi);
        let anthropic = key(ProviderKind::Anthropic);
        scheduler.reconcile_relevance([openai.clone(), anthropic.clone()]);
        assert!(scheduler.demand(openai));
        assert!(scheduler.demand(anthropic));
    }

    #[test]
    fn quota_snapshot_update_preserves_other_provider() {
        let mut snapshot = QuotaSnapshot::default();
        snapshot.update(usage(
            "openai",
            AccountIdentity::Default,
            UsageStatus::Fresh,
        ));
        snapshot.update(usage(
            "anthropic",
            AccountIdentity::Default,
            UsageStatus::Fresh,
        ));
        snapshot.update(usage(
            "openai",
            AccountIdentity::Default,
            UsageStatus::Stale,
        ));
        let records = snapshot.records().collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records
                .iter()
                .find(|record| record.provider_id == "openai")
                .map(|record| &record.status),
            Some(&UsageStatus::Stale)
        );
        assert!(records
            .iter()
            .any(|record| record.provider_id == "anthropic"));
    }

    #[test]
    fn quota_snapshot_fetched_metadata_is_replaced() {
        let mut snapshot = QuotaSnapshot::default();
        let mut first = usage("openai", AccountIdentity::Default, UsageStatus::Fresh);
        first.fetched_at = Some(crate::Timestamp(1));
        first.cache_age_secs = Some(10);
        snapshot.update(first);
        let mut second = usage("openai", AccountIdentity::Default, UsageStatus::Fresh);
        second.fetched_at = Some(crate::Timestamp(2));
        second.cache_age_secs = Some(0);
        snapshot.update(second);
        let record = snapshot.records().next().unwrap();
        assert_eq!(record.fetched_at, Some(crate::Timestamp(2)));
        assert_eq!(record.cache_age_secs, Some(0));
    }

    #[test]
    fn named_and_unknown_accounts_are_not_fetch_targets() {
        let usage = [
            ActiveAgentUsage {
                agent_id: "codex".into(),
                provider_id: "openai".into(),
                account_id: AccountIdentity::Named("work".into()),
                display_name: "Codex".into(),
                active_instances: 1,
                meters: Vec::new(),
                summary: crate::UsageSummary {
                    primary_meter_id: None,
                    remaining_pct: None,
                },
                status: UsageStatus::Unavailable,
                fetched_at: None,
                cache_age_secs: None,
            },
            ActiveAgentUsage {
                agent_id: "codex".into(),
                provider_id: "openai".into(),
                account_id: AccountIdentity::Unknown,
                display_name: "Codex".into(),
                active_instances: 1,
                meters: Vec::new(),
                summary: crate::UsageSummary {
                    primary_meter_id: None,
                    remaining_pct: None,
                },
                status: UsageStatus::Unknown,
                fetched_at: None,
                cache_age_secs: None,
            },
        ];
        assert!(refresh_scopes(&usage).is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quota_fetch_in_flight_does_not_block_discovery() {
        let mut model = CollectorModel::new();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        event_tx
            .send(DiscoveryEvent::AgentStarted(instance(9001)))
            .unwrap();

        let fetch = std::future::pending::<()>();
        tokio::pin!(fetch);
        tokio::select! {
            _ = &mut fetch => panic!("the mock fetch must remain pending"),
            event = event_rx.recv() => model.apply_discovery_event(event.unwrap()),
        }

        let active = model.active_usage();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].agent_id, "codex");
        assert_eq!(active[0].active_instances, 1);
        assert_eq!(active[0].status, UsageStatus::Unavailable);
    }

    #[test]
    fn discovery_readiness_drains_all_available_events() {
        let mut model = CollectorModel::new();
        let events = [
            DiscoveryEvent::AgentStarted(instance(9001)),
            DiscoveryEvent::AgentStarted(instance(9002)),
            DiscoveryEvent::AgentExited(ProcessIdentity {
                pid: 9001,
                starttime: 42,
            }),
        ];

        for event in events {
            model.apply_discovery_event(event);
        }

        let active = model.active_usage();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].active_instances, 1);
    }

    #[test]
    fn rapid_same_scope_agent_starts_do_not_schedule_followup_fetch() {
        let mut model = CollectorModel::new();
        let mut scheduler = RefreshScheduler::default();
        let first = instance(9001);
        let key = key(ProviderKind::OpenAi);
        let mut refreshes_started = 0;

        model.apply_discovery_event(DiscoveryEvent::AgentStarted(first));
        let initial = model.active_usage();
        scheduler.reconcile_relevance(refresh_scopes(&initial));
        assert!(scheduler.demand(key.clone()));
        refreshes_started += 1;

        for pid in [9002, 9003] {
            let before = refresh_scopes(&model.active_usage());
            model.apply_discovery_event(DiscoveryEvent::AgentStarted(instance(pid)));
            let active = model.active_usage();
            let after = refresh_scopes(&active);
            scheduler.reconcile_relevance(after.iter().cloned());
            for scope in after.into_iter().filter(|scope| !before.contains(scope)) {
                assert!(scheduler.demand(scope));
                refreshes_started += 1;
            }
        }

        let active = model.active_usage();
        assert_eq!(active[0].active_instances, 3);
        assert_eq!(refreshes_started, 1);
        assert!(scheduler.in_flight.contains(&key));
        assert!(scheduler.pending.is_empty());
        assert!(!scheduler.complete(&key));
    }
}
