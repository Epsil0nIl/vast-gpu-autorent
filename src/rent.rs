use std::{collections::HashSet, path::PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use uuid::Uuid;

use crate::{
    client::VastClient,
    config::AppConfig,
    lease::{
        runtime_exceeded, LeaseGuard, LeaseRecord, LeaseStore, STATUS_DESTROY_FAILED,
        STATUS_PROVISIONING,
    },
    select::{
        build_create_request, geolocation_api_code, is_stale_offer_error, offer_in_region,
        ranked_offers,
    },
    types::VastInstance,
    VastOffer,
};

#[derive(Debug, Clone)]
pub struct RentOutcome {
    pub already_active: bool,
    pub resumed_provisioning: bool,
    pub lease: LeaseRecord,
}

pub struct Renter {
    config: AppConfig,
    client: VastClient,
    leases: LeaseStore,
}

impl Renter {
    pub fn new(config: AppConfig) -> Result<Self> {
        config.validate()?;
        let lease_path = PathBuf::from(&config.vast.state_dir).join("leases.json");
        let client = VastClient::new(config.vast.clone())?;
        Ok(Self {
            config,
            client,
            leases: LeaseStore::new(lease_path),
        })
    }

    pub fn price_caveat(&self, profile_name: &str) -> Result<String> {
        let profile = self.config.profile(profile_name)?;
        let runtime = match self.config.vast.max_runtime_hours {
            Some(hours) => format!(
                "Instances older than {hours} hours are destroyed on the next rent, status, or destroy."
            ),
            None => "Runtime is not capped. Destroy the instance when you are done. A stopped instance can keep accruing storage charges.".to_string(),
        };
        Ok(format!(
            "Hourly cap ${:.4}/hr is the Vast dph_total quote with {:.0} GB of disk included. Bandwidth is billed separately and is not part of this cap. {runtime}",
            self.config.vast.max_hourly_price_usd,
            profile.disk_gb
        ))
    }

    pub async fn search(&self, profile_name: &str) -> Result<Vec<VastOffer>> {
        let profile = self.config.profile(profile_name)?.clone();
        let machine_id = self.machine_constraint(&profile).await?;
        self.matching_offers(&profile, machine_id, &HashSet::new())
            .await
    }

    pub async fn rent(&self, profile_name: &str, force_replace: bool) -> Result<RentOutcome> {
        let profile = self.config.profile(profile_name)?.clone();
        let guard = self.leases.exclusive().await?;
        if let Some(existing) = self.reconcile(&guard, profile_name).await?.lease {
            if existing.status == STATUS_DESTROY_FAILED && !force_replace {
                bail!(
                    "instance {} is still billed after a failed destroy. Run destroy, or pass --force-replace to retry it. A new instance was not created.",
                    existing.instance_id
                );
            }
            if !force_replace && existing.status == STATUS_PROVISIONING {
                return self
                    .finish_provisioning(&guard, existing, profile_name, true)
                    .await;
            }
            if !force_replace {
                return Ok(RentOutcome {
                    already_active: true,
                    resumed_provisioning: false,
                    lease: existing,
                });
            }
            self.destroy_recorded(&guard, &existing)
                .await
                .with_context(|| {
                    format!(
                        "refusing to rent a replacement while instance {} still exists",
                        existing.instance_id
                    )
                })?;
        }

        let lease_id = Uuid::new_v4().to_string();
        let create_request =
            build_create_request(&self.config.vast, &profile, profile_name, &lease_id)?;
        let machine_constraint = self.machine_constraint(&profile).await?;
        let mut rejected_offer_ids = HashSet::new();
        let mut last_create_error = None;
        let (instance_id, offer) = loop {
            let offers = self
                .matching_offers(&profile, machine_constraint, &rejected_offer_ids)
                .await?;
            let Some(selected) = offers.into_iter().next() else {
                if let Some(error) = last_create_error.as_ref() {
                    bail!(
                        "no valid Vast offers found for profile {profile_name} after rejecting stale offers: {error:#}"
                    );
                }
                bail!("no valid Vast offers found for profile {profile_name}");
            };
            match self
                .client
                .create_instance(selected.id, &create_request)
                .await
            {
                Ok(created_instance_id) => break (created_instance_id, selected),
                Err(error) if is_stale_offer_error(&error) => {
                    rejected_offer_ids.insert(selected.id);
                    last_create_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        };

        let lease = LeaseRecord {
            profile: profile_name.to_string(),
            lease_id,
            instance_id,
            offer_id: offer.id,
            gpu_name: offer.gpu_name.clone(),
            geolocation: offer.geolocation.clone(),
            host: None,
            ssh_port: None,
            public_ip: None,
            hourly_price_usd: offer.dph_total,
            status: STATUS_PROVISIONING.to_string(),
            created_at: Utc::now(),
        };
        if let Err(error) = guard.upsert(lease.clone()) {
            return self.abandon_unrecorded(instance_id, error).await;
        }
        self.finish_provisioning(&guard, lease, profile_name, false)
            .await
    }

    pub async fn status(&self, profile_name: &str) -> Result<LeaseOutcome> {
        let guard = self.leases.exclusive().await?;
        let reconciled = self.reconcile(&guard, profile_name).await?;
        Ok(LeaseOutcome {
            lease: reconciled.lease,
            message: reconciled.message,
        })
    }

    pub async fn destroy(&self, profile_name: &str) -> Result<LeaseOutcome> {
        let guard = self.leases.exclusive().await?;
        let reconciled = self.reconcile(&guard, profile_name).await?;
        let Some(active) = reconciled.lease else {
            return Ok(LeaseOutcome {
                lease: None,
                message: reconciled.message.or(Some("no local lease".to_string())),
            });
        };
        let instance_id = active.instance_id;
        self.destroy_recorded(&guard, &active).await?;
        Ok(LeaseOutcome {
            lease: Some(active),
            message: Some(format!("destroyed instance {instance_id}")),
        })
    }

    pub async fn logs(&self, profile_name: &str) -> Result<crate::VastLogsResponse> {
        let guard = self.leases.exclusive().await?;
        let reconciled = self.reconcile(&guard, profile_name).await?;
        let active = reconciled.lease.with_context(|| {
            reconciled
                .message
                .unwrap_or_else(|| format!("no local lease for profile {profile_name}"))
        })?;
        self.client.request_logs(active.instance_id).await
    }

    async fn matching_offers(
        &self,
        profile: &crate::WorkerProfileConfig,
        machine_id: Option<u64>,
        rejected_offer_ids: &HashSet<u64>,
    ) -> Result<Vec<VastOffer>> {
        // Ask Vast for each preferred region on its own. A single global page of
        // search_limit offers can hide every machine in the region we actually want.
        if machine_id.is_none() {
            for region in profile
                .preferred_geolocations
                .iter()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
            {
                let code = geolocation_api_code(region);
                let offers = self
                    .client
                    .search_offers(profile, None, Some(code.as_str()))
                    .await?;
                let regional = offers
                    .into_iter()
                    .filter(|offer| offer_in_region(offer, region, &code))
                    .collect::<Vec<_>>();
                let ranked = ranked_offers(
                    &regional,
                    &self.config.vast,
                    profile,
                    None,
                    rejected_offer_ids,
                );
                if !ranked.is_empty() {
                    return Ok(ranked.into_iter().cloned().collect());
                }
            }
        }
        let offers = self.client.search_offers(profile, machine_id, None).await?;
        Ok(ranked_offers(
            &offers,
            &self.config.vast,
            profile,
            machine_id,
            rejected_offer_ids,
        )
        .into_iter()
        .cloned()
        .collect())
    }

    async fn machine_constraint(
        &self,
        profile: &crate::WorkerProfileConfig,
    ) -> Result<Option<u64>> {
        Ok(self
            .client
            .resolve_volume_constraint(profile)
            .await?
            .map(|constraint| constraint.machine_id))
    }

    async fn reconcile(&self, guard: &LeaseGuard, profile_name: &str) -> Result<Reconciled> {
        let Some(mut lease) = guard.get(profile_name)? else {
            return Ok(Reconciled {
                lease: None,
                message: None,
            });
        };
        if self.runtime_is_over(&lease) {
            let instance_id = lease.instance_id;
            self.destroy_recorded(guard, &lease)
                .await
                .with_context(|| {
                    format!(
                    "instance {instance_id} exceeded max_runtime_hours and could not be destroyed"
                )
                })?;
            return Ok(Reconciled {
                lease: None,
                message: Some(format!(
                    "destroyed instance {instance_id} because it exceeded max_runtime_hours"
                )),
            });
        }
        if lease.status == STATUS_DESTROY_FAILED {
            match self.client.destroy_instance(lease.instance_id).await {
                Ok(()) => {
                    let instance_id = lease.instance_id;
                    guard.remove(profile_name)?;
                    return Ok(Reconciled {
                        lease: None,
                        message: Some(format!(
                            "cleared instance {instance_id} after a failed destroy was retried"
                        )),
                    });
                }
                Err(error) if VastClient::missing_instance(&error) => {
                    let instance_id = lease.instance_id;
                    guard.remove(profile_name)?;
                    return Ok(Reconciled {
                        lease: None,
                        message: Some(format!(
                            "Vast no longer has instance {instance_id}. Cleared the local lease."
                        )),
                    });
                }
                Err(_) => {
                    return Ok(Reconciled {
                        lease: Some(lease),
                        message: None,
                    })
                }
            }
        }

        match self.client.show_instance(lease.instance_id).await {
            Ok(instance) => {
                apply_instance(&mut lease, &instance);
                guard.upsert(lease.clone())?;
                Ok(Reconciled {
                    lease: Some(lease),
                    message: None,
                })
            }
            Err(error) if VastClient::missing_instance(&error) => {
                let instance_id = lease.instance_id;
                guard.remove(profile_name)?;
                Ok(Reconciled {
                    lease: None,
                    message: Some(format!(
                        "Vast no longer has instance {instance_id}. Cleared the local lease."
                    )),
                })
            }
            Err(error) => Err(error).with_context(|| {
                format!(
                    "instance {} is recorded locally but Vast could not be queried. No second instance was created.",
                    lease.instance_id
                )
            }),
        }
    }

    fn runtime_is_over(&self, lease: &LeaseRecord) -> bool {
        match self.config.vast.max_runtime_hours {
            Some(hours) => runtime_exceeded(lease.created_at, hours, Utc::now()),
            None => false,
        }
    }

    async fn finish_provisioning(
        &self,
        guard: &LeaseGuard,
        mut lease: LeaseRecord,
        profile_name: &str,
        resumed: bool,
    ) -> Result<RentOutcome> {
        match self
            .client
            .wait_for_instance_ready(lease.instance_id, profile_name)
            .await
        {
            Ok(instance) => {
                apply_instance(&mut lease, &instance);
                lease.status = instance
                    .actual_status
                    .clone()
                    .or(instance.cur_state.clone())
                    .unwrap_or_else(|| "running".to_string());
                guard.upsert(lease.clone())?;
                Ok(RentOutcome {
                    already_active: false,
                    resumed_provisioning: resumed,
                    lease,
                })
            }
            Err(error) => {
                let logs = self.client.request_logs(lease.instance_id).await.ok();
                let log_url = logs.and_then(|logs| logs.result_url).unwrap_or_default();
                let destroy_note = match self.client.destroy_instance(lease.instance_id).await {
                    Ok(()) => {
                        guard.remove(&lease.profile)?;
                        "destroyed the instance that failed to become ready".to_string()
                    }
                    Err(destroy_error) if VastClient::missing_instance(&destroy_error) => {
                        guard.remove(&lease.profile)?;
                        "the instance was already gone".to_string()
                    }
                    Err(destroy_error) => {
                        lease.status = STATUS_DESTROY_FAILED.to_string();
                        guard.upsert(lease.clone()).with_context(|| {
                            format!(
                                "instance {} is still billed and the lease file could not record the failed destroy",
                                lease.instance_id
                            )
                        })?;
                        format!(
                            "could not destroy instance {}: {destroy_error:#}. The local lease was kept so the next command can retry.",
                            lease.instance_id
                        )
                    }
                };
                Err(anyhow!(
                    "vast instance {} failed to become ready: {error:#}. {destroy_note}. logs: {log_url}",
                    lease.instance_id
                ))
            }
        }
    }

    async fn destroy_recorded(&self, guard: &LeaseGuard, active: &LeaseRecord) -> Result<()> {
        match self.client.destroy_instance(active.instance_id).await {
            Ok(()) => {
                guard.remove(&active.profile)?;
                Ok(())
            }
            Err(error) if VastClient::missing_instance(&error) => {
                guard.remove(&active.profile)?;
                Ok(())
            }
            Err(error) => {
                let mut failed = active.clone();
                failed.status = STATUS_DESTROY_FAILED.to_string();
                let destroy_message = format!("{error:#}");
                guard.upsert(failed).with_context(|| {
                    format!(
                        "instance {} could not be destroyed ({destroy_message}) and the lease file could not record it",
                        active.instance_id
                    )
                })?;
                Err(error)
            }
        }
    }

    async fn abandon_unrecorded(
        &self,
        instance_id: u64,
        write_error: anyhow::Error,
    ) -> Result<RentOutcome> {
        match self.client.destroy_instance(instance_id).await {
            Ok(()) => Err(write_error.context(format!(
                "destroyed instance {instance_id} because the lease file could not be written"
            ))),
            Err(destroy_error) if VastClient::missing_instance(&destroy_error) => {
                Err(write_error.context(format!(
                    "instance {instance_id} was already gone and the lease file could not be written"
                )))
            }
            Err(destroy_error) => Err(anyhow!(
                "created Vast instance {instance_id} but could not write the lease file ({write_error:#}) and could not destroy it ({destroy_error:#}). Destroy instance {instance_id} in the Vast console before renting again."
            )),
        }
    }
}

fn apply_instance(lease: &mut LeaseRecord, instance: &VastInstance) {
    if let Some(host) = instance
        .ssh_host
        .clone()
        .or_else(|| instance.public_ipaddr.clone())
    {
        lease.host = Some(host);
    }
    if let Some(port) = instance.ssh_port {
        lease.ssh_port = Some(port);
    }
    if let Some(ip) = instance.public_ipaddr.clone() {
        lease.public_ip = Some(ip);
    }
    if let Some(price) = instance.dph_total {
        lease.hourly_price_usd = price;
    }
    if let Some(geo) = instance.geolocation.clone() {
        lease.geolocation = Some(geo);
    }
    if lease.status != STATUS_PROVISIONING && lease.status != STATUS_DESTROY_FAILED {
        if let Some(status) = instance
            .actual_status
            .clone()
            .or_else(|| instance.cur_state.clone())
        {
            lease.status = status;
        }
    }
}

struct Reconciled {
    lease: Option<LeaseRecord>,
    message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LeaseOutcome {
    pub lease: Option<LeaseRecord>,
    pub message: Option<String>,
}
