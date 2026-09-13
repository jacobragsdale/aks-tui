//! The command line: the flags that override `config.toml`.

use std::path::PathBuf;

use clap::Parser;

use crate::config::Config;
use crate::ui::theme::ThemeChoice;

#[derive(Debug, Parser)]
#[command(
    name = "aks-tui",
    version,
    about = "A fast terminal browser for AKS: pods, logs, events, configmaps and secrets"
)]
pub struct Cli {
    /// Seconds between reads of the open tab; 0 leaves `r` as the only read.
    #[arg(long, value_name = "SECS")]
    pub refresh: Option<u64>,

    /// terminal · terminal-light · mono · custom
    #[arg(long, value_name = "NAME", global = true)]
    pub theme: Option<String>,

    /// Read this file instead of ~/.config/aks-tui/config.toml.
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Keep the cache here instead of in the data directory.
    #[arg(long, value_name = "PATH", global = true)]
    pub cache: Option<PathBuf>,

    /// Neither read nor write the cache.
    #[arg(long, global = true)]
    pub no_cache: bool,
}

impl Cli {
    /// The configuration this run works from: the file, then every flag that
    /// was given on top of it.
    #[must_use]
    pub fn merge(&self, mut config: Config) -> Config {
        if let Some(refresh) = self.refresh {
            config.refresh = Some(refresh);
        }
        config
    }

    /// Which theme this run paints with, once the file has been read.
    pub fn resolve_theme(&self, config: &Config) -> anyhow::Result<ThemeChoice> {
        let env = std::env::var("AKS_TUI_THEME").ok();
        let chosen = crate::ui::theme::chosen_theme(self.theme.as_deref(), env.as_deref())?;
        ThemeChoice::resolve(std::env::var_os("NO_COLOR").is_some(), chosen, config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_all_optional_and_a_flag_beats_the_file() {
        let cli = Cli::parse_from(["aks-tui"]);
        assert!(cli.refresh.is_none());
        assert!(!cli.no_cache);

        let file = crate::config::parse("refresh = 30\n").unwrap();
        assert_eq!(
            Cli::parse_from(["aks-tui"]).merge(file.clone()).refresh,
            Some(30)
        );
        assert_eq!(
            Cli::parse_from(["aks-tui", "--refresh", "0", "--no-cache"])
                .merge(file)
                .refresh,
            Some(0)
        );
    }
}
