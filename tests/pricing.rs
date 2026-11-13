mod common;

use common::Env;
use session_ledger::config::Config;
use session_ledger::pricing::PriceTable;
use session_ledger::transcript::MessageUsage;

const TOML: &str = r#"
as_of = "2026-01-02"
web_search_per_1000 = 10.0

[models.test-model]
input = 3.0
output = 15.0
cache_read = 0.30
cache_write_5m = 3.75
cache_write_1h = 6.0
"#;

fn table() -> PriceTable {
    PriceTable::from_toml(TOML).unwrap()
}

fn usage(model: &str) -> MessageUsage {
    MessageUsage {
        key: "k".into(),
        model: model.into(),
        input: 1000,
        output: 2000,
        cache_read: 100_000,
        cache_write_5m: 10_000,
        cache_write_1h: 20_000,
        web_search: 0,
        service_tier: None,
        speed: None,
        subagent: false,
    }
}

fn approx(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "{a} vs {b}");
}

#[test]
fn hand_calculated() {
    // 1000*3 + 2000*15 + 100000*0.30 + 10000*3.75 + 20000*6 = 220_500 / 1e6
    approx(table().cost(&usage("test-model")).unwrap(), 0.2205);
}

#[test]
fn web_search_added() {
    let mut u = usage("test-model");
    u.web_search = 3;
    approx(table().cost(&u).unwrap(), 0.2505);
}

#[test]
fn unknown_model_is_none() {
    assert!(table().cost(&usage("other-model")).is_none());
}

#[test]
fn dated_id_matches_by_prefix() {
    approx(table().cost(&usage("test-model-20260101")).unwrap(), 0.2205);
}

#[test]
fn prefix_needs_a_date_suffix() {
    let t = table();
    assert!(t.rates_for("test-model-20260101").is_some());
    for m in [
        "test-model-x",
        "test-model-2",
        "test-model-2026010",
        "test-model-20260101-v2",
        "test-model-202601011",
        "test-modelx-20260101",
        "test-model-2026010a",
    ] {
        assert!(t.rates_for(m).is_none(), "{m}");
        assert!(t.cost(&usage(m)).is_none(), "{m}");
    }
}

#[test]
fn fast_without_fast_rates_is_none() {
    let mut u = usage("test-model");
    u.speed = Some("fast".into());
    assert!(table().cost(&u).is_none());
}

const FAST_BATCH: &str = r#"
web_search_per_1000 = 10.0
[models.m]
input = 3.0
output = 15.0
cache_read = 0.30
cache_write_5m = 3.75
cache_write_1h = 6.0
[models.m.fast]
input = 6.0
output = 30.0
cache_read = 0.60
cache_write_5m = 7.5
cache_write_1h = 12.0
[models.m.batch]
input = 1.5
output = 7.5
cache_read = 0.15
cache_write_5m = 1.875
cache_write_1h = 3.0
"#;

#[test]
fn fast_uses_fast_rates() {
    let t = PriceTable::from_toml(FAST_BATCH).unwrap();
    let mut u = usage("m");
    u.speed = Some("fast".into());
    approx(t.cost(&u).unwrap(), 0.441);
    // batch tier uses batch rates, standard tier uses base rates
    let mut b = usage("m");
    b.service_tier = Some("batch".into());
    approx(t.cost(&b).unwrap(), 0.11025);
    b.service_tier = Some("standard".into());
    approx(t.cost(&b).unwrap(), 0.2205);
}

#[test]
fn unknown_service_tier_is_none() {
    let mut u = usage("test-model");
    u.service_tier = Some("priority".into());
    assert!(table().cost(&u).is_none());
    // batch requested but the model has no batch rates
    u.service_tier = Some("batch".into());
    assert!(table().cost(&u).is_none());
}

#[test]
fn fast_with_unknown_tier_is_none() {
    let t = PriceTable::from_toml(FAST_BATCH).unwrap();
    let mut u = usage("m");
    u.speed = Some("fast".into());
    u.service_tier = Some("priority".into());
    assert!(t.cost(&u).is_none());
    // fast with the standard tier still uses fast rates
    u.service_tier = Some("standard".into());
    approx(t.cost(&u).unwrap(), 0.441);
}

#[test]
fn fast_with_batch_tier_is_none() {
    let t = PriceTable::from_toml(FAST_BATCH).unwrap();
    let mut u = usage("m");
    u.speed = Some("fast".into());
    u.service_tier = Some("batch".into());
    assert!(t.cost(&u).is_none());
}

#[test]
fn long_context_switches_whole_message() {
    let t = PriceTable::from_toml(
        r#"
web_search_per_1000 = 10.0
[models.m]
input = 1.0
output = 2.0
cache_read = 0.1
cache_write_5m = 1.25
cache_write_1h = 2.0
[models.m.long_context]
threshold_tokens = 100000
input = 2.0
output = 4.0
cache_read = 0.2
cache_write_5m = 2.5
cache_write_1h = 4.0
"#,
    )
    .unwrap();
    let mut u = usage("m");
    u.input = 100_000;
    u.output = 1_000_000;
    u.cache_read = 0;
    u.cache_write_5m = 0;
    u.cache_write_1h = 0;
    // exactly at the threshold: base rates
    approx(t.cost(&u).unwrap(), 0.1 + 2.0);
    // one token over: long-context rates apply to input and output
    u.input = 100_001;
    approx(t.cost(&u).unwrap(), 0.200002 + 4.0);
    // cache tokens count toward the threshold
    u.input = 1;
    u.cache_read = 100_000;
    approx(t.cost(&u).unwrap(), 0.000002 + 0.02 + 4.0);
}

#[test]
fn config_override_replaces_builtin_model() {
    let env = Env::new();
    let file = env.data.join("config.toml");
    std::fs::write(
        &file,
        "[prices.claude-opus-5-5]\ninput = 1.0\noutput = 2.0\ncache_read = 3.0\ncache_write_5m = 4.0\ncache_write_1h = 5.0\n",
    )
    .unwrap();
    let cfg = Config::load(&file).unwrap();
    let t = PriceTable::for_config(&cfg);
    let r = t.rates_for("claude-opus-5-5").unwrap();
    assert_eq!((r.input, r.output, r.cache_write_1h), (1.0, 2.0, 5.0));
    assert!(r.fast.is_none());
    // other builtin models are untouched
    assert!(t.rates_for("claude-sonnet-5-5").is_some());
}

#[test]
fn as_of_reads_top_level_date() {
    assert_eq!(table().as_of(), Some("2026-01-02"));
    let t = PriceTable::from_toml("web_search_per_1000 = 10.0\n").unwrap();
    assert_eq!(t.as_of(), None);
    assert_eq!(PriceTable::builtin().as_of(), Some("2026-11-13"));
}

#[test]
fn builtin_table_parses_and_prices_current_models() {
    let t = PriceTable::builtin();
    for m in [
        "claude-fable-5-2",
        "claude-fable-5-1",
        "claude-fable-5",
        "claude-opus-5-5",
        "claude-opus-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
    ] {
        assert!(t.rates_for(m).is_some(), "{m}");
    }
}

// ---- hardening ----

const BASE: &str = "input = 3.0\noutput = 15.0\ncache_read = 0.30\ncache_write_5m = 3.75\ncache_write_1h = 6.0\n";

#[test]
fn web_search_without_a_rate_is_unknown() {
    let t = PriceTable::from_toml(&format!("[models.test-model]\n{BASE}")).unwrap();
    approx(t.cost(&usage("test-model")).unwrap(), 0.2205);
    let mut u = usage("test-model");
    u.web_search = 1;
    assert!(t.cost(&u).is_none());
    // The built-in table keeps its rate.
    let b = PriceTable::builtin();
    let mut u = usage("claude-opus-5-5");
    let plain = b.cost(&u).unwrap();
    u.web_search = 2;
    approx(b.cost(&u).unwrap(), plain + 0.02);
}

/// The error text from loading `text` as a price table.
fn table_error(text: &str) -> String {
    format!("{:#}", PriceTable::from_toml(text).expect_err(text))
}

#[test]
fn table_rates_must_be_finite_and_not_negative() {
    for (field, value) in [("input", "-1.0"), ("output", "nan"), ("cache_read", "inf"), ("cache_write_1h", "-0.5")] {
        let body = BASE
            .lines()
            .map(|l| if l.starts_with(&format!("{field} =")) { format!("{field} = {value}") } else { l.to_string() })
            .collect::<Vec<_>>()
            .join("\n");
        let err = table_error(&format!("[models.bad-model]\n{body}\n"));
        assert!(err.contains("bad-model") && err.contains(field), "{field}: {err}");
    }
    let err = table_error(&format!("[models.m]\n{BASE}[models.m.batch]\n{}", BASE.replace("output = 15.0", "output = -15.0")));
    assert!(err.contains("model m") && err.contains("batch.output"), "{err}");
    let err = table_error(&format!("web_search_per_1000 = -1.0\n[models.m]\n{BASE}"));
    assert!(err.contains("web_search_per_1000"), "{err}");
    // Zero is allowed.
    PriceTable::from_toml(&format!("[models.m]\n{}", BASE.replace("3.0", "0.0"))).unwrap();
}

#[test]
fn long_context_threshold_must_be_above_zero() {
    let err = table_error(&format!("[models.m]\n{BASE}[models.m.long_context]\nthreshold_tokens = 0\n{BASE}"));
    assert!(err.contains("model m") && err.contains("long_context.threshold_tokens"), "{err}");
}

#[test]
fn long_context_may_not_nest_tiers() {
    for inner in ["fast", "batch", "long_context"] {
        let extra = if inner == "long_context" { "threshold_tokens = 5\n" } else { "" };
        let text = format!(
            "[models.m]\n{BASE}[models.m.long_context]\nthreshold_tokens = 100\n{BASE}[models.m.long_context.{inner}]\n{extra}{BASE}"
        );
        let err = table_error(&text);
        assert!(err.contains("model m") && err.contains(&format!("long_context.{inner}")), "{inner}: {err}");
    }
    // Inside a batch block too.
    let text = format!(
        "[models.m]\n{BASE}[models.m.batch]\n{BASE}[models.m.batch.long_context]\nthreshold_tokens = 100\n{BASE}[models.m.batch.long_context.fast]\n{BASE}"
    );
    let err = table_error(&text);
    assert!(err.contains("batch.long_context.fast"), "{err}");
}

#[test]
fn config_prices_and_multiplier_are_validated() {
    let env = Env::new();
    let file = env.data.join("config.toml");
    let load_err = |text: &str| {
        std::fs::write(&file, text).unwrap();
        format!("{:#}", Config::load(&file).expect_err(text))
    };
    let err = load_err(&format!("[prices.my-model]\n{}", BASE.replace("input = 3.0", "input = -3.0")));
    assert!(err.contains("my-model") && err.contains("input"), "{err}");
    let err = load_err(&format!("[prices.my-model]\n{BASE}[prices.my-model.long_context]\nthreshold_tokens = 0\n{BASE}"));
    assert!(err.contains("my-model") && err.contains("threshold_tokens"), "{err}");
    for m in ["-1.0", "nan", "inf"] {
        let err = load_err(&format!("cost_mode = \"custom\"\nmultiplier = {m}\n"));
        assert!(err.contains("multiplier"), "{m}: {err}");
    }
    std::fs::write(&file, format!("multiplier = 0.0\n[prices.my-model]\n{BASE}")).unwrap();
    Config::load(&file).unwrap();
}

#[test]
fn huge_token_counts_do_not_overflow() {
    let mut u = usage("claude-haiku-5-5");
    u.input = u64::MAX;
    u.cache_read = u64::MAX;
    u.cache_write_5m = u64::MAX - 1;
    u.cache_write_1h = u64::MAX;
    let c = PriceTable::builtin().cost(&u).unwrap();
    assert!(c.is_finite() && c > 0.0, "{c}");
}

/// Haiku 5.5 usage with `input` input tokens and 1M output tokens, batch tier.
fn haiku_batch(input: u64) -> MessageUsage {
    let mut u = usage("claude-haiku-5-5");
    u.input = input;
    u.output = 1_000_000;
    u.cache_read = 0;
    u.cache_write_5m = 0;
    u.cache_write_1h = 0;
    u.service_tier = Some("batch".into());
    u
}

#[test]
fn builtin_haiku_batch_with_long_context() {
    let t = PriceTable::builtin();
    // At the 100k threshold: batch rates 0.05 / 0.25.
    approx(t.cost(&haiku_batch(100_000)).unwrap(), 100_000.0 * 0.05 / 1e6 + 0.25);
    // Over it: batch long-context rates 0.25 / 1.25.
    approx(t.cost(&haiku_batch(100_001)).unwrap(), 100_001.0 * 0.25 / 1e6 + 1.25);
}

#[test]
fn fast_with_long_context() {
    let t = PriceTable::from_toml(&format!(
        "[models.m]\n{BASE}[models.m.fast]\ninput = 6.0\noutput = 30.0\ncache_read = 0.6\ncache_write_5m = 7.5\ncache_write_1h = 12.0\n\
         [models.m.fast.long_context]\nthreshold_tokens = 1000\ninput = 12.0\noutput = 60.0\ncache_read = 1.2\ncache_write_5m = 15.0\ncache_write_1h = 24.0\n"
    ))
    .unwrap();
    let mut u = usage("m");
    u.speed = Some("fast".into());
    u.input = 1000;
    u.output = 1_000_000;
    u.cache_read = 0;
    u.cache_write_5m = 0;
    u.cache_write_1h = 0;
    approx(t.cost(&u).unwrap(), 1000.0 * 6.0 / 1e6 + 30.0);
    u.cache_read = 1;
    approx(t.cost(&u).unwrap(), (1000.0 * 12.0 + 1.2) / 1e6 + 60.0);
    // Standard speed ignores the fast block's long-context rates.
    u.speed = None;
    approx(t.cost(&u).unwrap(), (1000.0 * 3.0 + 0.3) / 1e6 + 15.0);
}
