use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::config::{VastConfig, WorkerProfileConfig, WorkerVolumeConfig};
use crate::types::{VastCreateInstanceRequest, VastOffer, VastVolumeInfo};

pub fn search_request(
    config: &VastConfig,
    profile: &WorkerProfileConfig,
    machine_id: Option<u64>,
    geolocation: Option<&str>,
) -> Value {
    // allocated_storage is what the Vast CLI sends so dph_total includes this disk.
    // Omitting it prices the default 8 GB, then create requests profile.disk_gb.
    // inet_*_cost bandwidth is metered per GB and is not part of dph_total.
    let mut filters = json!({
        "limit": config.search_limit.max(1),
        "order": [["dph_total", "asc"]],
        "type": "ondemand",
        "allocated_storage": profile.disk_gb,
        "rentable": {"eq": true},
        "rented": {"eq": false},
        "num_gpus": {"eq": profile.gpu_count},
        "gpu_ram": {"gte": profile.min_gpu_ram_gb * 1024},
        "disk_space": {"gte": profile.disk_gb},
        "dph_total": {"lte": config.max_hourly_price_usd},
        "reliability2": {"gte": profile.min_reliability.max(config.min_reliability)},
    });
    if config.verified_only {
        filters["verified"] = json!({"eq": true});
    }
    if profile.direct_ports_required {
        filters["direct_port_count"] = json!({"gte": 1});
    }
    if !profile.gpu_names.is_empty() {
        filters["gpu_name"] = json!({"in": profile.gpu_names});
    }
    if let Some(machine_id) = machine_id {
        filters["machine_id"] = json!({"eq": machine_id});
    }
    if let Some(geolocation) = geolocation.map(str::trim).filter(|value| !value.is_empty()) {
        filters["geolocation"] = json!({"eq": geolocation});
    }
    filters
}

/// Vast's geolocation filter takes a two-letter country code.
pub fn instance_label(prefix: &str, lease_id: &str) -> String {
    format!("{prefix}-{lease_id}")
}

pub fn geolocation_api_code(preferred: &str) -> String {
    let trimmed = preferred.trim();
    let lower = trimmed.to_ascii_lowercase();
    // Check aliases before the two-letter fast path, or "UK" never becomes "GB".
    if matches!(lower.as_str(), "uk" | "united kingdom" | "great britain") {
        return "GB".to_string();
    }
    if trimmed.len() == 2 && trimmed.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return trimmed.to_ascii_uppercase();
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "germany" => "DE",
        "belgium" => "BE",
        "netherlands" | "holland" => "NL",
        "czechia" | "czech republic" => "CZ",
        "austria" => "AT",
        "switzerland" => "CH",
        "france" => "FR",
        "poland" => "PL",
        "hungary" => "HU",
        "united states" | "united states of america" | "usa" => "US",
        "canada" => "CA",
        "sweden" => "SE",
        "norway" => "NO",
        "finland" => "FI",
        "spain" => "ES",
        "italy" => "IT",
        "portugal" => "PT",
        "ireland" => "IE",
        "denmark" => "DK",
        "romania" => "RO",
        "bulgaria" => "BG",
        "greece" => "GR",
        "slovakia" => "SK",
        "slovenia" => "SI",
        "croatia" => "HR",
        "estonia" => "EE",
        "latvia" => "LV",
        "lithuania" => "LT",
        "luxembourg" => "LU",
        "iceland" => "IS",
        "japan" => "JP",
        "singapore" => "SG",
        "australia" => "AU",
        "taiwan" => "TW",
        "south korea" | "korea" => "KR",
        other => other,
    }
    .to_string()
}

pub fn geo_matches(geolocation: Option<&str>, preferred: &str) -> bool {
    let Some(geolocation) = geolocation else {
        return false;
    };
    let preferred = preferred.trim();
    if preferred.is_empty() {
        return false;
    }
    if preferred.len() == 2 && preferred.chars().all(|ch| ch.is_ascii_alphabetic()) {
        let code = preferred.to_ascii_uppercase();
        return geolocation
            .split(|ch: char| ch == ',' || ch.is_whitespace())
            .any(|part| part.eq_ignore_ascii_case(&code));
    }
    geolocation
        .to_ascii_lowercase()
        .contains(&preferred.to_ascii_lowercase())
}

pub fn offer_in_region(offer: &VastOffer, preferred: &str, api_code: &str) -> bool {
    geo_matches(offer.geolocation.as_deref(), preferred)
        || geo_matches(offer.geolocation.as_deref(), api_code)
}

/// Offers that pass the filters, narrowed to the first preferred region that has
/// any match, then ordered cheapest first. Equal price keeps the higher reliability.
pub fn ranked_offers<'a>(
    offers: &'a [VastOffer],
    config: &VastConfig,
    profile: &WorkerProfileConfig,
    machine_id: Option<u64>,
    rejected_offer_ids: &HashSet<u64>,
) -> Vec<&'a VastOffer> {
    let mut pool = preferred_geo_pool(
        filtered_offers(offers, config, profile, machine_id, rejected_offer_ids),
        &profile.preferred_geolocations,
    );
    pool.sort_by(|left, right| offer_order(left, right));
    pool
}

pub fn select_offer<'a>(
    offers: &'a [VastOffer],
    config: &VastConfig,
    profile: &WorkerProfileConfig,
    machine_id: Option<u64>,
    rejected_offer_ids: &HashSet<u64>,
) -> Option<&'a VastOffer> {
    ranked_offers(offers, config, profile, machine_id, rejected_offer_ids)
        .into_iter()
        .next()
}

pub fn build_create_request(
    config: &VastConfig,
    profile: &WorkerProfileConfig,
    profile_name: &str,
    lease_id: &str,
) -> Result<VastCreateInstanceRequest> {
    let volume_info = match profile.volume.as_ref() {
        Some(volume) => Some(build_volume_info(volume)?),
        None => None,
    };
    let mut env_values = profile.env.clone();
    env_values.insert("RENT_LEASE_ID".to_string(), lease_id.to_string());
    env_values.insert("RENT_PROFILE".to_string(), profile_name.to_string());
    if !config.callback_base_url.trim().is_empty() {
        env_values.insert(
            "RENT_CALLBACK_URL".to_string(),
            config.callback_base_url.clone(),
        );
    }
    if !config.bootstrap_token.trim().is_empty() {
        env_values.insert(
            "RENT_BOOTSTRAP_TOKEN".to_string(),
            config.bootstrap_token.clone(),
        );
    }
    if let Some(volume) = profile.volume.as_ref() {
        inject_volume_cache_env(&mut env_values, volume);
    }
    Ok(VastCreateInstanceRequest {
        image: profile.image.clone(),
        template_hash_id: Some(profile.template_hash_id.clone())
            .filter(|value| !value.trim().is_empty()),
        label: instance_label(&profile.label_prefix, lease_id),
        disk: profile.disk_gb,
        runtype: profile.runtype.clone(),
        target_state: profile.target_state.clone(),
        env: encode_env_map(&env_values, &profile.ports),
        cancel_unavail: Some(true),
        onstart: profile.onstart.clone(),
        price: None,
        volume_info,
    })
}

pub fn is_stale_offer_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let text = cause.to_string();
        text.contains("no_such_ask") || text.contains("410 Gone")
    })
}

fn filtered_offers<'a>(
    offers: &'a [VastOffer],
    config: &VastConfig,
    profile: &WorkerProfileConfig,
    machine_id: Option<u64>,
    rejected_offer_ids: &HashSet<u64>,
) -> Vec<&'a VastOffer> {
    let floor = profile.min_reliability.max(config.min_reliability);
    offers
        .iter()
        .filter(|offer| !rejected_offer_ids.contains(&offer.id))
        .filter(|offer| offer.rentable && !offer.rented)
        .filter(|offer| offer.dph_total <= config.max_hourly_price_usd)
        .filter(|offer| offer.num_gpus == profile.gpu_count)
        .filter(|offer| offer.gpu_ram >= profile.min_gpu_ram_gb * 1024)
        .filter(|offer| match offer.disk_space {
            Some(space) => space + f64::EPSILON >= profile.disk_gb,
            None => true,
        })
        .filter(|offer| offer.reliability2.unwrap_or(0.0) >= floor)
        .filter(|offer| match machine_id {
            Some(machine_id) => offer.machine_id == Some(machine_id),
            None => true,
        })
        .filter(|offer| {
            if config.verified_only {
                offer
                    .verification
                    .as_deref()
                    .map(|value| value.eq_ignore_ascii_case("verified"))
                    .unwrap_or(false)
            } else {
                true
            }
        })
        .filter(|offer| {
            if profile.direct_ports_required {
                offer.direct_port_count.unwrap_or_default() > 0
            } else {
                true
            }
        })
        .filter(|offer| gpu_name_matches(offer, profile))
        .collect()
}

fn gpu_name_matches(offer: &VastOffer, profile: &WorkerProfileConfig) -> bool {
    if profile.gpu_names.is_empty() {
        return true;
    }
    profile.gpu_names.iter().any(|name| {
        offer
            .gpu_name
            .as_deref()
            .map(|value| value.eq_ignore_ascii_case(name))
            .unwrap_or(false)
    })
}

fn preferred_geo_pool<'a>(
    candidates: Vec<&'a VastOffer>,
    preferred_geolocations: &[String],
) -> Vec<&'a VastOffer> {
    if preferred_geolocations.is_empty() {
        return candidates;
    }
    for preferred in preferred_geolocations {
        let preferred = preferred.trim();
        if preferred.is_empty() {
            continue;
        }
        let tier = candidates
            .iter()
            .copied()
            .filter(|offer| geo_matches(offer.geolocation.as_deref(), preferred))
            .collect::<Vec<_>>();
        if !tier.is_empty() {
            return tier;
        }
    }
    candidates
}

fn offer_order(left: &VastOffer, right: &VastOffer) -> Ordering {
    left.dph_total
        .partial_cmp(&right.dph_total)
        .unwrap_or(Ordering::Equal)
        .then_with(|| {
            right
                .reliability2
                .unwrap_or(0.0)
                .partial_cmp(&left.reliability2.unwrap_or(0.0))
                .unwrap_or(Ordering::Equal)
        })
}

fn build_volume_info(volume: &WorkerVolumeConfig) -> Result<VastVolumeInfo> {
    let volume_id = volume
        .volume_id
        .context("profile volume.volume_id is required")?;
    let mount_path = api_mount_path(volume)?;
    if volume.create_new && volume.size_gb.unwrap_or_default() == 0 {
        bail!("profile volume.size_gb must be set when create_new=true");
    }
    Ok(VastVolumeInfo {
        mount_path,
        volume_id,
        create_new: volume.create_new,
        size: if volume.create_new {
            volume.size_gb
        } else {
            None
        },
    })
}

fn inject_volume_cache_env(values: &mut HashMap<String, String>, volume: &WorkerVolumeConfig) {
    let mount_path = runtime_mount_path(volume);
    let cache_subdir = volume.cache_subdir.trim_matches('/');
    let cache_root = if cache_subdir.is_empty() {
        mount_path
    } else {
        format!("{mount_path}/{cache_subdir}")
    };
    insert_default_env(values, "HF_HOME", format!("{cache_root}/hf"));
    insert_default_env(
        values,
        "HUGGINGFACE_HUB_CACHE",
        format!("{cache_root}/huggingface/hub"),
    );
    insert_default_env(
        values,
        "TRANSFORMERS_CACHE",
        format!("{cache_root}/transformers"),
    );
    insert_default_env(values, "TORCH_HOME", format!("{cache_root}/torch"));
    insert_default_env(values, "XDG_CACHE_HOME", format!("{cache_root}/xdg"));
    insert_default_env(values, "VLLM_CACHE_ROOT", format!("{cache_root}/vllm"));
}

fn insert_default_env(values: &mut HashMap<String, String>, key: &str, value: String) {
    values.entry(key.to_string()).or_insert(value);
}

fn api_mount_path(volume: &WorkerVolumeConfig) -> Result<String> {
    let mount_path = volume.mount_path.trim_matches('/');
    if mount_path.is_empty() {
        bail!("profile volume.mount_path must not be empty");
    }
    if !mount_path
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("profile volume.mount_path contains characters Vast rejects in mount names");
    }
    Ok(mount_path.to_string())
}

fn runtime_mount_path(volume: &WorkerVolumeConfig) -> String {
    format!("/{}", volume.mount_path.trim_matches('/'))
}

/// Vast publishes ports by accepting env keys of the form `-p HOST:CONTAINER`.
fn encode_env_map(values: &HashMap<String, String>, ports: &[u16]) -> HashMap<String, String> {
    let mut env = values.clone();
    for port in ports {
        env.insert(format!("-p {port}:{port}"), "1".to_string());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkerVolumeConfig;

    fn config() -> VastConfig {
        VastConfig {
            api_base_url: "https://console.vast.ai/api/v0".to_string(),
            api_key: String::new(),
            max_hourly_price_usd: 0.45,
            search_limit: 50,
            poll_interval_ms: 1_000,
            bootstrap_timeout_s: 60,
            verified_only: true,
            min_reliability: 0.95,
            log_tail_lines: 500,
            state_dir: "./state".to_string(),
            callback_base_url: String::new(),
            bootstrap_token: String::new(),
            max_runtime_hours: None,
        }
    }

    fn profile() -> WorkerProfileConfig {
        WorkerProfileConfig {
            min_gpu_ram_gb: 24,
            gpu_count: 1,
            min_reliability: 0.95,
            direct_ports_required: true,
            gpu_names: Vec::new(),
            preferred_geolocations: Vec::new(),
            image: "image".to_string(),
            template_hash_id: String::new(),
            disk_gb: 80.0,
            runtype: "ssh_direct".to_string(),
            target_state: "running".to_string(),
            label_prefix: "gpu".to_string(),
            onstart: String::new(),
            env: HashMap::new(),
            ports: vec![8000],
            volume: None,
        }
    }

    fn offer(id: u64, price: f64, machine_id: u64) -> VastOffer {
        VastOffer {
            id,
            machine_id: Some(machine_id),
            gpu_name: Some("RTX 3090".to_string()),
            geolocation: None,
            gpu_ram: 24_576,
            num_gpus: 1,
            direct_port_count: Some(8),
            disk_space: Some(80.0),
            dph_total: price,
            reliability2: Some(0.98),
            verification: Some("verified".to_string()),
            rented: false,
            rentable: true,
        }
    }

    #[test]
    fn selector_prefers_cheapest_valid_offer() {
        let offers = vec![offer(2, 0.41, 11), offer(1, 0.39, 12)];
        let selected = select_offer(&offers, &config(), &profile(), None, &HashSet::new()).unwrap();
        assert_eq!(selected.id, 1);
    }

    #[test]
    fn selector_breaks_price_ties_with_higher_reliability() {
        let mut cheaper_looking = offer(1, 0.20, 11);
        cheaper_looking.reliability2 = Some(0.96);
        let mut steadier = offer(2, 0.20, 12);
        steadier.reliability2 = Some(0.99);
        let offers = vec![cheaper_looking, steadier];
        let selected = select_offer(&offers, &config(), &profile(), None, &HashSet::new()).unwrap();
        assert_eq!(selected.id, 2);
    }

    #[test]
    fn search_request_asks_for_cheapest_ondemand_offer_under_the_cap() {
        let mut profile = profile();
        profile.gpu_names = vec!["RTX 3090".to_string()];
        let request = search_request(&config(), &profile, None, None);
        assert_eq!(request["type"], json!("ondemand"));
        assert_eq!(request["dph_total"]["lte"], json!(0.45));
        assert_eq!(request["order"], json!([["dph_total", "asc"]]));
        assert_eq!(request["gpu_ram"]["gte"], json!(24_576));
        assert_eq!(request["verified"]["eq"], json!(true));
        assert_eq!(request["allocated_storage"], json!(80.0));
        assert_eq!(request["disk_space"]["gte"], json!(80.0));
        assert!(request.get("geolocation").is_none());

        let regional = search_request(&config(), &profile, None, Some("DE"));
        assert_eq!(regional["geolocation"]["eq"], json!("DE"));
        assert_eq!(regional["allocated_storage"], regional["disk_space"]["gte"]);
    }

    #[test]
    fn selector_rejects_offers_without_the_requested_disk() {
        let mut tiny = offer(1, 0.10, 11);
        tiny.disk_space = Some(10.0);
        let mut fits = offer(2, 0.20, 12);
        fits.disk_space = Some(80.0);
        let offers = [tiny, fits];
        let selected = select_offer(&offers, &config(), &profile(), None, &HashSet::new()).unwrap();
        assert_eq!(selected.id, 2);
    }

    #[test]
    fn geolocation_query_uses_country_codes_and_does_not_substring_match_codes() {
        assert_eq!(geolocation_api_code("Germany"), "DE");
        assert_eq!(geolocation_api_code("de"), "DE");
        assert_eq!(geolocation_api_code("UK"), "GB");
        assert_eq!(geolocation_api_code("uk"), "GB");
        assert!(geo_matches(Some("Germany, DE"), "Germany"));
        assert!(geo_matches(Some("Germany, DE"), "DE"));
        assert!(!geo_matches(Some("China, CN"), "IN"));
        assert!(!geo_matches(Some("Finland, FI"), "IN"));
    }

    #[test]
    fn selector_does_not_treat_in_as_letters_inside_another_country() {
        let mut profile = profile();
        profile.preferred_geolocations = vec!["IN".to_string()];
        let mut china = offer(1, 0.10, 11);
        china.geolocation = Some("China, CN".to_string());
        let mut finland = offer(2, 0.11, 12);
        finland.geolocation = Some("Finland, FI".to_string());
        let mut india = offer(3, 0.20, 13);
        india.geolocation = Some("India, IN".to_string());
        let offers = [china, finland, india];
        let selected = select_offer(&offers, &config(), &profile, None, &HashSet::new()).unwrap();
        assert_eq!(selected.id, 3);
    }

    #[test]
    fn selector_honors_machine_constraint() {
        let offers = vec![offer(10, 0.10, 99), offer(11, 0.12, 77)];
        let selected =
            select_offer(&offers, &config(), &profile(), Some(77), &HashSet::new()).unwrap();
        assert_eq!(selected.id, 11);
    }

    #[test]
    fn selector_skips_rejected_offer_ids() {
        let offers = vec![offer(1, 0.11, 11), offer(2, 0.12, 12)];
        let rejected = HashSet::from([1u64]);
        let selected = select_offer(&offers, &config(), &profile(), None, &rejected).unwrap();
        assert_eq!(selected.id, 2);
    }

    #[test]
    fn stale_offer_errors_are_detected() {
        let error = anyhow::anyhow!(
            "creating Vast instance from offer 1: Vast API request failed with 410 Gone: {{\"error\":\"no_such_ask\"}}"
        );
        assert!(is_stale_offer_error(&error));
    }

    #[test]
    fn create_request_publishes_ports_and_volume_cache() {
        let mut config = config();
        config.callback_base_url = "https://callback.example".to_string();
        config.bootstrap_token = "bootstrap-token".to_string();
        let mut profile = profile();
        profile.volume = Some(WorkerVolumeConfig {
            mount_path: "/workspace-cache".to_string(),
            volume_id: Some(42),
            machine_id: None,
            create_new: false,
            size_gb: None,
            cache_subdir: "models".to_string(),
        });
        let request = build_create_request(&config, &profile, "gpu", "lease-001").unwrap();
        assert_eq!(
            request.env.get("HF_HOME").map(String::as_str),
            Some("/workspace-cache/models/hf")
        );
        assert_eq!(
            request.env.get("RENT_CALLBACK_URL").map(String::as_str),
            Some("https://callback.example")
        );
        assert_eq!(
            request.env.get("RENT_BOOTSTRAP_TOKEN").map(String::as_str),
            Some("bootstrap-token")
        );
        assert_eq!(
            request.env.get("-p 8000:8000").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            request
                .volume_info
                .as_ref()
                .map(|info| info.mount_path.as_str()),
            Some("workspace-cache")
        );
        assert_eq!(request.disk, profile.disk_gb);
        assert!(request.env.get("RENT_LEASE_ID").is_some());
        let rendered = format!("{request:?}");
        assert!(!rendered.contains("bootstrap-token"));
        assert!(rendered.contains("[redacted]"));
    }

    #[test]
    fn create_request_omits_empty_callback_and_token() {
        let request = build_create_request(&config(), &profile(), "gpu", "lease-001").unwrap();
        assert!(!request.env.contains_key("RENT_CALLBACK_URL"));
        assert!(!request.env.contains_key("RENT_BOOTSTRAP_TOKEN"));
    }

    #[test]
    fn selector_prefers_configured_geolocations_when_available() {
        let mut profile = profile();
        profile.preferred_geolocations = vec!["Germany".to_string(), "Czechia".to_string()];
        let mut outside = offer(1, 0.10, 11);
        outside.geolocation = Some("California, US".to_string());
        let mut inside = offer(2, 0.14, 12);
        inside.geolocation = Some("Czechia, CZ".to_string());
        let offers = [outside, inside];
        let selected = select_offer(&offers, &config(), &profile, None, &HashSet::new()).unwrap();
        assert_eq!(selected.id, 2);
    }

    #[test]
    fn selector_uses_geolocation_priority_before_price() {
        let mut profile = profile();
        profile.preferred_geolocations = vec!["Czechia".to_string(), "Poland".to_string()];
        let mut poland = offer(1, 0.10, 11);
        poland.geolocation = Some("Poland, PL".to_string());
        let mut czechia = offer(2, 0.14, 12);
        czechia.geolocation = Some("Czechia, CZ".to_string());
        let offers = [poland, czechia];
        let selected = select_offer(&offers, &config(), &profile, None, &HashSet::new()).unwrap();
        assert_eq!(selected.id, 2);
    }
}
