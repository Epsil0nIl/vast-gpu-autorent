use std::fmt;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::{Client, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{ensure_trusted_api_base, VastConfig, WorkerProfileConfig};
use crate::select::search_request;
use crate::types::{
    VastCreateInstanceRequest, VastInstance, VastLogsResponse, VastOffer, VastVolume,
    VastVolumeOffer,
};

#[derive(Debug, Clone)]
pub struct VastClient {
    client: Client,
    pub config: VastConfig,
}

#[derive(Debug, Clone)]
pub struct ResolvedVolumeConstraint {
    pub machine_id: u64,
    pub source: &'static str,
    pub volume_id: u64,
}

impl VastClient {
    pub fn new(config: VastConfig) -> Result<Self> {
        ensure_trusted_api_base(&config.api_base_url)?;
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building Vast HTTP client")?;
        Ok(Self { client, config })
    }

    pub fn is_enabled(&self) -> bool {
        !self.config.api_key.trim().is_empty()
    }

    pub async fn search_offers(
        &self,
        profile: &WorkerProfileConfig,
        machine_id: Option<u64>,
        geolocation: Option<&str>,
    ) -> Result<Vec<VastOffer>> {
        let body = search_request(&self.config, profile, machine_id, geolocation);
        let response: SearchOffersEnvelope = self
            .request(reqwest::Method::POST, "/bundles/", Some(&body))
            .await
            .context("searching Vast offers")?;
        Ok(response.offers.into_vec())
    }

    pub async fn create_instance(
        &self,
        offer_id: u64,
        request: &VastCreateInstanceRequest,
    ) -> Result<u64> {
        let endpoint = format!("/asks/{offer_id}/");
        let response: VastCreateInstanceResponse = self
            .request(reqwest::Method::PUT, &endpoint, Some(request))
            .await
            .with_context(|| format!("creating Vast instance from offer {offer_id}"))?;
        if !response.success {
            bail!("Vast create_instance returned success=false");
        }
        Ok(response.new_contract)
    }

    pub async fn show_instance(&self, instance_id: u64) -> Result<VastInstance> {
        let endpoint = format!("/instances/{instance_id}/");
        let response: VastShowInstanceEnvelope = self
            .request(reqwest::Method::GET, &endpoint, Option::<&()>::None)
            .await
            .with_context(|| format!("fetching Vast instance {instance_id}"))?;
        Ok(response.instances)
    }

    pub async fn destroy_instance(&self, instance_id: u64) -> Result<()> {
        let endpoint = format!("/instances/{instance_id}/");
        let response: VastSimpleResponse = self
            .request(reqwest::Method::DELETE, &endpoint, Option::<&()>::None)
            .await
            .with_context(|| format!("destroying Vast instance {instance_id}"))?;
        if !response.success {
            bail!(
                "Vast destroy_instance returned success=false for {instance_id}: {}",
                response.msg.unwrap_or_default()
            );
        }
        Ok(())
    }

    pub async fn request_logs(&self, instance_id: u64) -> Result<VastLogsResponse> {
        let endpoint = format!("/instances/request_logs/{instance_id}");
        self.request(
            reqwest::Method::PUT,
            &endpoint,
            Some(&json!({
                "tail": self.config.log_tail_lines,
                "daemon_logs": true,
            })),
        )
        .await
        .with_context(|| format!("requesting Vast logs for {instance_id}"))
    }

    pub async fn wait_for_instance_ready(
        &self,
        instance_id: u64,
        profile_name: &str,
    ) -> Result<VastInstance> {
        let deadline =
            std::time::Instant::now() + Duration::from_secs(self.config.bootstrap_timeout_s.max(1));
        loop {
            let instance = self.show_instance(instance_id).await?;
            let status = instance
                .actual_status
                .as_deref()
                .or(instance.cur_state.as_deref())
                .unwrap_or("");
            if is_terminal_failure(status) {
                bail!("Vast instance {instance_id} entered status {status} before SSH was accepting connections");
            }
            if status.eq_ignore_ascii_case("running") {
                if let Some((host, port)) = ssh_endpoint(&instance) {
                    if tcp_accepts(&host, port).await {
                        return Ok(instance);
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                bail!(
                    "timed out waiting for Vast instance {instance_id} to accept SSH for profile {profile_name}"
                );
            }
            tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms.max(250))).await;
        }
    }

    pub async fn list_volumes(&self) -> Result<Vec<VastVolume>> {
        let response: VastVolumesEnvelope = self
            .request(reqwest::Method::GET, "/volumes/", Option::<&()>::None)
            .await
            .context("listing Vast volumes")?;
        Ok(response.volumes)
    }

    pub async fn search_volume_offers(&self, query: &Value) -> Result<Vec<VastVolumeOffer>> {
        let response: SearchVolumeOffersEnvelope = self
            .request(reqwest::Method::POST, "/volumes/search/", Some(query))
            .await
            .context("searching Vast volume offers")?;
        Ok(response.offers)
    }

    pub async fn resolve_volume_constraint(
        &self,
        profile: &WorkerProfileConfig,
    ) -> Result<Option<ResolvedVolumeConstraint>> {
        let Some(volume) = profile.volume.as_ref() else {
            return Ok(None);
        };
        if let Some(machine_id) = volume.machine_id {
            return Ok(Some(ResolvedVolumeConstraint {
                machine_id,
                source: "config_machine",
                volume_id: volume.volume_id.unwrap_or_default(),
            }));
        }
        let volume_id = volume.volume_id.with_context(|| {
            format!("profile {} volume requires volume_id", profile.label_prefix)
        })?;
        if volume.create_new {
            let offers = self
                .search_volume_offers(&json!({
                    "limit": 1,
                    "id": {"eq": volume_id},
                }))
                .await?;
            let offer = offers
                .into_iter()
                .find(|offer| offer.id == volume_id)
                .with_context(|| format!("Vast volume offer {volume_id} not found"))?;
            return Ok(Some(ResolvedVolumeConstraint {
                machine_id: offer.machine_id,
                source: "volume_offer",
                volume_id,
            }));
        }
        let volume_entry = self
            .list_volumes()
            .await?
            .into_iter()
            .find(|entry| entry.id == volume_id)
            .with_context(|| format!("Vast volume {volume_id} not found in account"))?;
        Ok(Some(ResolvedVolumeConstraint {
            machine_id: volume_entry.machine_id,
            source: "existing_volume",
            volume_id,
        }))
    }

    pub async fn list_instances(&self) -> Result<Vec<ListedInstance>> {
        let response: ListedInstancesEnvelope = self
            .request(reqwest::Method::GET, "/instances/", Option::<&()>::None)
            .await
            .context("listing Vast instances")?;
        Ok(response.instances.into_vec())
    }

    pub fn missing_instance(error: &anyhow::Error) -> bool {
        error
            .chain()
            .any(|cause| cause.downcast_ref::<MissingInstance>().is_some())
    }

    pub fn ambiguous_result(error: &anyhow::Error) -> bool {
        error
            .chain()
            .any(|cause| cause.downcast_ref::<AmbiguousApiResult>().is_some())
    }

    async fn request<B, T>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let url = format!("{}{}", self.config.api_base_url.trim_end_matches('/'), path);
        let request = self
            .client
            .request(method, url)
            .bearer_auth(&self.config.api_key);
        let request = if let Some(body) = body {
            request.json(body)
        } else {
            request
        };
        let response = request.send().await.map_err(|error| {
            anyhow!(AmbiguousApiResult {
                detail: error.to_string(),
            })
        })?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            bail!("Vast API rate-limited the request");
        }
        if response.status().is_server_error() || response.status() == StatusCode::REQUEST_TIMEOUT {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(AmbiguousApiResult {
                detail: format!("{status}: {body}"),
            }
            .into());
        }
        if response.status() == StatusCode::NOT_FOUND {
            if let Some(instance_id) = instance_id_from_path(path) {
                return Err(MissingInstance { instance_id }.into());
            }
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("Vast API request failed with {status}: {body}");
        }
        let status = response.status();
        let body = response.text().await.map_err(|error| {
            anyhow!(AmbiguousApiResult {
                detail: error.to_string(),
            })
        })?;
        serde_json::from_str(&body).map_err(|error| {
            anyhow!(AmbiguousApiResult {
                detail: format!("failed to decode {status} response: {error}; body: {body}"),
            })
        })
    }
}

#[derive(Debug)]
pub struct MissingInstance {
    pub instance_id: u64,
}

impl fmt::Display for MissingInstance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Vast instance {} was not found",
            self.instance_id
        )
    }
}

impl std::error::Error for MissingInstance {}

#[derive(Debug)]
pub struct AmbiguousApiResult {
    pub detail: String,
}

impl fmt::Display for AmbiguousApiResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Vast result is ambiguous: {}", self.detail)
    }
}

impl std::error::Error for AmbiguousApiResult {}

fn instance_id_from_path(path: &str) -> Option<u64> {
    let rest = path.trim_start_matches("/instances/").trim_end_matches('/');
    if rest.is_empty() || !rest.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListedInstance {
    pub id: u64,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ListedInstancesEnvelope {
    instances: OneOrMany<ListedInstance>,
}

#[derive(Debug, Clone, Deserialize)]
struct SearchOffersEnvelope {
    offers: OneOrMany<VastOffer>,
}

#[derive(Debug, Clone, Deserialize)]
struct SearchVolumeOffersEnvelope {
    offers: Vec<VastVolumeOffer>,
}

#[derive(Debug, Clone, Deserialize)]
struct VastVolumesEnvelope {
    volumes: Vec<VastVolume>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    fn into_vec(self) -> Vec<T> {
        match self {
            OneOrMany::One(item) => vec![item],
            OneOrMany::Many(items) => items,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct VastCreateInstanceResponse {
    success: bool,
    new_contract: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct VastShowInstanceEnvelope {
    instances: VastInstance,
}

#[derive(Debug, Clone, Deserialize)]
struct VastSimpleResponse {
    success: bool,
    msg: Option<String>,
}

pub fn ssh_endpoint(instance: &VastInstance) -> Option<(String, u16)> {
    let port = instance.ssh_port.filter(|port| *port != 0)?;
    let host = instance
        .ssh_host
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_string)
        .or_else(|| {
            instance
                .public_ipaddr
                .as_deref()
                .map(str::trim)
                .filter(|host| !host.is_empty())
                .map(str::to_string)
        })?;
    Some((host, port))
}

fn is_terminal_failure(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "exited" | "error" | "dead" | "offline" | "failed" | "destroyed" | "stopped" | "stopping"
    )
}

async fn tcp_accepts(host: &str, port: u16) -> bool {
    let connect = tokio::net::TcpStream::connect((host, port));
    matches!(
        tokio::time::timeout(Duration::from_secs(5), connect).await,
        Ok(Ok(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_instance_is_typed_and_a_bare_404_is_not_enough() {
        let error = anyhow::Error::from(MissingInstance { instance_id: 15 })
            .context("destroying Vast instance 15");
        assert!(VastClient::missing_instance(&error));
        let other_404 = anyhow::anyhow!(
            "Vast API request failed with 404 Not Found: {{\"error\":\"no_such_instance\"}}"
        );
        assert!(!VastClient::missing_instance(&other_404));
        assert!(instance_id_from_path("/instances/15/").is_some());
        assert!(instance_id_from_path("/instances/request_logs/15").is_none());
    }

    #[test]
    fn ssh_endpoint_requires_a_port_not_just_metadata() {
        let mut instance = VastInstance {
            id: 1,
            actual_status: Some("running".to_string()),
            cur_state: Some("running".to_string()),
            ssh_host: None,
            ssh_port: None,
            ports: json!({"8000/tcp": [{"HostPort": "8000"}]}),
            public_ipaddr: Some("203.0.113.10".to_string()),
            geolocation: None,
            dph_total: None,
            reliability2: None,
        };
        assert!(ssh_endpoint(&instance).is_none());
        instance.ssh_port = Some(22);
        assert_eq!(
            ssh_endpoint(&instance),
            Some(("203.0.113.10".to_string(), 22))
        );
        instance.ssh_host = Some("ssh.example".to_string());
        assert_eq!(
            ssh_endpoint(&instance),
            Some(("ssh.example".to_string(), 22))
        );
    }

    #[tokio::test]
    async fn tcp_probe_requires_an_open_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        assert!(tcp_accepts("127.0.0.1", port).await);

        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_port = closed.local_addr().unwrap().port();
        drop(closed);
        assert!(!tcp_accepts("127.0.0.1", closed_port).await);
    }
}
