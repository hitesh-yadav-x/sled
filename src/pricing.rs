use crate::config::Config;
use crate::transcript::MessageUsage;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// USD per million tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    /// Rates when `speed == "fast"`.
    pub fast: Option<Box<Rates>>,
    /// Rates when `service_tier == "batch"`.
    pub batch: Option<Box<Rates>>,
    pub long_context: Option<LongContext>,
}

impl Rates {
    /// Every rate must be finite and not negative, each `long_context.threshold_tokens` above 0,
    /// and a `long_context` block may not hold `fast`, `batch` or `long_context`. The error
    /// names the model and the field.
    pub fn validate(&self, model: &str) -> Result<()> {
        self.check(model, "", true)
    }

    fn check(&self, model: &str, at: &str, may_nest: bool) -> Result<()> {
        for (field, value) in [
            ("input", self.input),
            ("output", self.output),
            ("cache_read", self.cache_read),
            ("cache_write_5m", self.cache_write_5m),
            ("cache_write_1h", self.cache_write_1h),
        ] {
            check_rate(&format!("price for model {model}: {at}{field}"), value)?;
        }
        for (block, present) in [
            ("fast", self.fast.is_some()),
            ("batch", self.batch.is_some()),
            ("long_context", self.long_context.is_some()),
        ] {
            if present && !may_nest {
                bail!("price for model {model}: {at}{block} is not allowed inside long_context");
            }
        }
        if let Some(f) = &self.fast {
            f.check(model, &format!("{at}fast."), true)?;
        }
        if let Some(b) = &self.batch {
            b.check(model, &format!("{at}batch."), true)?;
        }
        if let Some(lc) = &self.long_context {
            if lc.threshold_tokens == 0 {
                bail!("price for model {model}: {at}long_context.threshold_tokens must be above 0");
            }
            lc.rates.check(model, &format!("{at}long_context."), false)?;
        }
        Ok(())
    }
}

/// A rate or multiplier must be a finite number, not negative. `what` names it in the error.
pub fn check_rate(what: &str, value: f64) -> Result<()> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        bail!("{what} must be a finite number, not negative (found {value})")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LongContext {
    pub threshold_tokens: u64,
    #[serde(flatten)]
    pub rates: Box<Rates>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PriceTable {
    #[serde(default)]
    as_of: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, Rates>,
    /// USD per 1000 web searches; without it, a message that searched has unknown cost.
    #[serde(default)]
    web_search_per_1000: Option<f64>,
}

impl PriceTable {
    pub fn from_toml(text: &str) -> Result<PriceTable> {
        let t: PriceTable = toml::from_str(text).context("invalid price table")?;
        t.validate().context("invalid price table")?;
        Ok(t)
    }

    fn validate(&self) -> Result<()> {
        if let Some(w) = self.web_search_per_1000 {
            check_rate("web_search_per_1000", w)?;
        }
        for (model, rates) in &self.models {
            rates.validate(model)?;
        }
        Ok(())
    }

    pub fn builtin() -> PriceTable {
        PriceTable::from_toml(include_str!("../assets/prices.toml"))
            .expect("assets/prices.toml must parse")
    }

    /// The built-in table, with `cfg.prices` entries replacing same-named models.
    pub fn for_config(cfg: &Config) -> PriceTable {
        let mut t = PriceTable::builtin();
        for (model, rates) in &cfg.prices {
            t.models.insert(model.clone(), rates.clone());
        }
        t
    }

    /// Date the built-in prices were looked up, if the table states one.
    pub fn as_of(&self) -> Option<&str> {
        self.as_of.as_deref()
    }

    /// Exact id, else a table key followed by a date suffix (`-` and 8 ASCII digits).
    /// Nothing else matches, so an unlisted model never borrows another model's price.
    pub fn rates_for(&self, model: &str) -> Option<&Rates> {
        if let Some(r) = self.models.get(model) {
            return Some(r);
        }
        let cut = model.len().checked_sub(9)?;
        if !model.is_char_boundary(cut) {
            return None;
        }
        let (base, suffix) = model.split_at(cut);
        let digits = suffix.strip_prefix('-')?;
        if digits.len() == 8 && digits.bytes().all(|b| b.is_ascii_digit()) {
            self.models.get(base)
        } else {
            None
        }
    }

    /// Cost in USD; `None` means the price is unknown.
    pub fn cost(&self, u: &MessageUsage) -> Option<f64> {
        let mut r = self.rates_for(&u.model)?;
        let batch = match u.service_tier.as_deref() {
            None | Some("standard") => false,
            Some("batch") => true,
            Some(_) => return None,
        };
        let fast = u.speed.as_deref() == Some("fast");
        r = match (fast, batch) {
            (true, true) => return None,
            (true, false) => r.fast.as_deref()?,
            (false, true) => r.batch.as_deref()?,
            (false, false) => r,
        };
        if let Some(lc) = &r.long_context {
            let prompt = u
                .input
                .saturating_add(u.cache_read)
                .saturating_add(u.cache_write_5m)
                .saturating_add(u.cache_write_1h);
            if prompt > lc.threshold_tokens {
                r = &lc.rates;
            }
        }
        let tokens = u.input as f64 * r.input
            + u.output as f64 * r.output
            + u.cache_read as f64 * r.cache_read
            + u.cache_write_5m as f64 * r.cache_write_5m
            + u.cache_write_1h as f64 * r.cache_write_1h;
        let web = match u.web_search {
            0 => 0.0,
            n => n as f64 * self.web_search_per_1000? / 1000.0,
        };
        Some(tokens / 1e6 + web)
    }
}
