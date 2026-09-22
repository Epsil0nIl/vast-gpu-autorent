use std::{collections::HashMap, fmt, fs, path::Path};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use url::Url;

use crate::types::redacted_env;

const TRUSTED_API_HOST: &str = "console.vast.ai";

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_profile_name")]
    pub default_profile: String,
    pub vast: VastConfig,
    #[serde(default)]
    pub profiles: HashMap<String, WorkerProfileConfig>,
}

#[derive(Clone, Deserialize)]
pub struct VastConfig {
    #[serde(default = "default_api_base_url")]
    pub api_base_url: String,
    #[serde(default)]
    pub api_key: String,
    pub max_hourly_price_usd: f64,
    #[serde(default = "default_search_limit")]
    pub search_limit: usize,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_bootstrap_timeout_s")]
    pub bootstrap_timeout_s: u64,
    #[serde(default = "default_verified_only")]
    pub verified_only: bool,
    #[serde(default = "default_min_reliability")]
    pub min_reliability: f64,
    #[serde(default = "default_log_tail_lines")]
    pub log_tail_lines: usize,
    #[serde(default = "default_state_dir")]
    pub state_dir: String,
    #[serde(default)]
    pub callback_base_url: String,
    #[serde(default)]
    pub bootstrap_token: String,
    /// Checked on the next rent, status, destroy, or logs. Not a background timer.
    #[serde(default)]
    pub max_runtime_hours: Option<f64>,
}

impl fmt::Debug for VastConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VastConfig")
            .field("api_base_url", &self.api_base_url)
            .field("api_key", &redact_secret(&self.api_key))
            .field("max_hourly_price_usd", &self.max_hourly_price_usd)
            .field("search_limit", &self.search_limit)
            .field("poll_interval_ms", &self.poll_interval_ms)
            .field("bootstrap_timeout_s", &self.bootstrap_timeout_s)
            .field("verified_only", &self.verified_only)
            .field("min_reliability", &self.min_reliability)
            .field("log_tail_lines", &self.log_tail_lines)
            .field("state_dir", &self.state_dir)
            .field("callback_base_url", &redact_url(&self.callback_base_url))
            .field("bootstrap_token", &redact_secret(&self.bootstrap_token))
            .field("max_runtime_hours", &self.max_runtime_hours)
            .finish()
    }
}

fn redact_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match Url::parse(trimmed) {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.to_string()
        }
        Err(_) => "[redacted]".to_string(),
    }
}

fn redact_secret(value: &str) -> &'static str {
    if value.trim().is_empty() {
        ""
    } else {
        "[redacted]"
    }
}

#[derive(Clone, Deserialize)]
pub struct WorkerProfileConfig {
    pub min_gpu_ram_gb: u64,
    #[serde(default = "default_gpu_count")]
    pub gpu_count: u32,
    #[serde(default = "default_min_reliability")]
    pub min_reliability: f64,
    #[serde(default = "default_direct_ports_required")]
    pub direct_ports_required: bool,
    #[serde(default)]
    pub gpu_names: Vec<String>,
    #[serde(default)]
    pub preferred_geolocations: Vec<String>,
    pub image: String,
    #[serde(default)]
    pub template_hash_id: String,
    pub disk_gb: f64,
    #[serde(default = "default_runtype")]
    pub runtype: String,
    #[serde(default = "default_target_state")]
    pub target_state: String,
    pub label_prefix: String,
    #[serde(default)]
    pub onstart: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub volume: Option<WorkerVolumeConfig>,
}

impl fmt::Debug for WorkerProfileConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerProfileConfig")
            .field("min_gpu_ram_gb", &self.min_gpu_ram_gb)
            .field("gpu_count", &self.gpu_count)
            .field("min_reliability", &self.min_reliability)
            .field("direct_ports_required", &self.direct_ports_required)
            .field("gpu_names", &self.gpu_names)
            .field("preferred_geolocations", &self.preferred_geolocations)
            .field("image", &self.image)
            .field("template_hash_id", &self.template_hash_id)
            .field("disk_gb", &self.disk_gb)
            .field("runtype", &self.runtype)
            .field("target_state", &self.target_state)
            .field("label_prefix", &self.label_prefix)
            .field("onstart", &crate::types::redact_if_present(&self.onstart))
            .field("env", &redacted_env(&self.env))
            .field("ports", &self.ports)
            .field("volume", &self.volume)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkerVolumeConfig {
    pub mount_path: String,
    #[serde(default)]
    pub volume_id: Option<u64>,
    #[serde(default)]
    pub machine_id: Option<u64>,
    #[serde(default)]
    pub create_new: bool,
    #[serde(default)]
    pub size_gb: Option<u64>,
    #[serde(default = "default_cache_subdir")]
    pub cache_subdir: String,
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut config: Self =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        if let Ok(key) = std::env::var("VAST_API_KEY") {
            if !key.trim().is_empty() {
                config.vast.api_key = key;
            }
        }
        if let Ok(token) = std::env::var("RENT_BOOTSTRAP_TOKEN") {
            if !token.trim().is_empty() {
                config.vast.bootstrap_token = token;
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure_trusted_api_base(&self.vast.api_base_url)?;
        if !self.vast.max_hourly_price_usd.is_finite() || self.vast.max_hourly_price_usd <= 0.0 {
            bail!("vast.max_hourly_price_usd must be a positive number");
        }
        if let Some(hours) = self.vast.max_runtime_hours {
            if !hours.is_finite() || hours <= 0.0 {
                bail!("vast.max_runtime_hours must be a positive number of hours when set");
            }
        }
        if self.profiles.is_empty() {
            bail!("configure at least one profile");
        }
        for (name, profile) in &self.profiles {
            if !profile.disk_gb.is_finite() || profile.disk_gb <= 0.0 {
                bail!("profile {name} disk_gb must be a positive number of GB");
            }
        }
        Ok(())
    }

    pub fn profile(&self, name: &str) -> Result<&WorkerProfileConfig> {
        self.profiles.get(name).with_context(|| {
            format!(
                "unknown profile {name}. configured: {}",
                self.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    }

    pub fn require_api_key(&self) -> Result<()> {
        if self.vast.api_key.trim().is_empty() {
            anyhow::bail!("set VAST_API_KEY or vast.api_key before calling Vast");
        }
        Ok(())
    }
}

/// The bearer token is sent to this URL. Only the official Vast host over HTTPS is accepted.
pub fn ensure_trusted_api_base(raw: &str) -> Result<()> {
    let url = Url::parse(raw.trim()).context("vast.api_base_url is not a URL")?;
    if url.scheme() != "https" {
        bail!("vast.api_base_url must use https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("vast.api_base_url must not contain credentials");
    }
    if url.host_str() != Some(TRUSTED_API_HOST) {
        bail!("vast.api_base_url host must be {TRUSTED_API_HOST}");
    }
    if let Some(port) = url.port() {
        if port != 443 {
            bail!("vast.api_base_url must use port 443");
        }
    }
    Ok(())
}

fn default_profile_name() -> String {
    "gpu".to_string()
}

fn default_api_base_url() -> String {
    "https://console.vast.ai/api/v0".to_string()
}

fn default_search_limit() -> usize {
    50
}

fn default_poll_interval_ms() -> u64 {
    5_000
}

fn default_bootstrap_timeout_s() -> u64 {
    600
}

fn default_verified_only() -> bool {
    true
}

fn default_min_reliability() -> f64 {
    0.95
}

fn default_log_tail_lines() -> usize {
    500
}

fn default_state_dir() -> String {
    "./state".to_string()
}

fn default_gpu_count() -> u32 {
    1
}

fn default_direct_ports_required() -> bool {
    true
}

fn default_runtype() -> String {
    "ssh_direct".to_string()
}

fn default_target_state() -> String {
    "running".to_string()
}

fn default_cache_subdir() -> String {
    "model-cache".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses_without_secrets() {
        let text = include_str!("../config.example.toml");
        let config: AppConfig = toml::from_str(text).unwrap();
        assert!(config.vast.api_key.trim().is_empty());
        assert!(config.vast.bootstrap_token.trim().is_empty());
        assert!(config.vast.callback_base_url.trim().is_empty());
        assert!(config.profile("gpu").is_ok());
        assert!(config.profiles["gpu"].volume.is_none());
        let lower = text.to_ascii_lowercase();
        assert!(!lower.contains("postgresql://"));
        assert!(!lower.contains("bearer "));
        assert!(!lower.contains("ssh-"));
        config.validate().unwrap();
    }

    #[test]
    fn debug_output_redacts_credentials() {
        let mut config: AppConfig = toml::from_str(include_str!("../config.example.toml")).unwrap();
        config.vast.api_key = "vast-live-key".to_string();
        config.vast.bootstrap_token = "launch-token".to_string();
        config.vast.callback_base_url =
            "https://user:secret@callback.example/hook?token=abc".to_string();
        let profile = config.profiles.get_mut("gpu").unwrap();
        profile.env.insert(
            "DATABASE_URL".to_string(),
            "postgres://user:secret@db/app".to_string(),
        );
        profile.onstart = "curl -H 'token: launch-token' https://example".to_string();
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("vast-live-key"));
        assert!(!rendered.contains("launch-token"));
        assert!(!rendered.contains("postgres://user:secret"));
        assert!(!rendered.contains("user:secret"));
        assert!(!rendered.contains("token=abc"));
        assert!(rendered.contains("[redacted]"));
    }

    #[test]
    fn api_base_rejects_untrusted_hosts() {
        assert!(ensure_trusted_api_base("https://console.vast.ai/api/v0").is_ok());
        assert!(ensure_trusted_api_base("http://console.vast.ai/api/v0").is_err());
        assert!(ensure_trusted_api_base("https://user:secret@console.vast.ai/api/v0").is_err());
        assert!(ensure_trusted_api_base("https://console.vast.ai.example/api/v0").is_err());
        assert!(ensure_trusted_api_base("https://evil.example/api/v0").is_err());
        assert!(ensure_trusted_api_base("https://console.vast.ai:8443/api/v0").is_err());
    }
}
