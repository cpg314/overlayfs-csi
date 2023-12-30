use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tracing::*;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

pub mod v1 {
    tonic::include_proto!("csi.v1");
}

#[derive(Parser)]
struct Flags {
    #[clap(flatten)]
    overlay: overlayfs_csi::OverlayFlags,
    #[clap(long, alias = "endpoint")]
    socket: PathBuf,
    #[clap(long, short)]
    debug: bool,
}

fn unimplemented() -> tonic::Status {
    tonic::Status::unimplemented("Unimplemented")
}

/// Service that simply provides information about the CSI driver
struct IdentityService {
    name: String,
}
#[async_trait::async_trait]
impl v1::identity_server::Identity for IdentityService {
    async fn get_plugin_info(
        &self,
        req: tonic::Request<v1::GetPluginInfoRequest>,
    ) -> Result<tonic::Response<v1::GetPluginInfoResponse>, tonic::Status> {
        info!("Returning plugin info");
        let req = req.into_inner();
        debug!("{:?}", req);
        Ok(tonic::Response::new(v1::GetPluginInfoResponse {
            name: self.name.clone(),
            vendor_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        }))
    }
    async fn get_plugin_capabilities(
        &self,
        _request: tonic::Request<v1::GetPluginCapabilitiesRequest>,
    ) -> Result<tonic::Response<v1::GetPluginCapabilitiesResponse>, tonic::Status> {
        Ok(tonic::Response::new(Default::default()))
    }
    async fn probe(
        &self,
        _request: tonic::Request<v1::ProbeRequest>,
    ) -> Result<tonic::Response<v1::ProbeResponse>, tonic::Status> {
        Ok(tonic::Response::new(v1::ProbeResponse {
            ready: Some(true),
        }))
    }
}
struct NodeService {
    node_id: String,
    overlays: Arc<overlayfs_csi::Overlays>,
}
#[async_trait::async_trait]
impl v1::node_server::Node for NodeService {
    async fn node_stage_volume(
        &self,
        _req: tonic::Request<v1::NodeStageVolumeRequest>,
    ) -> tonic::Result<tonic::Response<v1::NodeStageVolumeResponse>> {
        Err(unimplemented())
    }
    async fn node_unstage_volume(
        &self,
        _req: tonic::Request<v1::NodeUnstageVolumeRequest>,
    ) -> tonic::Result<tonic::Response<v1::NodeUnstageVolumeResponse>> {
        Err(unimplemented())
    }

    async fn node_publish_volume(
        &self,
        req: tonic::Request<v1::NodePublishVolumeRequest>,
    ) -> tonic::Result<tonic::Response<v1::NodePublishVolumeResponse>> {
        let req = req.into_inner();
        info!(req.volume_id, ?req.target_path, "Publishing volume");
        debug!("{:?}", req);
        match self.overlays.mount(&req.volume_id, req.target_path).await {
            Ok(()) => Ok(tonic::Response::new(Default::default())),
            Err(e) => {
                error!(req.volume_id, "Failed publishing: {}", e);
                Err(tonic::Status::internal(e.to_string()))
            }
        }
    }
    async fn node_unpublish_volume(
        &self,
        req: tonic::Request<v1::NodeUnpublishVolumeRequest>,
    ) -> tonic::Result<tonic::Response<v1::NodeUnpublishVolumeResponse>> {
        let req = req.into_inner();
        info!(
            req.volume_id, ?req.target_path,
            "Unpublishing volume"
        );
        debug!("{:?}", req);
        match self.overlays.unmount(&req.volume_id, req.target_path).await {
            Ok(()) => Ok(tonic::Response::new(Default::default())),
            Err(e) => {
                error!(req.volume_id, "Failed unpublishing: {}", e);
                Err(tonic::Status::internal(e.to_string()))
            }
        }
    }
    async fn node_get_volume_stats(
        &self,
        _req: tonic::Request<v1::NodeGetVolumeStatsRequest>,
    ) -> tonic::Result<tonic::Response<v1::NodeGetVolumeStatsResponse>> {
        Err(unimplemented())
    }
    async fn node_expand_volume(
        &self,
        _req: tonic::Request<v1::NodeExpandVolumeRequest>,
    ) -> tonic::Result<tonic::Response<v1::NodeExpandVolumeResponse>> {
        Err(unimplemented())
    }
    async fn node_get_capabilities(
        &self,
        _req: tonic::Request<v1::NodeGetCapabilitiesRequest>,
    ) -> tonic::Result<tonic::Response<v1::NodeGetCapabilitiesResponse>> {
        Ok(tonic::Response::new(Default::default()))
    }
    async fn node_get_info(
        &self,
        _req: tonic::Request<v1::NodeGetInfoRequest>,
    ) -> tonic::Result<tonic::Response<v1::NodeGetInfoResponse>> {
        Ok(tonic::Response::new(v1::NodeGetInfoResponse {
            node_id: self.node_id.clone(),
            ..Default::default()
        }))
    }
}

async fn main_impl(args: Flags) -> anyhow::Result<()> {
    info!("Connecting to Kubernetes API");
    let kube_client = kube::Client::try_default().await?;
    let pods: Api<Pod> = Api::namespaced(kube_client, &args.overlay.namespace);
    let identity_service = IdentityService {
        name: args.overlay.name.clone(),
    };
    let node_service = NodeService {
        node_id: args.overlay.node.clone(),
        overlays: overlayfs_csi::Overlays::from_flags(args.overlay, pods).await?,
    };

    info!("Connecting to socket {:?}", args.socket);
    let _ = std::fs::remove_file(&args.socket);
    let uds = UnixListener::bind(&args.socket)?;
    let uds_stream = UnixListenerStream::new(uds);

    info!("Started server on socket {:?}", args.socket);
    let layer = tower::ServiceBuilder::new().into_inner();

    let mut builder = tonic::transport::Server::builder().layer(layer);
    builder
        .add_service(v1::node_server::NodeServer::new(node_service))
        .add_service(v1::identity_server::IdentityServer::new(identity_service))
        .serve_with_incoming(uds_stream)
        .await?;

    Ok(())
}

#[tokio::main]
async fn main() {
    let args = Flags::parse();
    tracing_subscriber::registry()
        .with(Some(tracing_subscriber::fmt::layer().with_filter(
            if args.debug {
                LevelFilter::DEBUG
            } else {
                LevelFilter::INFO
            },
        )))
        .init();

    if let Err(e) = main_impl(args).await {
        error!("{:#?}", e);
        std::process::exit(1);
    }
}
