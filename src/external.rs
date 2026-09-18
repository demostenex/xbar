use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalPresentation {
    FloatingTerminal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalLaunchRequest {
    pub argv: Vec<String>,
    pub presentation: ExternalPresentation,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExternalLauncher {
    floating_terminal: Option<Vec<String>>,
}

impl ExternalLauncher {
    pub fn new(floating_terminal: Option<Vec<String>>) -> Self {
        Self { floating_terminal }
    }

    pub fn launch(&self, request: &ExternalLaunchRequest) -> Result<(), String> {
        let argv = self.compose_argv(request)?;
        let (executable, args) = argv
            .split_first()
            .ok_or_else(|| "external launch resolved to an empty argv".to_owned())?;
        Command::new(executable)
            .args(args)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("failed to spawn external command {executable:?}: {error}"))
    }

    fn compose_argv(&self, request: &ExternalLaunchRequest) -> Result<Vec<String>, String> {
        let manager = valid_argv(&request.argv)
            .ok_or_else(|| "external launch requested with an empty executable".to_owned())?;
        match request.presentation {
            ExternalPresentation::FloatingTerminal => {
                let prefix = self
                    .floating_terminal
                    .as_deref()
                    .and_then(valid_argv)
                    .ok_or_else(|| {
                        "FloatingTerminal launch requested but external.floating_terminal is not configured"
                            .to_owned()
                    })?;
                let mut argv = prefix.to_vec();
                argv.extend_from_slice(manager);
                Ok(argv)
            }
        }
    }
}

fn valid_argv(argv: &[String]) -> Option<&[String]> {
    (!argv.is_empty() && !argv[0].is_empty()).then_some(argv)
}

#[cfg(test)]
mod tests {
    use super::{ExternalLaunchRequest, ExternalLauncher, ExternalPresentation};

    fn request(argv: &[&str]) -> ExternalLaunchRequest {
        ExternalLaunchRequest {
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
            presentation: ExternalPresentation::FloatingTerminal,
        }
    }

    #[test]
    fn floating_terminal_composition_preserves_argv_boundaries() {
        let launcher = ExternalLauncher::new(Some(
            ["terminal-test", "--identity", "external-test", "-e"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        ));
        let resolved = launcher
            .compose_argv(&request(&["manager-test", "--foo"]))
            .unwrap();
        assert_eq!(
            resolved,
            [
                "terminal-test",
                "--identity",
                "external-test",
                "-e",
                "manager-test",
                "--foo"
            ]
        );
    }

    #[test]
    fn missing_or_empty_configuration_never_falls_back_to_direct_launch() {
        let request = request(&["manager-test"]);
        assert!(ExternalLauncher::default().launch(&request).is_err());
        assert!(ExternalLauncher::new(Some(Vec::new()))
            .launch(&request)
            .is_err());
        assert!(ExternalLauncher::new(Some(vec![String::new()]))
            .launch(&request)
            .is_err());
        assert!(
            ExternalLauncher::new(Some(vec!["missing-executable-test".into()]))
                .launch(&request)
                .is_err()
        );
    }

    #[test]
    fn empty_manager_argv_is_rejected_without_spawn() {
        let launcher = ExternalLauncher::new(Some(vec!["terminal-test".into()]));
        assert!(launcher
            .launch(&ExternalLaunchRequest {
                argv: Vec::new(),
                presentation: ExternalPresentation::FloatingTerminal,
            })
            .is_err());
    }
}
