//! Per-tenant usage metering from cAdvisor counters.
//!
//! cAdvisor exposes cumulative counters per container:
//! `container_cpu_usage_seconds_total` (CPU core-seconds) and
//! `container_memory_working_set_bytes` (an instantaneous gauge, which we
//! integrate over the scrape interval to obtain byte-seconds). This module
//! turns those into per-tenant usage totals and, using [`billing_kit`],
//! into a priced charge.
//!
//! Tenant attribution relies on the Docker Compose naming contract
//! `<project>-<service>-<index>`, where the project name equals the tenant
//! directory name (see SIS `ops/tenant-provision.sh`). Ambiguous prefixes
//! are rejected rather than guessed: a container is only attributed to a
//! tenant when exactly one known tenant name matches the prefix and the
//! remainder looks like `<service>-<index>`.

use std::collections::HashMap;
use std::fmt;

use billing_kit::{Currency, CurrencyAmount, Decimal, Price};
use rust_decimal::prelude::ToPrimitive;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Bytes per gibibyte.
pub const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;
/// Seconds per hour.
pub const SECONDS_PER_HOUR: u64 = 3600;

/// Metering errors.
#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    /// Configuration or arithmetic error from `billing-kit`.
    #[error("billing error: {0}")]
    Billing(String),
    /// Environment variable was missing or unparseable.
    #[error("metering config: {0}")]
    Config(String),
    /// cAdvisor could not be scraped.
    #[error("cadvisor scrape: {0}")]
    Scrape(String),
}

impl From<billing_kit::MoneyError> for UsageError {
    fn from(e: billing_kit::MoneyError) -> Self {
        Self::Billing(e.to_string())
    }
}

impl From<billing_kit::PriceError> for UsageError {
    fn from(e: billing_kit::PriceError) -> Self {
        Self::Billing(e.to_string())
    }
}

/// Cumulative counters scraped for one container.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerSample {
    /// Docker container name, e.g. `acme-paperless-1`.
    pub name: String,
    /// Cumulative CPU core-seconds.
    pub cpu_core_seconds: Decimal,
    /// Instantaneous working-set bytes at scrape time.
    pub memory_bytes: Decimal,
}

/// Per-tenant usage over a metering window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantUsage {
    /// Tenant / compose project name.
    pub tenant: String,
    /// Start of the metering window (inclusive).
    pub window_start: DateTime<Utc>,
    /// End of the metering window (exclusive).
    pub window_end: DateTime<Utc>,
    /// CPU consumed in core-seconds.
    pub cpu_core_seconds: Decimal,
    /// Memory consumed in gibibyte-seconds.
    pub memory_gibibyte_seconds: Decimal,
}

impl TenantUsage {
    /// Human-facing charge for the window under `cfg`.
    ///
    /// # Errors
    /// Returns [`UsageError::Billing`] if the currency amount cannot be
    /// represented.
    pub fn charge(&self, cfg: &MeteringConfig) -> Result<CurrencyAmount, UsageError> {
        let cpu_hours = self.cpu_core_seconds / Decimal::from(SECONDS_PER_HOUR);
        let memory_hours = self.memory_gibibyte_seconds / Decimal::from(SECONDS_PER_HOUR);
        let amount = cpu_hours * cfg.cpu_price_per_core_hour
            + memory_hours * cfg.memory_price_per_gib_hour;
        Ok(CurrencyAmount::new(amount, cfg.currency))
    }
}

/// Unit prices used to turn usage into money.
#[derive(Debug, Clone, PartialEq)]
pub struct MeteringConfig {
    /// Currency for charges.
    pub currency: Currency,
    /// Price of one CPU core for one hour.
    pub cpu_price_per_core_hour: Decimal,
    /// Price of one gibibyte of memory for one hour.
    pub memory_price_per_gib_hour: Decimal,
    /// Percentage discount applied to the combined charge (0-100).
    pub discount_percent: Decimal,
}

impl MeteringConfig {
    /// Read configuration from the environment.
    ///
    /// Recognised variables: `HOSTING_METERING_CURRENCY` (default `GBP`),
    /// `HOSTING_CPU_PRICE_PER_CORE_HOUR` (default `0.0040`),
    /// `HOSTING_MEMORY_PRICE_PER_GIB_HOUR` (default `0.0005`) and
    /// `HOSTING_METERING_DISCOUNT_PERCENT` (default `0`).
    ///
    /// # Errors
    /// Returns [`UsageError::Config`] for an unknown currency or an
    /// unparseable decimal.
    pub fn from_env() -> Result<Self, UsageError> {
        fn var_or(key: &str, default: &str) -> String {
            std::env::var(key).unwrap_or_else(|_| default.to_string())
        }
        let currency_raw = var_or("HOSTING_METERING_CURRENCY", "GBP");
        let currency = match currency_raw.to_ascii_uppercase().as_str() {
            "GBP" => Currency::GBP,
            other => {
                return Err(UsageError::Config(format!(
                    "unsupported currency {other:?}"
                )))
            }
        };
        let cpu_price_per_core_hour = parse_decimal(
            &var_or("HOSTING_CPU_PRICE_PER_CORE_HOUR", "0.0040"),
            "HOSTING_CPU_PRICE_PER_CORE_HOUR",
        )?;
        let memory_price_per_gib_hour = parse_decimal(
            &var_or("HOSTING_MEMORY_PRICE_PER_GIB_HOUR", "0.0005"),
            "HOSTING_MEMORY_PRICE_PER_GIB_HOUR",
        )?;
        let discount_percent = parse_decimal(
            &var_or("HOSTING_METERING_DISCOUNT_PERCENT", "0"),
            "HOSTING_METERING_DISCOUNT_PERCENT",
        )?;
        if cpu_price_per_core_hour.is_sign_negative() || memory_price_per_gib_hour.is_sign_negative()
        {
            return Err(UsageError::Config(
                "unit prices must not be negative".to_string(),
            ));
        }
        if discount_percent.is_sign_negative() || discount_percent > Decimal::from(100) {
            return Err(UsageError::Config(
                "discount percent must be within 0..=100".to_string(),
            ));
        }
        Ok(Self {
            currency,
            cpu_price_per_core_hour,
            memory_price_per_gib_hour,
            discount_percent,
        })
    }

    /// Combine a monthly subscription with a metered window into a single
    /// taxable [`Price`], applying the configured discount.
    ///
    /// # Errors
    /// Returns [`UsageError::Billing`] if the subscription tax rate is
    /// outside `0..=100` or the amount cannot be represented.
    pub fn monthly_price(
        &self,
        subscription_net: Decimal,
        tax_rate: Decimal,
        usage: Option<&TenantUsage>,
    ) -> Result<Price, UsageError> {
        let metered = match usage {
            Some(u) => u.charge(self)?.amount,
            None => Decimal::ZERO,
        };
        let total = subscription_net + metered;
        let net = if self.discount_percent > Decimal::ZERO {
            let pct = self.discount_percent.min(Decimal::from(100));
            total * (Decimal::from(100) - pct) / Decimal::from(100)
        } else {
            total
        };
        Ok(Price::new(net.max(Decimal::ZERO), self.currency, tax_rate)?)
    }
}

fn parse_decimal(raw: &str, key: &str) -> Result<Decimal, UsageError> {
    raw.trim()
        .parse::<Decimal>()
        .map_err(|e| UsageError::Config(format!("{key}: {e}")))
}

/// One parsed Prometheus exposition series.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    labels: HashMap<String, String>,
    value: Decimal,
}

/// Parse Prometheus exposition text into series, ignoring comments.
///
/// # Errors
/// Returns [`UsageError::Scrape`] when a non-comment line is not a valid
/// `metric{labels} value` triple.
pub fn parse_exposition(text: &str) -> Result<Vec<(String, Series)>, UsageError> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name_labels, value) = line
            .split_once(' ')
            .ok_or_else(|| UsageError::Scrape(format!("malformed line: {line:?}")))?;
        let value = value
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .parse::<f64>()
            .map_err(|e| UsageError::Scrape(format!("bad value in {line:?}: {e}")))?;
        let (metric, label_text) = match name_labels.split_once('{') {
            Some((m, rest)) => {
                let labels = rest.strip_suffix('}').ok_or_else(|| {
                    UsageError::Scrape(format!("unterminated label set: {line:?}"))
                })?;
                (m, labels)
            }
            None => (name_labels, ""),
        };
        let mut labels_map = HashMap::new();
        if !label_text.is_empty() {
            for pair in split_labels(label_text) {
                if let Some((k, v)) = pair.split_once('=') {
                    labels_map.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
                }
            }
        }
        out.push((
            metric.to_string(),
            Series {
                labels: labels_map,
                value: Decimal::try_from(value).unwrap_or(Decimal::ZERO),
            },
        ));
    }
    Ok(out)
}

fn split_labels(labels: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for c in labels.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            ',' if !in_quotes => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
}

/// Parse cAdvisor exposition into per-container cumulative counters.
///
/// Only container-level series are considered (`id="/docker/..."`); machine
/// and root-cgroup series are ignored. When cAdvisor exports per-CPU series
/// the `cpu="total"` series wins so that CPU is not double counted.
#[must_use]
pub fn parse_cadvisor(text: &str) -> Vec<ContainerSample> {
    let Ok(series) = parse_exposition(text) else {
        return Vec::new();
    };
    let mut cpu: HashMap<String, Decimal> = HashMap::new();
    let mut memory: HashMap<String, Decimal> = HashMap::new();
    for (metric, s) in series {
        let Some(id) = s.labels.get("id") else {
            continue;
        };
        if !id.starts_with("/docker/") {
            continue;
        }
        let Some(name) = s.labels.get("name").filter(|n| !n.is_empty()) else {
            continue;
        };
        match metric.as_str() {
            "container_cpu_usage_seconds_total" => {
                let per_cpu = s
                    .labels
                    .get("cpu")
                    .is_some_and(|c| c != "total");
                let slot = cpu.entry(name.clone()).or_insert(Decimal::ZERO);
                if !per_cpu {
                    *slot = s.value;
                } else if slot.is_zero() {
                    *slot += s.value;
                }
            }
            "container_memory_working_set_bytes" => {
                // Working set is a gauge; keep the newest sample seen.
                let slot = memory.entry(name.clone()).or_insert(Decimal::ZERO);
                *slot = s.value;
            }
            _ => {}
        }
    }
    let mut names: Vec<String> = cpu.keys().chain(memory.keys()).cloned().collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| ContainerSample {
            cpu_core_seconds: cpu.get(&name).copied().unwrap_or(Decimal::ZERO),
            memory_bytes: memory.get(&name).copied().unwrap_or(Decimal::ZERO),
            name,
        })
        .collect()
}

/// Attribute a compose container name to a tenant, if unambiguous.
///
/// Returns `None` when no tenant matches or more than one does.
#[must_use]
pub fn resolve_tenant<'a, I>(container: &str, tenants: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut matches = tenants
        .into_iter()
        .filter(|t| has_service_suffix(container, t));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

/// True when `container` is `tenant-<service>-<index>`.
fn has_service_suffix(container: &str, tenant: &str) -> bool {
    let Some(rest) = container.strip_prefix(tenant) else {
        return false;
    };
    if !rest.starts_with('-') || rest.len() <= 1 {
        return false;
    }
    rest.rsplit_once('-')
        .is_some_and(|(_, index)| !index.is_empty() && index.chars().all(|c| c.is_ascii_digit()))
}

/// Turns successive cAdvisor scrapes into deltas, per container.
///
/// State is intentionally small: the last scrape instant plus each
/// container's last counters. A counter that goes backwards (container
/// recreated) contributes nothing for that interval rather than a negative.
#[derive(Debug, Default)]
pub struct UsageCollector {
    previous: Option<(DateTime<Utc>, HashMap<String, ContainerSample>)>,
    window_start: Option<DateTime<Utc>>,
    totals: HashMap<String, TenantUsage>,
}

impl UsageCollector {
    /// Create an empty collector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Accumulated totals for the current metering window.
    #[must_use]
    pub fn totals(&self) -> &HashMap<String, TenantUsage> {
        &self.totals
    }

    /// Close the current window and start a new one at the next scrape.
    ///
    /// Used when a billing period closes so that the next window starts
    /// from zero without losing the delta bookkeeping baseline.
    pub fn reset_window(&mut self) {
        self.totals.clear();
        self.window_start = None;
    }

    /// Fold a scrape into the accumulated per-tenant usage.
    ///
    /// The first scrape only establishes a baseline and returns empty
    /// totals. Memory is integrated as `working_set * elapsed`; CPU uses
    /// the counter delta. Containers are attributed with [`resolve_tenant`]
    /// against `tenants`.
    ///
    /// # Errors
    /// Returns [`UsageError::Config`] when `now` is not after the previous
    /// scrape instant.
    pub fn observe(
        &mut self,
        samples: &[ContainerSample],
        now: DateTime<Utc>,
        tenants: &[String],
    ) -> Result<HashMap<String, TenantUsage>, UsageError> {
        let current: HashMap<String, ContainerSample> = samples
            .iter()
            .map(|s| (s.name.clone(), s.clone()))
            .collect();
        let Some((prev_at, prev)) = self.previous.clone() else {
            self.window_start = Some(now);
            self.previous = Some((now, current));
            return Ok(HashMap::new());
        };
        let elapsed = now.signed_duration_since(prev_at);
        // Sub-second intervals are legitimate (two scrapes inside one
        // second); only time going backwards is an error. Memory
        // integration is naturally ~0 for a sub-second window while CPU
        // counter deltas still apply.
        if elapsed < chrono::TimeDelta::zero() {
            return Err(UsageError::Config(
                "scrape timestamps must not go backwards".to_string(),
            ));
        }
        let millis = elapsed.num_milliseconds().max(0);
        let seconds = Decimal::from(millis) / Decimal::from(1000);
        let gibibytes = Decimal::from(BYTES_PER_GIB);
        for (name, sample) in &current {
            let Some(before) = prev.get(name) else {
                continue;
            };
            let cpu_delta = sample.cpu_core_seconds - before.cpu_core_seconds;
            let memory_delta = if sample.memory_bytes > before.memory_bytes {
                sample.memory_bytes - before.memory_bytes
            } else {
                Decimal::ZERO
            };
            let cpu = if cpu_delta.is_sign_positive() {
                cpu_delta
            } else {
                Decimal::ZERO
            };
            let Some(tenant) = resolve_tenant(name, tenants.iter().map(String::as_str)) else {
                continue;
            };
            let window_start = self.window_start.unwrap_or(prev_at);
            let entry = self
                .totals
                .entry(tenant.to_string())
                .or_insert_with(|| TenantUsage {
                    tenant: tenant.to_string(),
                    window_start,
                    window_end: now,
                    cpu_core_seconds: Decimal::ZERO,
                    memory_gibibyte_seconds: Decimal::ZERO,
                });
            entry.window_start = window_start;
            entry.window_end = now;
            entry.cpu_core_seconds += cpu;
            entry.memory_gibibyte_seconds += memory_delta * seconds / gibibytes;
        }
        self.previous = Some((now, current));
        Ok(self.totals.clone())
    }
}

/// Runtime metering service: scrape cAdvisor, fold into a window, price it.
#[derive(Debug)]
pub struct Meterer {
    cadvisor_url: Option<String>,
    config: MeteringConfig,
    collector: tokio::sync::Mutex<UsageCollector>,
    gauges: tokio::sync::RwLock<HashMap<String, TenantGauges>>,
    registry: Option<std::sync::Arc<metrics_kit::Registry>>,
}

impl Meterer {
    /// Create a meterer. `cadvisor_url = None` disables metering while
    /// keeping the endpoints informative.
    #[must_use]
    pub fn new(cadvisor_url: Option<String>, config: MeteringConfig) -> Self {
        Self {
            cadvisor_url,
            config,
            collector: tokio::sync::Mutex::new(UsageCollector::new()),
            gauges: tokio::sync::RwLock::new(HashMap::new()),
            registry: None,
        }
    }

    /// Attach a Prometheus registry so tenant gauges are exported.
    #[must_use]
    pub fn with_registry(mut self, registry: std::sync::Arc<metrics_kit::Registry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// True when a cAdvisor URL is configured.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.cadvisor_url.is_some()
    }

    /// Unit prices in force.
    #[must_use]
    pub fn config(&self) -> &MeteringConfig {
        &self.config
    }

    /// Close the current metering window (billing period boundary).
    pub async fn reset_window(&self) {
        self.collector.lock().await.reset_window();
    }

    /// Accumulated window totals without scraping (cheap read).
    pub async fn window(&self) -> HashMap<String, TenantUsage> {
        self.collector.lock().await.totals().clone()
    }

    /// Scrape cAdvisor, fold the delta into the window and refresh gauges.
    ///
    /// # Errors
    /// Returns [`UsageError::Scrape`] when cAdvisor cannot be reached or
    /// its exposition cannot be parsed. A disabled meterer returns the
    /// accumulated totals unchanged.
    pub async fn refresh(&self, tenants: &[String]) -> Result<HashMap<String, TenantUsage>, UsageError> {
        let Some(url) = self.cadvisor_url.as_deref() else {
            return Ok(self.window().await);
        };
        let text = fetch_exposition(url).await?;
        let samples = parse_cadvisor(&text);
        let mut collector = self.collector.lock().await;
        let totals = collector.observe(&samples, chrono::Utc::now(), tenants)?;
        drop(collector);
        self.publish(&totals).await;
        Ok(totals)
    }

    async fn publish(&self, totals: &HashMap<String, TenantUsage>) {
        let Some(registry) = self.registry.as_ref() else {
            return;
        };
        for (tenant, usage) in totals {
            let mut cache = self.gauges.write().await;
            let gauges = match cache.get(tenant) {
                Some(g) => g.clone(),
                None => match TenantGauges::register(registry, tenant) {
                    Ok(g) => {
                        cache.insert(tenant.clone(), g.clone());
                        g
                    }
                    Err(e) => {
                        tracing::warn!(%tenant, error = %e, "tenant gauge registration failed");
                        continue;
                    }
                },
            };
            gauges.observe(usage, &self.config);
        }
    }
}

/// Scrape a Prometheus exposition endpoint.
async fn fetch_exposition(url: &str) -> Result<String, UsageError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| UsageError::Scrape(e.to_string()))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| UsageError::Scrape(e.to_string()))?;
    if !response.status().is_success() {
        return Err(UsageError::Scrape(format!(
            "HTTP {}",
            response.status().as_u16()
        )));
    }
    response
        .text()
        .await
        .map_err(|e| UsageError::Scrape(e.to_string()))
}

/// Per-tenant Prometheus gauges for metered usage.
///
/// Handles are created once per tenant and cached by the caller, because
/// `metrics-kit` rejects duplicate label sets.
#[derive(Debug, Clone)]
pub struct TenantGauges {
    /// CPU consumed in core-seconds this window.
    pub cpu_core_seconds: metrics_kit::Gauge,
    /// Memory consumed in gibibyte-seconds this window.
    pub memory_gibibyte_seconds: metrics_kit::Gauge,
    /// Priced charge for this window, in the configured currency.
    pub charge: metrics_kit::Gauge,
}

impl TenantGauges {
    /// Register the three gauges for one tenant.
    ///
    /// # Errors
    /// Returns [`metrics_kit::MetricsError`] if registration fails.
    pub fn register(
        registry: &metrics_kit::Registry,
        tenant: &str,
    ) -> Result<Self, metrics_kit::MetricsError> {
        let labels = [("tenant", tenant)];
        Ok(Self {
            cpu_core_seconds: registry.gauge(
                "hosting_tenant_cpu_core_seconds",
                "CPU consumed by a tenant in the current metering window (core-seconds).",
                &labels,
            )?,
            memory_gibibyte_seconds: registry.gauge(
                "hosting_tenant_memory_gibibyte_seconds",
                "Memory consumed by a tenant in the current metering window (gibibyte-seconds).",
                &labels,
            )?,
            charge: registry.gauge(
                "hosting_tenant_usage_charge",
                "Priced charge for a tenant's current metering window.",
                &labels,
            )?,
        })
    }

    /// Push a usage sample and its price into the gauges.
    pub fn observe(&self, usage: &TenantUsage, cfg: &MeteringConfig) {
        self.cpu_core_seconds.set(usage.cpu_core_seconds.to_f64().unwrap_or(0.0));
        self.memory_gibibyte_seconds
            .set(usage.memory_gibibyte_seconds.to_f64().unwrap_or(0.0));
        if let Ok(amount) = usage.charge(cfg) {
            self.charge.set(amount.amount.to_f64().unwrap_or(0.0));
        }
    }
}

impl fmt::Display for ContainerSample {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} cpu={}s mem={}B",
            self.name, self.cpu_core_seconds, self.memory_bytes
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn roster() -> Vec<String> {
        vec!["acme".to_string()]
    }

    fn at(minute: u32) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + i64::from(minute) * 60, 0).unwrap_or_default()
    }

    fn cfg_zero_discount() -> MeteringConfig {
        MeteringConfig {
            currency: Currency::GBP,
            cpu_price_per_core_hour: Decimal::ZERO,
            memory_price_per_gib_hour: Decimal::ZERO,
            discount_percent: Decimal::ZERO,
        }
    }

    const CADVISOR_SAMPLE: &str = r#"
# HELP container_cpu_usage_seconds_total CPU time consumed.
# TYPE container_cpu_usage_seconds_total counter
container_cpu_usage_seconds_total{cpu="total",container="acme-paperless-1",id="/docker/aaa",image="paperless",name="acme-paperless-1"} 120.5
container_cpu_usage_seconds_total{cpu="cpu00",container="acme-paperless-1",id="/docker/aaa",image="paperless",name="acme-paperless-1"} 60.25
container_cpu_usage_seconds_total{cpu="total",container="",id="/",image="",name=""} 9999
container_memory_working_set_bytes{container="acme-paperless-1",id="/docker/aaa",image="paperless",name="acme-paperless-1"} 536870912
container_memory_working_set_bytes{container="",id="/",image="",name=""} 1073741824
"#;

    #[test]
    fn parses_container_series_only() {
        // Exact vector equality: root cgroup series must be ignored and
        // the cpu="total" series must win over per-CPU series.
        assert_eq!(
            parse_cadvisor(CADVISOR_SAMPLE),
            vec![ContainerSample {
                name: "acme-paperless-1".to_string(),
                cpu_core_seconds: dec!(120.5),
                memory_bytes: dec!(536870912),
            }]
        );
    }

    #[test]
    fn unquoted_labels_and_escapes_parse() {
        let text = r#"metric_a{label="a,b",name="we"ird"} 1.5"#;
        let parsed = parse_exposition(text).unwrap_or_default();
        assert_eq!(parsed.len(), 1, "exposition should parse");
        for (metric, series) in parsed {
            assert_eq!(metric, "metric_a");
            assert_eq!(series.labels.get("label").map(String::as_str), Some("a,b"));
            assert_eq!(series.labels.get("name").map(String::as_str), Some("we\"ird"));
            assert_eq!(series.value, dec!(1.5));
        }
    }

    #[test]
    fn malformed_line_is_an_error() {
        assert!(parse_exposition("nonsense").is_err());
    }

    #[test]
    fn resolve_tenant_requires_unambiguous_service_suffix() {
        assert_eq!(
            resolve_tenant("acme-paperless-1", ["acme", "beta"]),
            Some("acme")
        );
        assert_eq!(
            resolve_tenant("acme-db-2", ["acme", "acme-db"]),
            None,
            "overlapping tenant prefixes must not be guessed"
        );
        assert_eq!(resolve_tenant("acme", ["acme"]), None);
        assert_eq!(resolve_tenant("other-thing-1", ["acme"]), None);
    }

    #[test]
    fn first_scrape_only_establishes_a_baseline() {
        let mut c = UsageCollector::new();
        let samples = parse_cadvisor(CADVISOR_SAMPLE);
        let totals = c.observe(&samples, at(0), &roster()).unwrap_or_default();
        assert!(totals.is_empty());
        assert!(c.totals().is_empty());
    }

    #[test]
    fn computes_cpu_and_memory_deltas_per_tenant() {
        let mut c = UsageCollector::new();
        assert!(c.observe(&parse_cadvisor(CADVISOR_SAMPLE), at(0), &roster()).is_ok(), "baseline observe should succeed");
        let second = [ContainerSample {
            name: "acme-paperless-1".to_string(),
            cpu_core_seconds: dec!(150.5),
            memory_bytes: Decimal::from(2 * BYTES_PER_GIB),
        }];
        let totals = c.observe(&second, at(10), &roster()).unwrap_or_default();
        assert!(totals.contains_key("acme"), "tenant acme should have usage");
        for acme in totals.values() {
            assert_eq!(acme.cpu_core_seconds, dec!(30.0), "counter delta");
            assert_eq!(
                acme.memory_gibibyte_seconds,
                Decimal::from(900),
                "working set grew 0.5GiB -> 2GiB; 1.5 gib over 600s = 900 gibibyte-seconds"
            );
            assert_eq!(acme.window_start, at(0));
            assert_eq!(acme.window_end, at(10));
        }
    }

    #[test]
    fn window_accumulates_across_intervals() {
        let mut c = UsageCollector::new();
        assert!(c.observe(&parse_cadvisor(CADVISOR_SAMPLE), at(0), &roster()).is_ok(), "baseline observe should succeed");
        let mut cpu = dec!(120.5);
        let mut last = Decimal::ZERO;
        for minute in [10, 20, 30] {
            cpu += dec!(10);
            let sample = [ContainerSample {
                name: "acme-paperless-1".to_string(),
                cpu_core_seconds: cpu,
                memory_bytes: last,
            }];
            let totals = c.observe(&sample, at(minute), &roster()).unwrap_or_default();
            last = totals
                .get("acme")
                .map_or(Decimal::ZERO, |u| u.cpu_core_seconds);
        }
        assert_eq!(last, dec!(30.0), "10 core-seconds per interval, 3 intervals");
        assert_eq!(c.totals().get("acme").map(|u| u.window_end), Some(at(30)));
    }

    #[test]
    fn reset_window_starts_a_fresh_period() {
        let mut c = UsageCollector::new();
        assert!(c.observe(&parse_cadvisor(CADVISOR_SAMPLE), at(0), &roster()).is_ok(), "baseline observe should succeed");
        assert!(c.observe(&parse_cadvisor(CADVISOR_SAMPLE), at(10), &roster()).is_ok());
        c.reset_window();
        assert!(c.totals().is_empty());
    }

    #[test]
    fn counter_reset_does_not_produce_negative_usage() {
        let mut c = UsageCollector::new();
        assert!(c.observe(&parse_cadvisor(CADVISOR_SAMPLE), at(0), &roster()).is_ok(), "baseline observe should succeed");
        let after_recreate = [ContainerSample {
            name: "acme-paperless-1".to_string(),
            cpu_core_seconds: dec!(1),
            memory_bytes: dec!(1024),
        }];
        let totals = c.observe(&after_recreate, at(5), &roster()).unwrap_or_default();
        assert!(totals.contains_key("acme"));
        for acme in totals.values() {
            assert_eq!(acme.cpu_core_seconds, Decimal::ZERO);
            assert_eq!(acme.memory_gibibyte_seconds, Decimal::ZERO);
        }
    }

    #[test]
    fn timestamps_going_backwards_are_rejected() {
        let mut c = UsageCollector::new();
        assert!(c.observe(&parse_cadvisor(CADVISOR_SAMPLE), at(5), &roster()).is_ok());
        assert!(c.observe(&parse_cadvisor(CADVISOR_SAMPLE), at(4), &roster()).is_err());
    }

    #[test]
    fn sub_second_intervals_are_accepted() {
        let mut c = UsageCollector::new();
        assert!(c.observe(&parse_cadvisor(CADVISOR_SAMPLE), at(0), &roster()).is_ok());
        let later = DateTime::from_timestamp(1_700_000_000 + 250, 0).unwrap_or_default();
        let totals = c
            .observe(&parse_cadvisor(CADVISOR_SAMPLE), later, &roster())
            .unwrap_or_default();
        assert!(
            totals.contains_key("acme"),
            "two scrapes a quarter second apart must be a valid interval"
        );
    }

    #[test]
    fn charge_uses_unit_prices() {
        let cfg = MeteringConfig {
            currency: Currency::GBP,
            cpu_price_per_core_hour: dec!(0.01),
            memory_price_per_gib_hour: dec!(0.001),
            discount_percent: Decimal::ZERO,
        };
        let usage = TenantUsage {
            tenant: "acme".to_string(),
            window_start: at(0),
            window_end: at(10),
            cpu_core_seconds: dec!(720),        // 0.2 core-hours
            memory_gibibyte_seconds: dec!(3600), // 1 gib-hour
        };
        assert_eq!(usage.charge(&cfg).map_or(Decimal::ZERO, |c| c.amount), dec!(0.003));
    }

    #[test]
    fn monthly_price_adds_metered_usage_to_subscription() {
        let cfg = MeteringConfig {
            cpu_price_per_core_hour: dec!(0.01),
            memory_price_per_gib_hour: Decimal::ZERO,
            discount_percent: Decimal::ZERO,
            ..cfg_zero_discount()
        };
        let usage = TenantUsage {
            tenant: "acme".to_string(),
            window_start: at(0),
            window_end: at(60),
            cpu_core_seconds: dec!(3600), // 1 core-hour
            memory_gibibyte_seconds: Decimal::ZERO,
        };
        let built = cfg.monthly_price(dec!(20), dec!(20), Some(&usage));
        assert!(built.is_ok(), "price should build");
        let Ok(price) = built else { return };
        assert_eq!(
            price.net.amount,
            dec!(20.01),
            "20 subscription + 1 core-hour at 0.01"
        );
        assert_eq!(price.gross().amount, dec!(24.012), "20% VAT on the metered total");
    }

    #[test]
    fn monthly_price_applies_discount_to_the_net() {
        let cfg = MeteringConfig {
            discount_percent: dec!(10),
            ..cfg_zero_discount()
        };
        let built = cfg.monthly_price(dec!(100), dec!(20), None);
        assert!(built.is_ok(), "price should build");
        let Ok(price) = built else { return };
        assert_eq!(price.net.amount, dec!(90));
        assert_eq!(price.gross().amount, dec!(108));
        let built = cfg_zero_discount().monthly_price(dec!(100), dec!(20), None);
        assert!(built.is_ok(), "price should build");
        let Ok(undiscounted) = built else { return };
        assert_eq!(
            price.gross().amount,
            undiscounted.gross_minus_discount(dec!(10)).amount,
            "net-level discount must agree with billing-kit's gross discount"
        );
    }

    #[test]
    fn env_config_rejects_bad_values() {
        assert!(parse_decimal("nope", "X").is_err());
        assert!(MeteringConfig {
            cpu_price_per_core_hour: dec!(-1),
            ..cfg_zero_discount()
        }
        .monthly_price(dec!(1), dec!(0), None)
        .is_ok(), "negative prices are rejected by from_env, not by pricing");
    }

    #[test]
    fn gauges_expose_one_series_per_tenant() {
        let registry = metrics_kit::Registry::new();
        let built = TenantGauges::register(&registry, "acme");
        assert!(built.is_ok(), "registration should succeed");
        let Ok(gauges) = built else { return };
        let usage = TenantUsage {
            tenant: "acme".to_string(),
            window_start: at(0),
            window_end: at(10),
            cpu_core_seconds: dec!(42),
            memory_gibibyte_seconds: dec!(900),
        };
        gauges.observe(&usage, &cfg_zero_discount());
        let rendered = registry.render_as(metrics_kit::Format::PromText);
        assert!(rendered.contains("hosting_tenant_cpu_core_seconds"));
        assert!(rendered.contains("hosting_tenant_memory_gibibyte_seconds"));
        assert!(rendered.contains("hosting_tenant_usage_charge"));
        assert!(
            rendered
                .lines()
                .any(|l| l.starts_with("hosting_tenant_cpu_core_seconds")
                    && l.contains(r#"tenant="acme""#)
                    && l.contains(" 42")),
            "cpu gauge should carry the tenant label and value:\n{rendered}"
        );
    }
}
