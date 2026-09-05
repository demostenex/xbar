use std::path::Path;

#[cfg(test)]
use super::AgentKind;
use super::{agent_registry, AccountScopeRule, AgentInstance, ProcessMatch, ProcessRecord};
use crate::AccountIdentity;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AccountScopeResolution {
    DefaultVariableAbsent,
    ExplicitScope,
    EnvironmentUnreadable { errno: Option<i32> },
    EnvironmentMalformed,
    DuplicateVariable,
    ScopeCanonicalizationFailed,
}

pub fn classify(record: ProcessRecord) -> Option<AgentInstance> {
    let executable = std::fs::canonicalize(&record.executable).unwrap_or(record.executable);
    let descriptor = classify_descriptor(&executable, &record.comm)?;
    let (account_scope, account_scope_resolution) = match descriptor.account_scope_rule {
        AccountScopeRule::Environment(variable) => {
            account_scope_resolution(record.environment.as_deref(), variable)
        }
        AccountScopeRule::Default => (
            AccountIdentity::Default,
            AccountScopeResolution::DefaultVariableAbsent,
        ),
    };
    Some(AgentInstance {
        process: record.process,
        agent: descriptor.agent,
        provider: descriptor.provider,
        usage_source: descriptor.usage_source,
        account_scope,
        account_scope_resolution,
        executable,
    })
}

fn classify_descriptor(path: &Path, comm: &str) -> Option<&'static super::AgentDescriptor> {
    agent_registry()
        .iter()
        .find(|descriptor| matches_process(descriptor.process_match, path, comm))
}

#[cfg(test)]
fn classify_executable(path: &Path, comm: &str) -> Option<AgentKind> {
    classify_descriptor(path, comm).map(|descriptor| descriptor.agent)
}

fn matches_process(process_match: ProcessMatch, path: &Path, comm: &str) -> bool {
    match process_match {
        ProcessMatch::Codex => is_codex_path(path, comm),
        ProcessMatch::ClaudeCode => comm == "claude" && is_versioned_claude_path(path),
        ProcessMatch::Antigravity => comm == "agy" && is_antigravity_path(path),
        ProcessMatch::Grok => comm == "grok" && is_grok_path(path),
    }
}

fn is_codex_path(path: &Path, comm: &str) -> bool {
    let components: Vec<_> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    path.file_name().and_then(|n| n.to_str()) == Some("codex")
        && comm == "codex"
        && components.windows(2).any(|w| w == ["bin", "codex"])
        && components.contains(&"vendor")
        && components.windows(2).any(|w| w == ["@openai", "codex"])
        && !components.contains(&"codex-linux-sandbox")
}

fn is_antigravity_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    let components: Vec<_> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    let in_local_bin = components
        .windows(2)
        .any(|window| window == [".local", "bin"]);
    let atomic_update = name
        .strip_prefix("agy.")
        .and_then(|value| value.strip_suffix(".old"))
        .is_some_and(|value| !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit()));
    in_local_bin && (name == "agy" || atomic_update)
}

fn is_grok_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    let components: Vec<_> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    let in_downloads = components
        .windows(2)
        .any(|window| window == [".grok", "downloads"]);
    let versioned_binary = name
        .strip_prefix("grok-")
        .and_then(|value| value.strip_suffix("-linux-x86_64"))
        .is_some_and(|value| {
            !value.is_empty()
                && value.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
                && value.chars().next().is_some_and(|ch| ch.is_ascii_digit())
                && value.chars().last().is_some_and(|ch| ch.is_ascii_digit())
        });
    in_downloads && versioned_binary
}

fn is_versioned_claude_path(path: &Path) -> bool {
    let components: Vec<_> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    let Some(index) = components
        .windows(2)
        .position(|w| w == ["claude", "versions"])
    else {
        return false;
    };
    index >= 2
        && components[index - 2..index + 1] == [".local", "share", "claude"]
        && components.len() == index + 3
        && !components[index + 2].is_empty()
        && components[index + 2] != "."
        && components[index + 2] != ".."
}

#[cfg(test)]
fn account_scope(environment: Option<&[(String, String)]>, variable: &str) -> AccountIdentity {
    account_scope_resolution(environment, variable).0
}

pub(crate) fn account_scope_resolution(
    environment: Option<&[(String, String)]>,
    variable: &str,
) -> (AccountIdentity, AccountScopeResolution) {
    let Some(environment) = environment else {
        return (
            AccountIdentity::Default,
            AccountScopeResolution::DefaultVariableAbsent,
        );
    };
    if let Some((_, detail)) = environment
        .iter()
        .find(|(name, _)| name == "__XBAR_DISCOVERY_ENV_UNREADABLE")
    {
        let resolution = if detail == "malformed" {
            AccountScopeResolution::EnvironmentMalformed
        } else {
            AccountScopeResolution::EnvironmentUnreadable {
                errno: detail
                    .strip_prefix("errno:")
                    .and_then(|value| value.parse().ok()),
            }
        };
        return (AccountIdentity::Unknown, resolution);
    }
    let values: Vec<_> = environment
        .iter()
        .filter(|(name, _)| name == variable)
        .map(|(_, value)| value)
        .collect();
    match values.as_slice() {
        [] => (
            AccountIdentity::Default,
            AccountScopeResolution::DefaultVariableAbsent,
        ),
        [value] if value.is_empty() => (
            AccountIdentity::Unknown,
            AccountScopeResolution::EnvironmentMalformed,
        ),
        [value] => match std::fs::canonicalize(value)
            .ok()
            .filter(|path| path.is_absolute())
        {
            Some(path) => (
                AccountIdentity::Named(path.to_string_lossy().into_owned()),
                AccountScopeResolution::ExplicitScope,
            ),
            None => (
                AccountIdentity::Unknown,
                AccountScopeResolution::ScopeCanonicalizationFailed,
            ),
        },
        _ => (
            AccountIdentity::Unknown,
            AccountScopeResolution::DuplicateVariable,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn valid_codex() {
        assert_eq!(
            classify_executable(
                Path::new(
                    "/x/@openai/codex/node_modules/@openai/codex-linux-x64/vendor/a/bin/codex"
                ),
                "codex"
            ),
            Some(AgentKind::Codex)
        );
    }
    #[test]
    fn codex_comm_alone_is_not_enough() {
        assert_eq!(
            classify_executable(Path::new("/usr/bin/codex"), "codex"),
            None
        );
    }
    #[test]
    fn sandbox_is_rejected() {
        assert_eq!(
            classify_executable(
                Path::new("/x/@openai/codex-linux-sandbox/vendor/bin/codex"),
                "codex"
            ),
            None
        );
    }
    #[test]
    fn valid_claude() {
        assert_eq!(
            classify_executable(
                Path::new("/home/u/.local/share/claude/versions/2.1.260"),
                "claude"
            ),
            Some(AgentKind::ClaudeCode)
        );
    }

    #[test]
    fn claude_version_component_is_variable() {
        assert_eq!(
            classify_executable(
                Path::new("/home/u/.local/share/claude/versions/99.0.0"),
                "claude"
            ),
            Some(AgentKind::ClaudeCode)
        );
    }

    #[test]
    fn claude_helper_near_miss_is_rejected() {
        assert_eq!(
            classify_executable(
                Path::new("/home/u/.local/share/claude/versions/2.1.260/helper"),
                "claude"
            ),
            None
        );
        assert_eq!(
            classify_executable(
                Path::new("/home/u/.local/share/claude-helper/versions/2.1.260"),
                "claude"
            ),
            None
        );
    }

    #[test]
    fn account_scope_is_applied_only_after_positive_claude_classification() {
        let dir = std::env::temp_dir().join(format!("xbar-claude-scope-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let scoped = classify(ProcessRecord {
            process: super::super::ProcessIdentity {
                pid: 1,
                starttime: 1,
            },
            executable: Path::new("/home/u/.local/share/claude/versions/2.1.260").into(),
            comm: "claude".into(),
            environment: Some(vec![(
                "CLAUDE_CONFIG_DIR".into(),
                dir.to_string_lossy().into_owned(),
            )]),
        })
        .expect("current Claude topology should classify");
        assert_eq!(scoped.agent, AgentKind::ClaudeCode);
        assert_eq!(
            scoped.account_scope,
            AccountIdentity::Named(
                fs::canonicalize(&dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            )
        );

        assert!(classify(ProcessRecord {
            process: super::super::ProcessIdentity {
                pid: 2,
                starttime: 1,
            },
            executable: Path::new("/home/u/.local/share/claude-helper/versions/2.1.260").into(),
            comm: "claude".into(),
            environment: Some(vec![(
                "CLAUDE_CONFIG_DIR".into(),
                dir.to_string_lossy().into_owned()
            )]),
        })
        .is_none());
        fs::remove_dir(&dir).unwrap();
    }
    #[test]
    fn arbitrary_claude_is_rejected() {
        assert_eq!(
            classify_executable(Path::new("/usr/bin/claude"), "claude"),
            None
        );
    }

    #[test]
    fn antigravity_registry_topology_is_structural() {
        assert_eq!(
            classify_executable(Path::new("/home/u/.local/bin/agy"), "agy"),
            Some(AgentKind::Antigravity)
        );
        assert_eq!(
            classify_executable(
                Path::new("/home/u/.local/bin/agy.1788570018439200905.old"),
                "agy"
            ),
            Some(AgentKind::Antigravity)
        );
        assert_eq!(
            classify_executable(Path::new("/home/u/.local/bin/agy.42.old"), "agy"),
            Some(AgentKind::Antigravity)
        );
    }

    #[test]
    fn antigravity_near_misses_are_rejected() {
        for (path, comm) in [
            ("/home/u/.local/bin/agy.foo.old", "agy"),
            ("/home/u/.local/bin/agy-helper", "agy-helper"),
            ("/usr/bin/agy", "agy"),
            ("/home/u/.local/bin/agy.42.old", "other"),
        ] {
            assert_eq!(
                classify_executable(Path::new(path), comm),
                None,
                "{path} {comm}"
            );
        }
    }

    #[test]
    fn grok_versioned_topology_is_structural() {
        assert_eq!(
            classify_executable(
                Path::new("/home/u/.grok/downloads/grok-1.0.13-linux-x86_64"),
                "grok"
            ),
            Some(AgentKind::Grok)
        );
        assert_eq!(
            classify_executable(
                Path::new("/home/u/.grok/downloads/grok-9.4-linux-x86_64"),
                "grok"
            ),
            Some(AgentKind::Grok)
        );
    }

    #[test]
    fn grok_near_misses_are_rejected() {
        for (path, comm) in [
            ("/home/u/.grok/downloads/grok-1.0.13-linux-aarch64", "grok"),
            ("/home/u/.grok/bin/grok-1.0.13-linux-x86_64", "grok"),
            (
                "/home/u/.grok/downloads/grok-helper-1.0-linux-x86_64",
                "grok",
            ),
            ("/home/u/.grok/downloads/grok-1.0-linux-x86_64", "other"),
        ] {
            assert_eq!(
                classify_executable(Path::new(path), comm),
                None,
                "{path} {comm}"
            );
        }
    }

    #[test]
    fn registry_has_unique_agents_and_allows_unresolved_usage_source() {
        let registry = super::super::agent_registry();
        let ids: std::collections::BTreeSet<_> = registry.iter().map(|entry| entry.agent).collect();
        assert_eq!(ids.len(), 4);
        assert!(registry
            .iter()
            .find(|entry| entry.agent == AgentKind::Grok)
            .is_some_and(|entry| entry.usage_source.is_none()));
        assert!(registry
            .iter()
            .find(|entry| entry.agent == AgentKind::Antigravity)
            .is_some_and(
                |entry| entry.usage_source == Some(super::super::UsageSourceId::Antigravity)
            ));
    }
    #[test]
    fn account_defaults() {
        assert_eq!(account_scope(None, "CODEX_HOME"), AccountIdentity::Default);
        assert_eq!(
            account_scope(None, "CLAUDE_CONFIG_DIR"),
            AccountIdentity::Default
        );
    }
    #[test]
    fn invalid_account_is_unknown() {
        assert_eq!(
            account_scope(
                Some(&[("CODEX_HOME".into(), "/does/not/exist".into())]),
                "CODEX_HOME"
            ),
            AccountIdentity::Unknown
        );
    }

    #[test]
    fn unreadable_environment_is_unknown_with_sanitized_reason() {
        assert_eq!(
            account_scope_resolution(
                Some(&[("__XBAR_DISCOVERY_ENV_UNREADABLE".into(), "errno:13".into(),)]),
                "CODEX_HOME",
            ),
            (
                AccountIdentity::Unknown,
                AccountScopeResolution::EnvironmentUnreadable { errno: Some(13) },
            )
        );
    }

    #[test]
    fn duplicate_scope_variable_is_unknown_with_sanitized_reason() {
        assert_eq!(
            account_scope_resolution(
                Some(&[
                    ("CODEX_HOME".into(), "/one".into()),
                    ("CODEX_HOME".into(), "/two".into()),
                ]),
                "CODEX_HOME",
            ),
            (
                AccountIdentity::Unknown,
                AccountScopeResolution::DuplicateVariable,
            )
        );
    }

    #[test]
    fn malformed_environment_is_unknown_with_sanitized_reason() {
        assert_eq!(
            account_scope_resolution(
                Some(&[("__XBAR_DISCOVERY_ENV_UNREADABLE".into(), "malformed".into(),)]),
                "CLAUDE_CONFIG_DIR",
            ),
            (
                AccountIdentity::Unknown,
                AccountScopeResolution::EnvironmentMalformed,
            )
        );
    }
    #[test]
    fn custom_account_is_canonical() {
        let dir = std::env::temp_dir().join(format!("xbar-discovery-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let got = account_scope(
            Some(&[("CODEX_HOME".into(), dir.to_string_lossy().into_owned())]),
            "CODEX_HOME",
        );
        assert_eq!(
            got,
            AccountIdentity::Named(
                fs::canonicalize(&dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            )
        );
        fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn custom_claude_account_is_canonical() {
        let dir =
            std::env::temp_dir().join(format!("xbar-claude-discovery-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let got = account_scope(
            Some(&[(
                "CLAUDE_CONFIG_DIR".into(),
                dir.to_string_lossy().into_owned(),
            )]),
            "CLAUDE_CONFIG_DIR",
        );
        assert_eq!(
            got,
            AccountIdentity::Named(
                fs::canonicalize(&dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            )
        );
        fs::remove_dir(&dir).unwrap();
    }
}
