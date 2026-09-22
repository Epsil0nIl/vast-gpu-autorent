use std::{env, path::PathBuf, process::ExitCode};

use anyhow::{bail, Context, Result};
use vast_gpu_autorent::{AppConfig, LeaseOutcome, LeaseRecord, Renter};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::from(1)
        }
    }
}

#[tokio::main]
async fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        println!("{}", usage());
        return Ok(());
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        println!("{}", usage());
        return Ok(());
    }
    if matches!(command.as_str(), "-V" | "--version") {
        println!("vast-gpu-autorent {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let mut config_path = PathBuf::from("config.toml");
    let mut profile_name = None;
    let mut force_replace = false;
    let mut limit = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config_path = PathBuf::from(args.next().context("--config needs a path")?);
            }
            "--profile" => {
                profile_name = Some(args.next().context("--profile needs a name")?);
            }
            "--force-replace" => force_replace = true,
            "--limit" => {
                let raw = args.next().context("--limit needs a number")?;
                limit = Some(
                    raw.parse::<usize>()
                        .with_context(|| format!("invalid --limit {raw}"))?,
                );
            }
            other => bail!("unknown argument {other}\n{}", usage()),
        }
    }

    let config = AppConfig::load(&config_path)?;
    config.require_api_key()?;
    let profile = profile_name
        .as_deref()
        .unwrap_or(config.default_profile.as_str())
        .to_string();
    let renter = Renter::new(config)?;

    match command.as_str() {
        "search" => {
            println!("{}", renter.price_caveat(&profile)?);
            let offers = renter.search(&profile).await?;
            let shown = match limit {
                Some(limit) => offers.into_iter().take(limit).collect::<Vec<_>>(),
                None => offers,
            };
            if shown.is_empty() {
                println!("no offers matched profile {profile}");
                return Ok(());
            }
            println!(
                "profile {profile}: {} offer(s), cheapest valid offer first",
                shown.len()
            );
            for (index, offer) in shown.iter().enumerate() {
                let marker = if index == 0 { "rent" } else { "    " };
                println!(
                    "{marker} #{:<2} offer {:<8} ${:.4}/hr  {:<12} {:>5} MB  rel {:<5} {}",
                    index + 1,
                    offer.id,
                    offer.dph_total,
                    offer.gpu_name.as_deref().unwrap_or("-"),
                    offer.gpu_ram,
                    offer
                        .reliability2
                        .map(|value| format!("{value:.3}"))
                        .unwrap_or_else(|| "-".to_string()),
                    offer.geolocation.as_deref().unwrap_or("-"),
                );
            }
        }
        "rent" => {
            println!("{}", renter.price_caveat(&profile)?);
            let outcome = renter.rent(&profile, force_replace).await?;
            if outcome.already_active {
                println!("profile {profile} already has a lease. pass --force-replace to destroy it and rent again.");
            } else if outcome.resumed_provisioning {
                println!("resumed provisioning for profile {profile}");
            } else {
                println!("rented profile {profile}");
            }
            print_lease(&outcome.lease);
        }
        "status" => print_outcome(&renter.status(&profile).await?),
        "destroy" => print_outcome(&renter.destroy(&profile).await?),
        "logs" => {
            let logs = renter.logs(&profile).await?;
            println!("success={}", logs.success);
            if let Some(url) = logs.result_url {
                println!("result_url={url}");
            }
            if let Some(message) = logs.msg {
                println!("msg={message}");
            }
        }
        other => bail!("unknown command {other}\n{}", usage()),
    }
    Ok(())
}

fn print_outcome(outcome: &LeaseOutcome) {
    if let Some(message) = &outcome.message {
        println!("{message}");
    }
    if let Some(lease) = &outcome.lease {
        print_lease(lease);
    }
}

fn print_lease(lease: &LeaseRecord) {
    println!("profile={}", lease.profile);
    println!("lease_id={}", lease.lease_id);
    println!("instance_id={}", lease.instance_id);
    println!("offer_id={}", lease.offer_id);
    println!("status={}", lease.status);
    println!("hourly_price_usd={:.4}", lease.hourly_price_usd);
    println!("gpu={}", lease.gpu_name.as_deref().unwrap_or("-"));
    println!(
        "geolocation={}",
        lease.geolocation.as_deref().unwrap_or("-")
    );
    println!("host={}", lease.host.as_deref().unwrap_or("-"));
    println!(
        "ssh_port={}",
        lease
            .ssh_port
            .map(|port| port.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!("public_ip={}", lease.public_ip.as_deref().unwrap_or("-"));
}

fn usage() -> String {
    format!(
        "\
vast-gpu-autorent {version}
Search Vast.ai for an on-demand GPU and rent the cheapest offer under your hourly cap.

The cap includes the configured disk. It does not include bandwidth or, unless
max_runtime_hours is set, how long the instance runs.

Usage:
  vast-gpu-autorent search  [--config path] [--profile name] [--limit N]
  vast-gpu-autorent rent    [--config path] [--profile name] [--force-replace]
  vast-gpu-autorent status  [--config path] [--profile name]
  vast-gpu-autorent destroy [--config path] [--profile name]
  vast-gpu-autorent logs    [--config path] [--profile name]

Config defaults to ./config.toml. Copy config.example.toml and export VAST_API_KEY.
search lists offers. rent creates a paid Vast.ai instance.

Vast.ai referral link: https://cloud.vast.ai/?ref_id=710527",
        version = env!("CARGO_PKG_VERSION")
    )
}
