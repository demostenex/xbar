use std::path::Path;

use super::{AgentInstance, AgentKind, ProcessRecord, ProviderKind};
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
    let agent = classify_executable(&executable, &record.comm)?;
    let (provider, variable) = match agent {
        AgentKind::Codex => (ProviderKind::OpenAi, "CODEX_HOME"),
        AgentKind::ClaudeCode => (ProviderKind::Anthropic, "CLAUDE_CONFIG_DIR"),
    };
    let (account_scope, account_scope_resolution) =
        account_scope_resolution(record.environment.as_deref(), variable);
    Some(AgentInstance {
        process: record.process,
        agent,
        provider,
        account_scope,
        account_scope_resolution,
        executable,
    })
}

fn classify_executable(path: &Path, comm: &str) -> Option<AgentKind> {
    let components: Vec<_> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if path.file_name().and_then(|n| n.to_str()) == Some("codex")
        && comm == "codex"
        && components.windows(2).any(|w| w == ["bin", "codex"])
        && components.contains(&"vendor")
        && components.windows(2).any(|w| w == ["@openai", "codex"])
        && !components.contains(&"codex-linux-sandbox")
    {
        return Some(AgentKind::Codex);
    }
    if path.file_name().and_then(|n| n.to_str()) == Some("claude")
        && comm == "claude"
        && is_versioned_claude_path(&components)
    {
        return Some(AgentKind::ClaudeCode);
    }
    None
}

fn is_versioned_claude_path(components: &[&str]) -> bool {
    let Some(index) = components
        .windows(2)
        .position(|w| w == ["claude", "versions"])
    else {
        return false;
    };
    index >= 2
        && components[index - 2..index + 1] == [".local", "share", "claude"]
        && components.len() == index + 4
        && !components[index + 2].is_empty()
        && components[index + 2] != "."
        && components[index + 2] != ".."
        && components[index + 3] == "claude"
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
                Path::new("/home/u/.local/share/claude/versions/2.9.1/claude"),
                "claude"
            ),
            Some(AgentKind::ClaudeCode)
        );
    }
    #[test]
    fn arbitrary_claude_is_rejected() {
        assert_eq!(
            classify_executable(Path::new("/usr/bin/claude"), "claude"),
            None
        );
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
