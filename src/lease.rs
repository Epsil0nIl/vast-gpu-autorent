use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};

pub const STATUS_PROVISIONING: &str = "provisioning";
pub const STATUS_DESTROY_FAILED: &str = "destroy_failed";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LeaseRecord {
    pub profile: String,
    pub lease_id: String,
    pub instance_id: u64,
    pub offer_id: u64,
    pub gpu_name: Option<String>,
    pub geolocation: Option<String>,
    pub host: Option<String>,
    pub ssh_port: Option<u16>,
    pub public_ip: Option<String>,
    pub hourly_price_usd: f64,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct LeaseStore {
    path: PathBuf,
}

pub struct LeaseGuard {
    path: PathBuf,
    _lock: File,
}

impl LeaseStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub async fn exclusive(&self) -> Result<LeaseGuard> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.lock_blocking())
            .await
            .context("lease lock task failed")?
    }

    pub fn lock_blocking(&self) -> Result<LeaseGuard> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        let lock_path = lock_path(&self.path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("locking {}", lock_path.display()))?;
        Ok(LeaseGuard {
            path: self.path.clone(),
            _lock: file,
        })
    }
}

impl LeaseGuard {
    pub fn get(&self, profile: &str) -> Result<Option<LeaseRecord>> {
        Ok(self.load()?.remove(profile))
    }

    pub fn upsert(&self, record: LeaseRecord) -> Result<()> {
        let mut leases = self.load()?;
        leases.insert(record.profile.clone(), record);
        self.save(&leases)
    }

    pub fn remove(&self, profile: &str) -> Result<Option<LeaseRecord>> {
        let mut leases = self.load()?;
        let removed = leases.remove(profile);
        self.save(&leases)?;
        Ok(removed)
    }

    fn load(&self) -> Result<BTreeMap<String, LeaseRecord>> {
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        let text = fs::read_to_string(&self.path)
            .with_context(|| format!("reading lease file {}", self.path.display()))?;
        if text.trim().is_empty() {
            return Ok(BTreeMap::new());
        }
        serde_json::from_str(&text)
            .with_context(|| format!("parsing lease file {}", self.path.display()))
    }

    fn save(&self, leases: &BTreeMap<String, LeaseRecord>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        let tmp = temp_path(&self.path);
        let body = serde_json::to_vec_pretty(leases).context("encoding lease file")?;
        fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {} with {}", self.path.display(), tmp.display()))?;
        Ok(())
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

fn temp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

pub fn runtime_exceeded(created_at: DateTime<Utc>, max_hours: f64, now: DateTime<Utc>) -> bool {
    if !max_hours.is_finite() || max_hours <= 0.0 {
        return false;
    }
    let elapsed = now.signed_duration_since(created_at).num_milliseconds();
    if elapsed < 0 {
        return false;
    }
    elapsed as f64 >= max_hours * 3_600_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> LeaseRecord {
        LeaseRecord {
            profile: "gpu".to_string(),
            lease_id: "lease-1".to_string(),
            instance_id: 9,
            offer_id: 3,
            gpu_name: Some("RTX 3090".to_string()),
            geolocation: None,
            host: Some("ssh.example".to_string()),
            ssh_port: Some(22),
            public_ip: None,
            hourly_price_usd: 0.2,
            status: "running".to_string(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn lease_roundtrip_keeps_one_record_per_profile() {
        let dir =
            std::env::temp_dir().join(format!("vast-gpu-autorent-lease-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let store = LeaseStore::new(dir.join("leases.json"));
        let guard = store.lock_blocking().unwrap();
        guard.upsert(record()).unwrap();
        assert_eq!(guard.get("gpu").unwrap().unwrap().instance_id, 9);
        guard.remove("gpu").unwrap();
        assert!(guard.get("gpu").unwrap().is_none());
        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_process_cannot_take_the_lock() {
        let dir =
            std::env::temp_dir().join(format!("vast-gpu-autorent-lock-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let store = LeaseStore::new(dir.join("leases.json"));
        let held = store.lock_blocking().unwrap();
        let mut lock_path = dir.join("leases.json").into_os_string();
        lock_path.push(".lock");
        let blocked = std::process::Command::new("flock")
            .arg("--nonblock")
            .arg("--exclusive")
            .arg(&lock_path)
            .arg("true")
            .status()
            .expect("flock");
        assert!(
            !blocked.success(),
            "a second process acquired the lease lock"
        );
        drop(held);
        let free = std::process::Command::new("flock")
            .arg("--nonblock")
            .arg("--exclusive")
            .arg(&lock_path)
            .arg("true")
            .status()
            .expect("flock");
        assert!(
            free.success(),
            "lock was not released when the guard dropped"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_limit_uses_elapsed_time() {
        let created = Utc::now();
        let now = created + chrono::Duration::hours(2);
        assert!(!runtime_exceeded(created, 3.0, now));
        assert!(runtime_exceeded(created, 2.0, now));
        assert!(!runtime_exceeded(created, 2.0, created));
    }
}
