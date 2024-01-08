use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use futures::{StreamExt, TryStreamExt};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{DeleteParams, WatchEvent, WatchParams};
use kube::Api;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tracing::*;

const BASE_CLEANUP_FREQ_S: u64 = 30;

#[derive(Parser)]
pub struct OverlayFlags {
    /// CSI name
    #[clap(long)]
    pub name: String,
    #[clap(long, alias = "nodeid")]
    pub node: String,
    #[clap(long, short)]
    pub namespace: String,
    #[clap(long)]
    bases: PathBuf,
    #[clap(long, default_value = "/var/lib/kubelet/pods")]
    pods: PathBuf,
    #[clap(long)]
    max_age_s: i64,
    /// Size per volume
    #[clap(long)]
    size_limit: String,
}
/// Base for the overlays
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct Base(PathBuf);
impl Base {
    /// Marker for volumes that can be transformed into bases.
    /// Once transformer, the file contains the creation date.
    fn as_base_filename() -> &'static str {
        ".as_base"
    }
    fn as_base_file(&self) -> PathBuf {
        self.0.join(Self::as_base_filename())
    }
    fn write_time(&self) -> anyhow::Result<()> {
        std::fs::write(
            self.as_base_file(),
            OffsetDateTime::now_utc().format(&Rfc3339)?,
        )?;
        Ok(())
    }
    fn read_time(&self) -> anyhow::Result<OffsetDateTime> {
        let data = std::fs::read_to_string(self.0.join(self.as_base_file()))?;
        Ok(OffsetDateTime::parse(&data, &Rfc3339)?)
    }
    /// Check if a base is younger than `max_age_s`.
    fn valid(&self, max_age_s: i64) -> bool {
        let Ok(dt) = self.read_time() else {
            return false;
        };
        let age = OffsetDateTime::now_utc() - dt;
        if age.is_negative() {
            warn!(?self, "Base in the future");
            false
        } else if age.whole_seconds() < max_age_s {
            true
        } else {
            debug!(s = age.whole_seconds(), max_age_s, ?self, "Too old base");
            false
        }
    }
}
pub struct Overlays {
    // {workdir}/bases/{id}
    //          /volumes/{id}/upper
    //                       /work
    flags: OverlayFlags,
    pods: Api<Pod>,
    // To avoid spurious cross-device errors when we move volumes into bases, we retrieve the path
    // where the `bases` volume is present on the host, which should be on the same device as the
    // `pods` folder.
    bases_host: PathBuf,
    lock: Mutex<HashMap<Base, HashSet<String> /* volumes */>>,
}
struct PodUid(String);
impl AsRef<Path> for PodUid {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}
impl Overlays {
    pub async fn from_flags(flags: OverlayFlags, pods: Api<Pod>) -> anyhow::Result<Arc<Self>> {
        let mut overlays = Self {
            flags,
            pods,
            bases_host: Default::default(),
            lock: Default::default(),
        };
        overlays.bases_host = overlays.empty_dir(
            PodUid(std::env::var("POD_ID").context("Failed to find pod ID from environment")?),
            "bases",
        );
        let overlays = Arc::new(overlays);
        // Cleanup thread
        tokio::task::spawn({
            let overlays = overlays.clone();
            async move {
                loop {
                    if let Err(e) = overlays.cleanup().await {
                        error!("Failed to cleanup bases: {}", e);
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(BASE_CLEANUP_FREQ_S)).await;
                }
            }
        });
        Ok(overlays)
    }
    fn empty_dir(&self, pod_uid: PodUid, volume: &str) -> PathBuf {
        self.flags
            .pods
            .join(pod_uid)
            .join("volumes")
            .join("kubernetes.io~empty-dir")
            .join(volume)
    }
    fn volume_dir(&self, pod_uid: PodUid) -> PathBuf {
        self.empty_dir(pod_uid, "volume")
    }
    async fn base_host(&self, id: &str) -> anyhow::Result<Base> {
        Ok(Base(self.bases_host.join(id)))
    }
    fn bases(&self) -> anyhow::Result<impl Iterator<Item = Base>> {
        Ok(std::fs::read_dir(&self.flags.bases)?
            .filter_map(Result::ok)
            .filter(|x| x.file_type().map_or(false, |t| t.is_dir()))
            .map(|x| Base(x.path().to_owned())))
    }
    fn find_valid_base(&self) -> anyhow::Result<Option<Base>> {
        Ok(self.bases()?.find(|base| base.valid(self.flags.max_age_s)))
    }
    async fn delete_pod(&self, id: &str) -> anyhow::Result<()> {
        info!(id, "Deleting pod");
        self.pods.delete(id, &DeleteParams::background()).await?;
        Ok(())
    }
    async fn watch_pod(&self, id: &str) -> anyhow::Result<()> {
        let mut watch = self
            .pods
            .watch(
                &WatchParams::default().fields(&format!("metadata.name={}", id)),
                "0",
            )
            .await?
            .boxed();
        while let Some(status) = watch.try_next().await? {
            if let WatchEvent::Modified(pod) = status {
                if pod
                    .status
                    .and_then(|status| status.phase)
                    .map_or(false, |phase| phase == "Running")
                {
                    info!(id, uid = pod.metadata.uid.unwrap(), "Pod was created");
                    return Ok(());
                }
            }
        }
        Ok(())
    }
    async fn create_pod(&self, id: &str) -> anyhow::Result<PodUid> {
        info!(id, "Creating pod to allocate storage");
        let mut pod: Pod = serde_yaml::from_str(include_str!("../data_pod.yaml"))?;
        pod.metadata.name = Some(id.into());
        pod.metadata.namespace = Some(self.flags.namespace.clone());
        let spec = pod.spec.as_mut().unwrap();
        spec.volumes.as_mut().unwrap()[0]
            .empty_dir
            .as_mut()
            .unwrap()
            .size_limit = Some(Quantity(self.flags.size_limit.clone()));
        spec.node_name = Some(self.flags.node.clone());
        let pod = self.pods.create(&Default::default(), &pod).await?;
        let uid = pod.metadata.uid.unwrap();
        info!(id, uid, "Waiting for pod to get created");
        loop {
            match self.watch_pod(id).await {
                Ok(()) => {
                    return Ok(PodUid(uid));
                }
                Err(e) => {
                    error!(
                        id,
                        "Watching for pod creation failed ({}), restarting watch", e
                    );
                }
            }
        }
    }
    pub async fn mount(&self, id: &str, mountpoint: impl AsRef<Path>) -> anyhow::Result<()> {
        let mountpoint = mountpoint.as_ref();
        let pod_uid = self.create_pod(id).await?;
        let volume_dir = self.volume_dir(pod_uid);

        let mut mapping = self.lock.lock().await;
        std::fs::create_dir_all(mountpoint)?;
        if let Some(base) = self.find_valid_base()? {
            // A base is available, we create an overlay
            info!(id, ?mountpoint, ?base, "Creating overlay",);
            let upper = volume_dir.join("upper");
            let workdir = volume_dir.join("workdir");
            for d in [&upper, &workdir] {
                std::fs::create_dir_all(d)?;
            }
            duct::cmd!(
                "mount",
                "-t",
                "overlay",
                id,
                "-o",
                format!(
                    "lowerdir={},upperdir={},workdir={}",
                    base.0.as_os_str().to_str().unwrap(),
                    upper.as_os_str().to_str().unwrap(),
                    workdir.as_os_str().to_str().unwrap()
                ),
                mountpoint
            )
            .run()?;
            mapping.entry(base).or_default().insert(id.to_string());
        } else {
            // If no base is available, we create a volume with a bind mount
            warn!(id, "Could not find a base, creating a volume from scratch");
            std::fs::create_dir_all(mountpoint)?;
            std::fs::create_dir_all(&volume_dir)?;
            duct::cmd!("mount", "--bind", volume_dir, mountpoint).run()?;
        }
        debug!(?mapping);
        Ok(())
    }
    pub async fn cleanup(&self) -> anyhow::Result<()> {
        let mut mapping = self.lock.lock().await;
        debug!("Cleaning up bases");
        for base in self.bases()?.filter(|b| !b.valid(self.flags.max_age_s)) {
            // We only clean up bases not tied to a volume.
            // The base might not be in the mapping if it has never been associated with a volume.
            if mapping.entry(base.clone()).or_default().is_empty() {
                warn!(?base, "Cleaning up");
                std::fs::remove_dir_all(&base.0)?;
                mapping.remove(&base);
            }
        }
        Ok(())
    }
    pub async fn unmount(&self, id: &str, mountpoint: impl AsRef<Path>) -> anyhow::Result<()> {
        let mut mapping = self.lock.lock().await;
        let mountpoint = mountpoint.as_ref();
        let is_overlay = mapping.values().flatten().any(|v| v == id);
        let no_valid_base = self.find_valid_base().map_or(true, |o| o.is_none());
        info!(id, ?mountpoint, is_overlay, no_valid_base, "Unmounting");
        // If this can be used as a base and we need one, transform it
        // TODO: We could also do that a bit before the previous base has expired.
        if !is_overlay && no_valid_base {
            // Get the volume path from the pod
            let pod: Pod = self.pods.get(id).await?;
            let volume_dir = self.volume_dir(PodUid(pod.metadata.uid.unwrap()));
            let as_base = volume_dir.join(Base::as_base_filename());
            if as_base.exists() {
                let base = self.base_host(id).await?;
                info!(id, ?mountpoint, src=?volume_dir, dst=?base.0, "Transforming volume into base");
                std::fs::rename(volume_dir, &base.0)?;
                base.write_time()?;
            } else {
                warn!(
                    id,
                    "Not transforming into base as {:?} does not exist", as_base
                );
            }
        }
        // Update the mapping so that the base can be cleaned up if necessary.
        for volumes in mapping.values_mut() {
            volumes.remove(id);
        }
        duct::cmd!("umount", "-f", mountpoint).unchecked().run()?;
        debug!(?mapping);
        drop(mapping);
        // Kubernetes will clean up the pod storage
        self.delete_pod(id).await?;
        Ok(())
    }
}
