// SPDX-License-Identifier: MIT
//! Per-device AMD counters from the amdgpu kernel driver's sysfs files.
//! Collected on Linux only, without extra tools: the driver publishes VRAM,
//! utilization and temperatures under `/sys/class/drm/card*/device`.

use crate::gpu::Device;
use crate::host::Host;
use std::path::{Path, PathBuf};

const DRM: &str = "/sys/class/drm";
const AMD_VENDOR: &str = "0x1002";
/// Integrated GPUs report their small BIOS carve-out as VRAM; it would read
/// as a full card on an idle desktop. Discrete cards and large carve-outs pass.
const MIN_VRAM: u64 = 1 << 30;
/// pci.ids locations on Debian/Ubuntu/Fedora and Arch, tried in order.
const PCI_IDS: [&str; 2] = ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids"];

/// Every amdgpu card with readable VRAM, in DRM card order. Names are looked
/// up once per card and carried over from `previous`.
pub(crate) fn collect(host: &dyn Host, previous: &[Device]) -> Vec<Device> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }
    let mut cards: Vec<PathBuf> = host
        .read_dir(Path::new(DRM))
        .into_iter()
        .filter(|path| card_number(path).is_some())
        .collect();
    cards.sort_by_key(|path| card_number(path));
    let mut devices = Vec::new();
    for card in cards {
        let dev = card.join("device");
        if read(host, &dev.join("vendor")).as_deref() != Some(AMD_VENDOR) {
            continue;
        }
        let Some(uuid) = identity(host, &dev) else {
            continue;
        };
        let total = number(host, &dev.join("mem_info_vram_total"));
        if total.is_some_and(|total| total < MIN_VRAM) {
            continue;
        }
        let used = number(host, &dev.join("mem_info_vram_used"));
        let name = previous
            .iter()
            .find(|gpu| gpu.uuid == uuid)
            .map(|gpu| gpu.name.clone())
            .unwrap_or_else(|| device_name(host, &dev));
        devices.push(Device {
            index: devices.len() as u32,
            uuid,
            name,
            utilization: number(host, &dev.join("gpu_busy_percent"))
                .filter(|n| *n <= 100)
                .map(|n| n as u8),
            used: used.filter(|used| total.is_none_or(|total| *used <= total)),
            total,
            temperature: temperature(host, &dev),
            throttle_reasons: None,
        });
    }
    devices
}

fn card_number(path: &Path) -> Option<u32> {
    path.file_name()?
        .to_str()?
        .strip_prefix("card")?
        .parse()
        .ok()
}

fn read(host: &dyn Host, path: &Path) -> Option<String> {
    let text = host.read_file(path)?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn number(host: &dyn Host, path: &Path) -> Option<u64> {
    read(host, path)?.parse().ok()
}

/// `AMD-` plus the driver's stable unique id, or the PCI slot on cards that
/// lack one. Either survives DRM renumbering between boots.
fn identity(host: &dyn Host, dev: &Path) -> Option<String> {
    let id = read(host, &dev.join("unique_id")).or_else(|| {
        read(host, &dev.join("uevent"))?
            .lines()
            .find_map(|line| line.strip_prefix("PCI_SLOT_NAME="))
            .map(str::to_owned)
    })?;
    if id.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    Some(format!("AMD-{id}"))
}

/// The marketing name from the driver when present, else from pci.ids
/// (`Navi 48 [Radeon RX 9070/9070 XT/9070 GRE]` becomes `AMD Radeon RX
/// 9070/9070 XT/9070 GRE`), else the PCI device id.
fn device_name(host: &dyn Host, dev: &Path) -> String {
    let clean =
        |name: &str| -> String { name.chars().filter(|c| !c.is_control()).take(120).collect() };
    if let Some(name) = read(host, &dev.join("product_name")) {
        return clean(&name);
    }
    let device = read(host, &dev.join("device"));
    let id = device
        .as_deref()
        .and_then(|id| id.strip_prefix("0x"))
        .map(str::to_ascii_lowercase);
    if let Some(id) = &id {
        let found = PCI_IDS
            .iter()
            .find_map(|path| host.read_file(Path::new(path)))
            .and_then(|ids| pci_ids_name(&ids, id));
        if let Some(name) = found {
            return clean(&format!("AMD {name}"));
        }
    }
    format!("AMD GPU 1002:{}", id.unwrap_or_else(|| "????".into()))
}

/// The device line under vendor 1002 in pci.ids, preferring the bracketed
/// product name over the chip codename.
pub(crate) fn pci_ids_name(ids: &str, device: &str) -> Option<String> {
    let mut in_amd = false;
    for line in ids.lines() {
        // Comments sit inside vendor blocks and must not end them.
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if !line.starts_with('\t') {
            in_amd = line.starts_with("1002 ");
            continue;
        }
        if !in_amd || line.starts_with("\t\t") {
            continue;
        }
        let Some((id, name)) = line[1..].split_once(char::is_whitespace) else {
            continue;
        };
        if id.eq_ignore_ascii_case(device) {
            let name = name.trim();
            let product = name
                .split_once('[')
                .and_then(|(_, rest)| rest.rsplit_once(']'))
                .map(|(inner, _)| inner.trim())
                .filter(|inner| !inner.is_empty());
            return Some(product.unwrap_or(name).to_owned());
        }
    }
    None
}

/// The edge sensor, the closest match to the core temperature NVIDIA
/// reports; the first hwmon reading when no sensor is labelled.
fn temperature(host: &dyn Host, dev: &Path) -> Option<u64> {
    let mut first = None;
    for hwmon in host.read_dir(&dev.join("hwmon")) {
        for sensor in 1..=3 {
            let Some(millidegrees) = number(host, &hwmon.join(format!("temp{sensor}_input")))
            else {
                continue;
            };
            let celsius = millidegrees / 1000;
            let label = read(host, &hwmon.join(format!("temp{sensor}_label")));
            if label.as_deref() == Some("edge") {
                return Some(celsius);
            }
            first.get_or_insert(celsius);
        }
    }
    first
}

#[cfg(test)]
#[path = "tests/gpu_amd.rs"]
mod tests;
