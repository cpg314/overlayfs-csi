use std::env;
use std::path::Path;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let out_dir = Path::new(&out_dir);

    let csi = out_dir.join("csi");
    std::fs::create_dir_all(&csi)?;

    let data = reqwest::get(
"https://raw.githubusercontent.com/container-storage-interface/spec/b01039c563108173c6743aa1410ec11fde7c24fe/csi.proto"
    )
    .await?
    .error_for_status()?
    .bytes()
    .await?;
    let proto = csi.join("csi.proto");
    std::fs::write(&proto, data)?;

    tonic_build::configure()
        .build_server(true)
        .emit_rerun_if_changed(false)
        .compile(&[proto], &[csi])?;

    println!("cargo:rerun-if-changed=build.rs");

    Ok(())
}
