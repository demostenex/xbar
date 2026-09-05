//! Event-driven discovery of supported AI-agent processes on Linux.
//!
//! Discovery deliberately has no quota, cache, UI, or xbar integration.  The
//! [`ActiveAgentSet`] state machine is platform independent; only `procfs` and
//! `cn_proc` perform operating-system I/O.

mod classifier;
mod cn_proc;
mod procfs;

use std::collections::BTreeMap;
use std::fmt;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;

use crate::AccountIdentity;

pub use classifier::classify;
pub(crate) use classifier::AccountScopeResolution;
pub use cn_proc::{CnProc, CnProcError};
pub use procfs::{startup_snapshot, ProcFsError};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AgentKind {
    Codex,
    ClaudeCode,
    Antigravity,
    Grok,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    Google,
    Xai,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum UsageSourceId {
    OpenAi,
    Anthropic,
    Antigravity,
    Grok,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountScopeRule {
    Environment(&'static str),
    Default,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessMatch {
    Codex,
    ClaudeCode,
    Antigravity,
    Grok,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentDescriptor {
    pub agent: AgentKind,
    pub provider: ProviderKind,
    pub display_name: &'static str,
    pub process_match: ProcessMatch,
    pub usage_source: Option<UsageSourceId>,
    pub account_scope_rule: AccountScopeRule,
}

pub const AGENT_REGISTRY: &[AgentDescriptor] = &[
    AgentDescriptor {
        agent: AgentKind::Codex,
        provider: ProviderKind::OpenAi,
        display_name: "Codex",
        process_match: ProcessMatch::Codex,
        usage_source: Some(UsageSourceId::OpenAi),
        account_scope_rule: AccountScopeRule::Environment("CODEX_HOME"),
    },
    AgentDescriptor {
        agent: AgentKind::ClaudeCode,
        provider: ProviderKind::Anthropic,
        display_name: "Claude",
        process_match: ProcessMatch::ClaudeCode,
        usage_source: Some(UsageSourceId::Anthropic),
        account_scope_rule: AccountScopeRule::Environment("CLAUDE_CONFIG_DIR"),
    },
    AgentDescriptor {
        agent: AgentKind::Antigravity,
        provider: ProviderKind::Google,
        display_name: "Antigravity",
        process_match: ProcessMatch::Antigravity,
        usage_source: Some(UsageSourceId::Antigravity),
        account_scope_rule: AccountScopeRule::Default,
    },
    AgentDescriptor {
        agent: AgentKind::Grok,
        provider: ProviderKind::Xai,
        display_name: "Grok",
        process_match: ProcessMatch::Grok,
        usage_source: None,
        account_scope_rule: AccountScopeRule::Default,
    },
];

pub fn agent_registry() -> &'static [AgentDescriptor] {
    AGENT_REGISTRY
}

pub fn agent_descriptor(agent: AgentKind) -> &'static AgentDescriptor {
    AGENT_REGISTRY
        .iter()
        .find(|descriptor| descriptor.agent == agent)
        .expect("every AgentKind has a registry descriptor")
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub starttime: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentInstance {
    pub process: ProcessIdentity,
    pub agent: AgentKind,
    pub provider: ProviderKind,
    pub usage_source: Option<UsageSourceId>,
    pub account_scope: AccountIdentity,
    pub(crate) account_scope_resolution: classifier::AccountScopeResolution,
    pub executable: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessRecord {
    pub process: ProcessIdentity,
    pub executable: PathBuf,
    pub comm: String,
    /// Contains only the one scope variable relevant to a positively
    /// classified process; it is never logged by this crate.
    pub environment: Option<Vec<(String, String)>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryEvent {
    AgentStarted(AgentInstance),
    AgentExited(ProcessIdentity),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ActiveAgentSet {
    agents: BTreeMap<ProcessIdentity, AgentInstance>,
}

impl ActiveAgentSet {
    pub fn instances(&self) -> impl Iterator<Item = &AgentInstance> {
        self.agents.values()
    }

    pub fn get(&self, identity: ProcessIdentity) -> Option<&AgentInstance> {
        self.agents.get(&identity)
    }

    pub fn apply_record(&mut self, record: ProcessRecord) -> Vec<DiscoveryEvent> {
        let identity = record.process;
        let mut events = self
            .agents
            .keys()
            .copied()
            .filter(|old| old.pid == identity.pid && *old != identity)
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|old| self.remove_identity(old))
            .collect::<Vec<_>>();
        let Some(instance) = classify(record) else {
            events.extend(self.remove_identity(identity));
            return events;
        };
        match self.agents.get(&instance.process) {
            Some(old)
                if old.agent == instance.agent
                    && old.provider == instance.provider
                    && old.account_scope == instance.account_scope =>
            {
                events
            }
            Some(_) => {
                events.extend(self.remove_identity(instance.process));
                self.agents.insert(instance.process, instance.clone());
                events.push(DiscoveryEvent::AgentStarted(instance));
                events
            }
            None => {
                self.agents.insert(instance.process, instance.clone());
                events.push(DiscoveryEvent::AgentStarted(instance));
                events
            }
        }
    }

    pub fn apply_exit(&mut self, identity: ProcessIdentity) -> Vec<DiscoveryEvent> {
        self.remove_identity(identity)
    }

    pub fn reconcile(
        &mut self,
        records: impl IntoIterator<Item = ProcessRecord>,
    ) -> Vec<DiscoveryEvent> {
        let mut next = BTreeMap::new();
        let mut events = Vec::new();
        for record in records {
            if let Some(instance) = classify(record) {
                next.insert(instance.process, instance);
            }
        }
        for identity in self
            .agents
            .keys()
            .copied()
            .filter(|id| !next.contains_key(id))
            .collect::<Vec<_>>()
        {
            events.push(DiscoveryEvent::AgentExited(identity));
        }
        for (identity, instance) in &next {
            match self.agents.get(identity) {
                None => events.push(DiscoveryEvent::AgentStarted(instance.clone())),
                Some(old)
                    if old.agent != instance.agent
                        || old.provider != instance.provider
                        || old.account_scope != instance.account_scope =>
                {
                    events.push(DiscoveryEvent::AgentExited(*identity));
                    events.push(DiscoveryEvent::AgentStarted(instance.clone()));
                }
                Some(_) => {}
            }
        }
        self.agents = next;
        events
    }

    fn remove_identity(&mut self, identity: ProcessIdentity) -> Vec<DiscoveryEvent> {
        self.agents
            .remove(&identity)
            .map(|_| vec![DiscoveryEvent::AgentExited(identity)])
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pid: u32, starttime: u64, path: &str, comm: &str) -> ProcessRecord {
        ProcessRecord {
            process: ProcessIdentity { pid, starttime },
            executable: path.into(),
            comm: comm.into(),
            environment: None,
        }
    }

    const CODEX: &str = "/x/@openai/codex/node_modules/@openai/codex-linux-x64/vendor/a/bin/codex";
    const CLAUDE: &str = "/home/u/.local/share/claude/versions/2.9.1";

    #[test]
    fn descendants_never_become_agents() {
        let mut set = ActiveAgentSet::default();
        for comm in [
            "printf", "sh", "git", "bash", "zsh", "sleep", "node", "python", "cargo", "rg", "sed",
            "cat",
        ] {
            assert!(set
                .apply_record(record(10, 1, "/usr/bin/anything", comm))
                .is_empty());
        }
        assert_eq!(set.instances().count(), 0);
    }

    #[test]
    fn duplicate_exec_and_pid_reuse_are_identity_safe() {
        let mut set = ActiveAgentSet::default();
        assert_eq!(set.apply_record(record(7, 10, CODEX, "codex")).len(), 1);
        assert!(set.apply_record(record(7, 10, CODEX, "codex")).is_empty());
        assert_eq!(set.apply_record(record(7, 11, CODEX, "codex")).len(), 2);
        assert_eq!(set.instances().count(), 1);
    }

    #[test]
    fn tracked_exit_is_exact_and_arbitrary_exit_is_ignored() {
        let mut set = ActiveAgentSet::default();
        set.apply_record(record(7, 10, CODEX, "codex"));
        assert_eq!(
            set.apply_exit(ProcessIdentity {
                pid: 99,
                starttime: 10
            }),
            Vec::new()
        );
        assert_eq!(
            set.apply_exit(ProcessIdentity {
                pid: 7,
                starttime: 10
            })
            .len(),
            1
        );
        assert_eq!(
            set.apply_exit(ProcessIdentity {
                pid: 7,
                starttime: 10
            }),
            Vec::new()
        );
    }

    #[test]
    fn reconciliation_adds_removes_and_preserves() {
        let mut set = ActiveAgentSet::default();
        set.apply_record(record(1, 1, CODEX, "codex"));
        let events = set.reconcile([record(1, 1, CODEX, "codex"), record(2, 2, CLAUDE, "claude")]);
        assert_eq!(
            events,
            vec![DiscoveryEvent::AgentStarted(AgentInstance {
                process: ProcessIdentity {
                    pid: 2,
                    starttime: 2
                },
                agent: AgentKind::ClaudeCode,
                provider: ProviderKind::Anthropic,
                usage_source: Some(UsageSourceId::Anthropic),
                account_scope: AccountIdentity::Default,
                account_scope_resolution: AccountScopeResolution::DefaultVariableAbsent,
                executable: CLAUDE.into()
            })]
        );
        let events = set.reconcile([record(2, 2, CLAUDE, "claude")]);
        assert_eq!(
            events,
            vec![DiscoveryEvent::AgentExited(ProcessIdentity {
                pid: 1,
                starttime: 1
            })]
        );
    }

    #[test]
    fn two_same_account_instances_remain_distinct() {
        let mut set = ActiveAgentSet::default();
        set.reconcile([record(1, 1, CODEX, "codex"), record(2, 2, CODEX, "codex")]);
        assert_eq!(set.instances().count(), 2);
    }

    #[test]
    fn exec_away_removes_agent_and_agent_swap_is_deterministic() {
        let mut set = ActiveAgentSet::default();
        set.apply_record(record(3, 3, CODEX, "codex"));
        assert_eq!(
            set.apply_record(record(3, 3, "/usr/bin/git", "git")),
            vec![DiscoveryEvent::AgentExited(ProcessIdentity {
                pid: 3,
                starttime: 3
            })]
        );
        set.apply_record(record(3, 3, CODEX, "codex"));
        let events = set.apply_record(record(3, 3, CLAUDE, "claude"));
        assert!(
            matches!(events.as_slice(), [DiscoveryEvent::AgentExited(_), DiscoveryEvent::AgentStarted(instance)] if instance.agent == AgentKind::ClaudeCode)
        );
    }

    #[test]
    fn synthetic_initial_snapshot_establishes_existing_agents_once() {
        let mut set = ActiveAgentSet::default();
        let events = set.reconcile([
            record(11, 1, CODEX, "codex"),
            record(12, 2, CLAUDE, "claude"),
        ]);
        assert_eq!(events.len(), 2);
        assert_eq!(set.instances().count(), 2);
        assert!(set
            .reconcile([
                record(11, 1, CODEX, "codex"),
                record(12, 2, CLAUDE, "claude")
            ])
            .is_empty());
    }
}

#[derive(Debug)]
pub enum DiscoveryError {
    ProcFs(ProcFsError),
    CnProc(CnProcError),
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProcFs(e) => write!(f, "procfs: {e}"),
            Self::CnProc(e) => write!(f, "CN_PROC: {e}"),
        }
    }
}

impl std::error::Error for DiscoveryError {}

pub struct Discovery {
    pub agents: ActiveAgentSet,
    cn_proc: CnProc,
}

impl Discovery {
    pub fn start() -> Result<(Self, Vec<DiscoveryEvent>), DiscoveryError> {
        let cn_proc = CnProc::listen().map_err(DiscoveryError::CnProc)?;
        let records = startup_snapshot().map_err(DiscoveryError::ProcFs)?;
        let mut agents = ActiveAgentSet::default();
        let events = agents.reconcile(records);
        Ok((Self { agents, cn_proc }, events))
    }

    pub fn fd(&self) -> RawFd {
        self.cn_proc.fd()
    }

    pub fn next_event(&mut self) -> Result<Vec<DiscoveryEvent>, DiscoveryError> {
        match self.cn_proc.next_event() {
            Ok(cn_proc::KernelEvent::Exec(pid)) => Ok(procfs::inspect(pid)
                .map(|record| self.agents.apply_record(record))
                .unwrap_or_default()),
            Ok(cn_proc::KernelEvent::Exit(pid)) => {
                let identity = self
                    .agents
                    .instances()
                    .find(|instance| instance.process.pid == pid)
                    .map(|instance| instance.process);
                Ok(identity
                    .map(|identity| self.agents.apply_exit(identity))
                    .unwrap_or_default())
            }
            Err(CnProcError::Lost) => {
                let records = startup_snapshot().map_err(DiscoveryError::ProcFs)?;
                Ok(self.agents.reconcile(records))
            }
            Err(error) => Err(DiscoveryError::CnProc(error)),
        }
    }
}

impl AsRawFd for Discovery {
    fn as_raw_fd(&self) -> RawFd {
        self.fd()
    }
}
