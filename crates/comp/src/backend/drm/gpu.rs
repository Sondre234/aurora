use std::path::{Path, PathBuf};

use smithay::backend::{
    drm::{DrmNode, NodeType},
    udev::{all_gpus, primary_gpu},
};

/// Kernel driver bound to a card, from the sysfs `driver` symlink.
fn sysfs_driver(card: &Path) -> Option<String> {
    let name = card.file_name()?;
    let link = Path::new("/sys/class/drm").join(name).join("device/driver");
    let target = std::fs::read_link(link).ok()?;
    Some(target.file_name()?.to_string_lossy().into_owned())
}

fn sysfs_boot_vga(card: &Path) -> bool {
    let Some(name) = card.file_name() else {
        return false;
    };
    let path = Path::new("/sys/class/drm")
        .join(name)
        .join("device/boot_vga");
    std::fs::read_to_string(path).is_ok_and(|s| s.trim() == "1")
}

fn primary_node(path: &Path) -> Result<DrmNode, String> {
    let node = DrmNode::from_path(path)
        .map_err(|err| format!("{} is not a drm node: {err}", path.display()))?;
    // Everything downstream is keyed by the card node, even when handed a render node.
    match node.node_with_type(NodeType::Primary) {
        Some(Ok(primary)) => Ok(primary),
        _ => Ok(node),
    }
}

/// Picks the GPU that drives the session: `AURORA_DRM_DEVICE`, else an nvidia card
/// (the boot_vga iGPU is often not the one we want), else udev's primary GPU.
pub fn select_primary(seat: &str) -> Result<DrmNode, String> {
    if let Ok(path) = std::env::var("AURORA_DRM_DEVICE") {
        let node = primary_node(Path::new(&path))?;
        tracing::info!(%node, %path, "primary gpu from AURORA_DRM_DEVICE");
        return Ok(node);
    }

    let candidates: Vec<PathBuf> =
        all_gpus(seat).map_err(|err| format!("could not enumerate gpus for {seat}: {err}"))?;
    if candidates.is_empty() {
        return Err(format!("no drm devices found on seat {seat}"));
    }
    for path in &candidates {
        tracing::info!(
            path = %path.display(),
            driver = sysfs_driver(path).as_deref().unwrap_or("unknown"),
            boot_vga = sysfs_boot_vga(path),
            "gpu candidate"
        );
    }

    let nvidia = candidates
        .iter()
        .find(|path| sysfs_driver(path).as_deref() == Some("nvidia"));
    let udev_primary = || primary_gpu(seat).ok().flatten();
    let chosen = nvidia
        .cloned()
        .or_else(udev_primary)
        .or_else(|| candidates.first().cloned())
        .ok_or_else(|| "no usable gpu".to_string())?;

    let node = primary_node(&chosen)?;
    tracing::info!(%node, path = %chosen.display(), "selected primary gpu");
    Ok(node)
}
