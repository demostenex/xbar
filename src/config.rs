use serde::Deserialize;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Config {
    #[serde(default)]
    pub external: ExternalConfig,
    #[serde(default)]
    pub bluetooth: BluetoothConfig,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ExternalConfig {
    pub floating_terminal: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct BluetoothConfig {
    pub manager_command: Option<Vec<String>>,
}

impl ExternalConfig {
    pub fn floating_terminal(&self) -> Option<Vec<String>> {
        self.floating_terminal
            .as_ref()
            .filter(|argv| {
                argv.first()
                    .is_some_and(|executable| !executable.is_empty())
            })
            .cloned()
    }
}

impl BluetoothConfig {
    pub fn manager_command(&self) -> Option<Vec<String>> {
        self.manager_command
            .as_ref()
            .filter(|argv| {
                argv.first()
                    .is_some_and(|executable| !executable.is_empty())
            })
            .cloned()
    }
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let Some(path) = config_path() else {
            return Ok(Self::default());
        };
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }
}

fn config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|base| base.join("xbar").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::{BluetoothConfig, Config};

    #[test]
    fn manager_command_is_optional_and_validates_argv() {
        assert_eq!(Config::default().bluetooth.manager_command(), None);
        assert_eq!(
            toml::from_str::<Config>("[bluetooth]\nmanager_command = []")
                .unwrap()
                .bluetooth
                .manager_command(),
            None
        );
        assert_eq!(
            toml::from_str::<Config>("[bluetooth]\nmanager_command = [\"\", \"arg\"]")
                .unwrap()
                .bluetooth
                .manager_command(),
            None
        );
        assert_eq!(
            toml::from_str::<Config>(
                "[bluetooth]\nmanager_command = [\"fake-manager\", \"--test\"]"
            )
            .unwrap()
            .bluetooth
            .manager_command(),
            Some(vec!["fake-manager".into(), "--test".into()])
        );
        assert_eq!(BluetoothConfig::default().manager_command(), None);
    }

    #[test]
    fn floating_terminal_is_optional_and_validates_argv_independently() {
        assert_eq!(Config::default().external.floating_terminal(), None);
        assert_eq!(
            toml::from_str::<Config>("[external]\nfloating_terminal = [\"terminal-test\", \"-e\"]")
                .unwrap()
                .external
                .floating_terminal(),
            Some(vec!["terminal-test".into(), "-e".into()])
        );
        assert_eq!(
            toml::from_str::<Config>("[external]\nfloating_terminal = []")
                .unwrap()
                .external
                .floating_terminal(),
            None
        );
        assert_eq!(
            toml::from_str::<Config>("[external]\nfloating_terminal = [\"\", \"-e\"]")
                .unwrap()
                .external
                .floating_terminal(),
            None
        );
        assert_eq!(
            toml::from_str::<Config>(
                "[bluetooth]\nmanager_command = [\"manager-test\", \"--foo\"]"
            )
            .unwrap()
            .bluetooth
            .manager_command(),
            Some(vec!["manager-test".into(), "--foo".into()])
        );
    }
}
