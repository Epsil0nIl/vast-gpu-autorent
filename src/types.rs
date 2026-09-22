use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct VastOffer {
    pub id: u64,
    pub machine_id: Option<u64>,
    pub gpu_name: Option<String>,
    pub geolocation: Option<String>,
    pub gpu_ram: u64,
    pub num_gpus: u32,
    pub direct_port_count: Option<u32>,
    /// Free disk on the machine, in GB. Absent when the API omits it.
    #[serde(default)]
    pub disk_space: Option<f64>,
    pub dph_total: f64,
    pub reliability2: Option<f64>,
    pub verification: Option<String>,
    pub rented: bool,
    pub rentable: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VastVolumeOffer {
    pub id: u64,
    pub machine_id: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VastVolume {
    pub id: u64,
    pub machine_id: u64,
}

#[derive(Clone, Serialize)]
pub struct VastCreateInstanceRequest {
    pub image: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_hash_id: Option<String>,
    pub label: String,
    pub disk: f64,
    pub runtype: String,
    pub target_state: String,
    pub env: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_unavail: Option<bool>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub onstart: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_info: Option<VastVolumeInfo>,
}

impl fmt::Debug for VastCreateInstanceRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VastCreateInstanceRequest")
            .field("image", &self.image)
            .field("template_hash_id", &self.template_hash_id)
            .field("label", &self.label)
            .field("disk", &self.disk)
            .field("runtype", &self.runtype)
            .field("target_state", &self.target_state)
            .field("env", &redacted_env(&self.env))
            .field("cancel_unavail", &self.cancel_unavail)
            .field("onstart", &redact_if_present(&self.onstart))
            .field("price", &self.price)
            .field("volume_info", &self.volume_info)
            .finish()
    }
}

pub(crate) fn redacted_env(values: &HashMap<String, String>) -> HashMap<String, String> {
    values
        .iter()
        .map(|(key, value)| (key.clone(), redact_if_present(value).to_string()))
        .collect()
}

pub(crate) fn redact_if_present(value: &str) -> &str {
    if value.trim().is_empty() {
        ""
    } else {
        "[redacted]"
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VastVolumeInfo {
    pub mount_path: String,
    pub volume_id: u64,
    pub create_new: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VastInstance {
    pub id: u64,
    pub actual_status: Option<String>,
    pub cur_state: Option<String>,
    pub ssh_host: Option<String>,
    pub ssh_port: Option<u16>,
    #[serde(default)]
    pub ports: Value,
    pub public_ipaddr: Option<String>,
    pub geolocation: Option<String>,
    pub dph_total: Option<f64>,
    pub reliability2: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VastLogsResponse {
    pub success: bool,
    pub result_url: Option<String>,
    pub msg: Option<String>,
}
