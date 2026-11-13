use crate::pricing::{check_rate, Rates};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CostMode {
    List,
    Custom,
    Tokens,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub port: u16,
    pub cost_mode: CostMode,
    pub multiplier: f64,
    pub prices: BTreeMap<String, Rates>,
    /// Demo mode: set by `sled serve --demo`, never read from the config file.
    #[serde(skip)]
    pub demo: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: 7878,
            cost_mode: CostMode::List,
            multiplier: 1.0,
            prices: BTreeMap::new(),
            demo: false,
        }
    }
}

impl Config {
    /// Reads the config file. A missing file yields defaults; a malformed one is an error.
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<Config>(&text)
                .map_err(anyhow::Error::from)
                .and_then(|c| c.validate().map(|()| c))
                .with_context(|| format!("invalid config file {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// `multiplier` and every `[prices]` rate must be finite and not negative; see
    /// [`Rates::validate`].
    pub fn validate(&self) -> Result<()> {
        check_rate("multiplier", self.multiplier)?;
        for (model, rates) in &self.prices {
            rates.validate(model)?;
        }
        Ok(())
    }
}
