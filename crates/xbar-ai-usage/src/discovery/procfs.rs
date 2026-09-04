use std::fmt;
use std::fs;
use std::path::PathBuf;

use super::classifier::AccountScopeResolution;
use super::{classify, ProcessIdentity, ProcessRecord};

#[derive(Debug)]
pub enum ProcFsError {
    ReadDir(std::io::Error),
}
impl fmt::Display for ProcFsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadDir(e) => write!(f, "cannot scan /proc: {e}"),
        }
    }
}
impl std::error::Error for ProcFsError {}

pub fn startup_snapshot() -> Result<Vec<ProcessRecord>, ProcFsError> {
    let entries = fs::read_dir("/proc").map_err(ProcFsError::ReadDir)?;
    let mut records = Vec::new();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if let Some(record) = inspect(pid) {
            records.push(record);
        }
    }
    Ok(records)
}

pub(crate) fn inspect(pid: u32) -> Option<ProcessRecord> {
    let root = PathBuf::from(format!("/proc/{pid}"));
    let starttime = parse_starttime(&fs::read_to_string(root.join("stat")).ok()?)?;
    let executable = fs::canonicalize(root.join("exe")).ok()?;
    let comm = fs::read_to_string(root.join("comm"))
        .ok()?
        .trim_end()
        .to_owned();
    let candidate = ProcessRecord {
        process: ProcessIdentity { pid, starttime },
        executable: executable.clone(),
        comm: comm.clone(),
        environment: None,
    };
    let environment = if let Some(agent) = classify(candidate.clone()) {
        let variable = match agent.agent {
            super::AgentKind::Codex => "CODEX_HOME",
            super::AgentKind::ClaudeCode => "CLAUDE_CONFIG_DIR",
        };
        Some(
            read_environment(&root.join("environ"), variable).unwrap_or_else(|error| {
                let detail = match error {
                    AccountScopeResolution::EnvironmentUnreadable { errno } => {
                        format!("errno:{}", errno.unwrap_or_default())
                    }
                    AccountScopeResolution::EnvironmentMalformed => "malformed".into(),
                    _ => "malformed".into(),
                };
                vec![("__XBAR_DISCOVERY_ENV_UNREADABLE".into(), detail)]
            }),
        )
    } else {
        None
    };
    let final_starttime = parse_starttime(&fs::read_to_string(root.join("stat")).ok()?)?;
    if !identity_still_matches(starttime, final_starttime) {
        return None;
    }
    Some(ProcessRecord {
        process: ProcessIdentity { pid, starttime },
        executable,
        comm,
        environment,
    })
}

fn identity_still_matches(first: u64, reread: u64) -> bool {
    first == reread
}

fn parse_starttime(stat: &str) -> Option<u64> {
    let end = stat.rfind(") ")? + 2;
    stat[end..].split_whitespace().nth(19)?.parse().ok()
}

fn read_environment(
    path: &std::path::Path,
    wanted: &str,
) -> Result<Vec<(String, String)>, AccountScopeResolution> {
    let bytes = fs::read(path).map_err(|error| AccountScopeResolution::EnvironmentUnreadable {
        errno: error.raw_os_error(),
    })?;
    let mut values = Vec::new();
    for item in bytes.split(|byte| *byte == 0) {
        if item.is_empty() {
            continue;
        }
        let separator = item
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or(AccountScopeResolution::EnvironmentMalformed)?;
        let (name, value) = item.split_at(separator);
        let value = &value[1..];
        if name == wanted.as_bytes() {
            values.push((
                String::from_utf8(name.to_vec())
                    .map_err(|_| AccountScopeResolution::EnvironmentMalformed)?,
                String::from_utf8(value.to_vec())
                    .map_err(|_| AccountScopeResolution::EnvironmentMalformed)?,
            ));
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_linux_stat_starttime() {
        let stat = "1 (a process) S 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 4242 1";
        assert_eq!(parse_starttime(stat), Some(4242));
    }

    #[test]
    fn stat_parser_handles_parentheses_and_spaces_in_comm() {
        let fields = ["1"; 18].join(" ");
        let stat = format!("1 (odd ) name (x)) S {fields} 9876");
        assert_eq!(parse_starttime(&stat), Some(9876));
    }

    #[test]
    fn malformed_stat_is_rejected() {
        assert_eq!(parse_starttime("1 missing fields"), None);
    }

    #[test]
    fn process_facts_are_rejected_when_pid_identity_changes() {
        assert!(!identity_still_matches(10, 11));
        assert!(identity_still_matches(10, 10));
    }
}
