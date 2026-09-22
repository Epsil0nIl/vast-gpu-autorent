use std::{collections::HashSet, path::PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use uuid::Uuid;

use crate::{
    client::VastClient,
    config::AppConfig,
    lease::{
        runtime_exceeded, LeaseGuard, LeaseRecord, LeaseStore, STATUS_DESTROY_FAILED,
        STATUS_INTENT, STATUS_PROVISIONING,
    },
    select::{
        build_create_request, geolocation_api_code, instance_label, is_stale_offer_error,
        offer_in_region, ranked_offers,
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
                "max_runtime_hours is {hours}. The next rent, status, destroy, or logs destroys an older instance. This program does not keep running after it exits, so that is not an automatic deadline."
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
        if let Some(existing) = self.reconcile(&guard, profile_name, false).await?.lease {
            if existing.status == STATUS_DESTROY_FAILED && !force_replace {
                bail!(
                    "instance {} is still billed after a failed destroy. Run destroy, or pass --force-replace to retry it. A new instance was not created.",
                    recorded_id(&existing)?
                );
            }
            if !force_replace
                && (existing.status == STATUS_PROVISIONING || existing.status == STATUS_INTENT)
            {
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
            let existing_id = recorded_id(&existing)?;
            self.destroy_recorded(&guard, &existing)
                .await
                .with_context(|| {
                    format!(
                        "refusing to rent a replacement while instance {existing_id} still exists"
                    )
                })?;
        }

        let lease_id = Uuid::new_v4().to_string();
        let mut lease = LeaseRecord {
            profile: profile_name.to_string(),
            lease_id: lease_id.clone(),
            label: instance_label(&profile.label_prefix, &lease_id),
            instance_id: None,
            offer_id: 0,
            gpu_name: None,
            geolocation: None,
            host: None,
            ssh_port: None,
            public_ip: None,
            hourly_price_usd: 0.0,
            status: STATUS_INTENT.to_string(),
            created_at: Utc::now(),
        };
        guard.upsert(lease.clone())?;
        let create_request =
            build_create_request(&self.config.vast, &profile, profile_name, &lease_id)?;
        let machine_constraint = self.machine_constraint(&profile).await?;
        let mut rejected_offer_ids = HashSet::new();
        let mut last_create_error = None;
        loop {
            let offers = self
                .matching_offers(&profile, machine_constraint, &rejected_offer_ids)
                .await?;
            let Some(selected) = offers.into_iter().next() else {
                guard.remove(profile_name)?;
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
                Ok(created_instance_id) => {
                    lease.instance_id = Some(created_instance_id);
                    lease.offer_id = selected.id;
                    lease.gpu_name = selected.gpu_name.clone();
                    lease.geolocation = selected.geolocation.clone();
                    lease.hourly_price_usd = selected.dph_total;
                    lease.status = STATUS_PROVISIONING.to_string();
                    if let Err(error) = guard.upsert(lease.clone()) {
                        return self
                            .keep_or_destroy_after_save_failure(
                                &guard,
                                &lease,
                                created_instance_id,
                                error,
                            )
                            .await;
                    }
                    return self
                        .finish_provisioning(&guard, lease, profile_name, false)
                        .await;
                }
                Err(error) if is_stale_offer_error(&error) => {
                    rejected_offer_ids.insert(selected.id);
                    last_create_error = Some(error);
                }
                Err(error) if VastClient::ambiguous_result(&error) => {
                    bail!(
                        "the create for label {} may have succeeded, but the response was lost ({error:#}). The local intent was kept. Run status before renting again. No second instance was created.",
                        lease.label
                    );
                }
                Err(error) => {
                    guard.remove(profile_name)?;
                    return Err(error);
                }
            }
        }
    }

    pub async fn status(&self, profile_name: &str) -> Result<LeaseOutcome> {
        let guard = self.leases.exclusive().await?;
        let reconciled = self.reconcile(&guard, profile_name, false).await?;
        Ok(LeaseOutcome {
            lease: reconciled.lease,
            message: reconciled.message,
        })
    }

    pub async fn destroy(
        &self,
        profile_name: &str,
        abandon_unresolved: bool,
    ) -> Result<LeaseOutcome> {
        let guard = self.leases.exclusive().await?;
        let reconciled = self
            .reconcile(&guard, profile_name, abandon_unresolved)
            .await?;
        let Some(active) = reconciled.lease else {
            return Ok(LeaseOutcome {
                lease: None,
                message: reconciled.message.or(Some("no local lease".to_string())),
            });
        };
        let instance_id = recorded_id(&active)?;
        self.destroy_recorded(&guard, &active).await?;
        Ok(LeaseOutcome {
            lease: Some(active),
            message: Some(format!("destroyed instance {instance_id}")),
        })
    }

    pub async fn logs(&self, profile_name: &str) -> Result<crate::VastLogsResponse> {
        let guard = self.leases.exclusive().await?;
        let reconciled = self.reconcile(&guard, profile_name, false).await?;
        let active = reconciled.lease.with_context(|| {
            reconciled
                .message
                .unwrap_or_else(|| format!("no local lease for profile {profile_name}"))
        })?;
        self.client.request_logs(recorded_id(&active)?).await
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

    async fn reconcile(
        &self,
        guard: &LeaseGuard,
        profile_name: &str,
        abandon_unresolved: bool,
    ) -> Result<Reconciled> {
        let Some(mut lease) = guard.get(profile_name)? else {
            return Ok(Reconciled {
                lease: None,
                message: None,
            });
        };
        if lease.instance_id.is_none() || lease.status == STATUS_INTENT {
            let Some(resolved) = self
                .resolve_intent(guard, lease, abandon_unresolved)
                .await?
            else {
                return Ok(Reconciled {
                    lease: None,
                    message: Some(
                        "cleared an unresolved create after Vast's listing did not show its label. Check the console before renting again; a delayed listing can still be wrong."
                            .to_string(),
                    ),
                });
            };
            lease = resolved;
        }
        let instance_id = recorded_id(&lease)?;
        if self.runtime_is_over(&lease) {
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
                    "destroyed instance {instance_id} because it exceeded max_runtime_hours on this command. This is not a background timer."
                )),
            });
        }
        if lease.status == STATUS_DESTROY_FAILED {
            match self.client.destroy_instance(instance_id).await {
                Ok(()) => {
                    guard.remove(profile_name)?;
                    return Ok(Reconciled {
                        lease: None,
                        message: Some(format!(
                            "cleared instance {instance_id} after a failed destroy was retried"
                        )),
                    });
                }
                Err(error) if VastClient::missing_instance(&error) => {
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

        match self.client.show_instance(instance_id).await {
            Ok(instance) => {
                apply_instance(&mut lease, &instance);
                guard.upsert(lease.clone())?;
                Ok(Reconciled {
                    lease: Some(lease),
                    message: None,
                })
            }
            Err(error) if VastClient::missing_instance(&error) => {
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
                    "instance {instance_id} is recorded locally but Vast could not be queried. No second instance was created."
                )
            }),
        }
    }

    async fn resolve_intent(
        &self,
        guard: &LeaseGuard,
        mut lease: LeaseRecord,
        abandon_unresolved: bool,
    ) -> Result<Option<LeaseRecord>> {
        if lease.label.trim().is_empty() {
            bail!(
                "the local create intent for profile {} has no label, so it cannot be matched to a Vast instance. No new instance was created.",
                lease.profile
            );
        }
        let listed = self.client.list_instances().await.with_context(|| {
            format!(
                "could not list Vast instances while label {} is unresolved. No new instance was created.",
                lease.label
            )
        })?;
        let pairs = listed
            .iter()
            .map(|item| (item.id, item.label.as_deref()))
            .collect::<Vec<_>>();
        match match_instance_label(&lease.label, &pairs) {
            LabelMatch::One(instance_id) => {
                lease.instance_id = Some(instance_id);
                if lease.status == STATUS_INTENT {
                    lease.status = STATUS_PROVISIONING.to_string();
                }
                guard.upsert(lease.clone())?;
                Ok(Some(lease))
            }
            LabelMatch::Many(ids) => {
                bail!(
                    "multiple Vast instances use label {}: {}. No new instance was created.",
                    lease.label,
                    ids.iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            LabelMatch::None if abandon_unresolved => {
                guard.remove(&lease.profile)?;
                Ok(None)
            }
            LabelMatch::None => {
                bail!(
                    "no instance with label {} is visible. An empty Vast listing is not proof the create failed. No new instance was created. If the console has no instance with that label, run destroy --abandon-unresolved.",
                    lease.label
                );
            }
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
        let instance_id = recorded_id(&lease)?;
        match self
            .client
            .wait_for_instance_ready(instance_id, profile_name)
            .await
        {
            Ok(instance) => {
                if let Some(price) = instance.dph_total {
                    let cap = self.config.vast.max_hourly_price_usd;
                    if price > cap + 0.000_1 {
                        let note = self
                            .destroy_after_failure(guard, &mut lease, instance_id)
                            .await;
                        bail!(
                            "Vast reported ${price:.4}/hr for instance {instance_id}, above the ${cap:.4}/hr cap. {note}"
                        );
                    }
                }
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
                let logs = self.client.request_logs(instance_id).await.ok();
                let log_url = logs.and_then(|logs| logs.result_url).unwrap_or_default();
                let destroy_note = self
                    .destroy_after_failure(guard, &mut lease, instance_id)
                    .await;
                Err(anyhow!(
                    "vast instance {instance_id} failed to become ready: {error:#}. {destroy_note}. logs: {log_url}"
                ))
            }
        }
    }

    async fn destroy_after_failure(
        &self,
        guard: &LeaseGuard,
        lease: &mut LeaseRecord,
        instance_id: u64,
    ) -> String {
        match self.client.destroy_instance(instance_id).await {
            Ok(()) => {
                let _ = guard.remove(&lease.profile);
                "destroyed the instance".to_string()
            }
            Err(destroy_error) if VastClient::missing_instance(&destroy_error) => {
                let _ = guard.remove(&lease.profile);
                "the instance was already gone".to_string()
            }
            Err(destroy_error) => {
                lease.instance_id = Some(instance_id);
                lease.status = STATUS_DESTROY_FAILED.to_string();
                if guard.upsert(lease.clone()).is_err() {
                    format!(
                        "could not destroy instance {instance_id} ({destroy_error:#}) and could not update the lease file. Destroy it in the Vast console."
                    )
                } else {
                    format!(
                        "could not destroy instance {instance_id}: {destroy_error:#}. The local lease was kept so the next command can retry."
                    )
                }
            }
        }
    }

    async fn destroy_recorded(&self, guard: &LeaseGuard, active: &LeaseRecord) -> Result<()> {
        let instance_id = recorded_id(active)?;
        match self.client.destroy_instance(instance_id).await {
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
                        "instance {instance_id} could not be destroyed ({destroy_message}) and the lease file could not record it"
                    )
                })?;
                Err(error)
            }
        }
    }

    async fn keep_or_destroy_after_save_failure(
        &self,
        guard: &LeaseGuard,
        lease: &LeaseRecord,
        instance_id: u64,
        write_error: anyhow::Error,
    ) -> Result<RentOutcome> {
        match self.client.destroy_instance(instance_id).await {
            Ok(()) => {
                let _ = guard.remove(&lease.profile);
                Err(write_error.context(format!(
                    "destroyed instance {instance_id} because the provisioning record could not be written. Label {} remains the thing to look for if the destroy did not stick.",
                    lease.label
                )))
            }
            Err(destroy_error) if VastClient::missing_instance(&destroy_error) => {
                let _ = guard.remove(&lease.profile);
                Err(write_error.context(format!(
                    "instance {instance_id} was already gone and the provisioning record could not be written"
                )))
            }
            Err(destroy_error) => Err(anyhow!(
                "created Vast instance {instance_id} with label {} but could not record the id ({write_error:#}) and could not destroy it ({destroy_error:#}). The create intent was kept. Run status before renting again.",
                lease.label
            )),
        }
    }
}

fn recorded_id(lease: &LeaseRecord) -> Result<u64> {
    lease.instance_id.with_context(|| {
        format!(
            "lease {} ({}) has no Vast instance id",
            lease.lease_id, lease.label
        )
    })
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LabelMatch {
    One(u64),
    None,
    Many(Vec<u64>),
}

pub(crate) fn match_instance_label(label: &str, instances: &[(u64, Option<&str>)]) -> LabelMatch {
    let ids = instances
        .iter()
        .filter(|(_, instance_label)| instance_label.as_deref() == Some(label))
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
    match ids.len() {
        0 => LabelMatch::None,
        1 => LabelMatch::One(ids[0]),
        _ => LabelMatch::Many(ids),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_match_does_not_treat_a_missing_list_as_a_different_instance() {
        let instances = [(7, Some("other-label")), (8, None)];
        assert_eq!(
            match_instance_label("gpu-lease", &instances),
            LabelMatch::None
        );
        assert_eq!(
            match_instance_label("other-label", &instances),
            LabelMatch::One(7)
        );
        let duplicated = [(7, Some("gpu-lease")), (9, Some("gpu-lease"))];
        assert_eq!(
            match_instance_label("gpu-lease", &duplicated),
            LabelMatch::Many(vec![7, 9])
        );
    }
}
