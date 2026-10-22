// SPDX-License-Identifier: MIT
use super::*;
use crate::test_support::FakeHost;

/// Excerpt of pci.ids around an RX 9070, with the comment lines the real
/// file has inside the AMD block.
const PCI_IDS_EXCERPT: &str = "\
# List of PCI ID's
1000  Broadcom / LSI
\t0001  53c810
1002  Advanced Micro Devices, Inc. [AMD/ATI]
# Used in the Steam Deck OLED
\t1435  Aerith
\t7550  Navi 48 [Radeon RX 9070/9070 XT/9070 GRE]
\t\t1da2 e490  Navi 48 XTX [Sapphire Pulse Radeon RX 9070 XT]
\t7551  Navi 48 XL
1003  ULSI Systems
\t0201  US201
";

/// One RX 9070 as the amdgpu driver presents it under sysfs on Linux,
/// readings recorded from a real card.
fn rx9070(host: FakeHost, card: &str, unique_id: &str) -> FakeHost {
    let dev = format!("/sys/class/drm/{card}/device");
    let hwmon = format!("{dev}/hwmon/hwmon3");
    host.file(&format!("{dev}/vendor"), "0x1002\n")
        .file(&format!("{dev}/device"), "0x7550\n")
        .file(&format!("{dev}/unique_id"), unique_id)
        .file(&format!("{dev}/mem_info_vram_total"), "17095983104\n")
        .file(&format!("{dev}/mem_info_vram_used"), "2323398656\n")
        .file(&format!("{dev}/gpu_busy_percent"), "5\n")
        .dir(&format!("{dev}/hwmon"), &[&hwmon])
        .file(&format!("{hwmon}/temp1_label"), "edge\n")
        .file(&format!("{hwmon}/temp1_input"), "43000\n")
        .file(&format!("{hwmon}/temp2_label"), "junction\n")
        .file(&format!("{hwmon}/temp2_input"), "47000\n")
        .file("/usr/share/hwdata/pci.ids", PCI_IDS_EXCERPT)
}

#[test]
fn pci_ids_prefers_the_product_name_and_skips_comments() {
    assert_eq!(
        pci_ids_name(PCI_IDS_EXCERPT, "7550").as_deref(),
        Some("Radeon RX 9070/9070 XT/9070 GRE")
    );
    assert_eq!(
        pci_ids_name(PCI_IDS_EXCERPT, "7551").as_deref(),
        Some("Navi 48 XL")
    );
    assert_eq!(
        pci_ids_name(PCI_IDS_EXCERPT, "1435").as_deref(),
        Some("Aerith")
    );
    // Device ids of other vendors and subsystem rows never match.
    assert_eq!(pci_ids_name(PCI_IDS_EXCERPT, "0001"), None);
    assert_eq!(pci_ids_name(PCI_IDS_EXCERPT, "1da2"), None);
    assert_eq!(pci_ids_name("", "7550"), None);
}

#[cfg(target_os = "linux")]
#[test]
fn reads_a_discrete_card_from_sysfs() {
    let host = rx9070(
        FakeHost::default().dir(
            "/sys/class/drm",
            &[
                "/sys/class/drm/card1",
                "/sys/class/drm/card1-DP-1",
                "/sys/class/drm/renderD128",
                "/sys/class/drm/version",
            ],
        ),
        "card1",
        "5b7c23d1d3025b6c\n",
    );
    let cards = collect(&host, &[]);
    assert_eq!(cards.len(), 1);
    let card = &cards[0];
    assert_eq!(card.index, 0);
    assert_eq!(card.uuid, "AMD-5b7c23d1d3025b6c");
    assert_eq!(card.vendor, Vendor::Amd);
    assert_eq!(card.name, "AMD Radeon RX 9070/9070 XT/9070 GRE");
    assert_eq!(card.utilization, Some(5));
    assert_eq!(card.used, Some(2323398656));
    assert_eq!(card.total, Some(17095983104));
    assert_eq!(card.memory_percent(), Some(13));
    // The edge sensor, not the hotter junction.
    assert_eq!(card.temperature, Some(43));
    assert_eq!(card.throttle_reasons, None);
}

#[cfg(target_os = "linux")]
#[test]
fn a_sleeping_card_is_listed_without_waking_it() {
    let dev = "/sys/class/drm/card1/device";
    let host = |status: &str| {
        rx9070(
            FakeHost::default().dir("/sys/class/drm", &["/sys/class/drm/card1"]),
            "card1",
            "5b7c23d1d3025b6c\n",
        )
        .file(
            &format!("{dev}/power/runtime_status"),
            &format!("{status}\n"),
        )
    };
    for status in ["suspended", "suspending", "resuming", "error"] {
        let host = host(status);
        let card = &collect(&host, &[])[0];
        assert_eq!(card.uuid, "AMD-5b7c23d1d3025b6c");
        // VRAM counters never resume the card; busy and temperature reads
        // do on kernels before 6.13, so they are not touched at all.
        assert_eq!(card.used, Some(2323398656));
        assert_eq!(card.utilization, None);
        assert_eq!(card.temperature, None);
        assert!(
            !host
                .calls()
                .iter()
                .any(|call| call.contains("gpu_busy_percent") || call.contains("hwmon")),
            "{status}"
        );
    }
    // Runtime PM switched off keeps the card powered, so it is read.
    for status in ["active", "unsupported"] {
        let host = host(status);
        let card = &collect(&host, &[])[0];
        assert_eq!(card.utilization, Some(5));
        assert_eq!(card.temperature, Some(43));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn skips_other_vendors_and_small_integrated_carve_outs() {
    let host = FakeHost::default()
        .dir(
            "/sys/class/drm",
            &["/sys/class/drm/card0", "/sys/class/drm/card2"],
        )
        // An Intel iGPU.
        .file("/sys/class/drm/card0/device/vendor", "0x8086\n")
        // An AMD APU reporting its 512 MiB BIOS carve-out as VRAM.
        .file("/sys/class/drm/card2/device/vendor", "0x1002\n")
        .file("/sys/class/drm/card2/device/unique_id", "abc\n")
        .file(
            "/sys/class/drm/card2/device/mem_info_vram_total",
            "536870912\n",
        );
    assert!(collect(&host, &[]).is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn orders_cards_numerically_and_reuses_known_names() {
    let host = rx9070(
        rx9070(
            FakeHost::default().dir(
                "/sys/class/drm",
                &["/sys/class/drm/card10", "/sys/class/drm/card2"],
            ),
            "card10",
            "second\n",
        ),
        "card2",
        "5b7c23d1d3025b6c\n",
    );
    let cards = collect(&host, &[]);
    assert_eq!(cards.len(), 2);
    assert_eq!(cards[0].uuid, "AMD-5b7c23d1d3025b6c");
    assert_eq!(cards[1].uuid, "AMD-second");
    assert_eq!(cards[1].index, 1);

    // A known card keeps its name without another pci.ids lookup.
    let mut previous = cards.clone();
    previous[0].name = "Remembered".into();
    let again = collect(&host, &previous);
    assert_eq!(again[0].name, "Remembered");
}

#[cfg(target_os = "linux")]
#[test]
fn falls_back_for_identity_name_and_temperature() {
    let dev = "/sys/class/drm/card0/device";
    let hwmon = "/sys/class/drm/card0/device/hwmon/hwmon0";
    let host = FakeHost::default()
        .dir("/sys/class/drm", &["/sys/class/drm/card0"])
        .file(&format!("{dev}/vendor"), "0x1002\n")
        .file(&format!("{dev}/device"), "0x74A1\n")
        // No unique_id: the PCI slot identifies the card.
        .file(
            &format!("{dev}/uevent"),
            "DRIVER=amdgpu\nPCI_SLOT_NAME=0000:03:00.0\n",
        )
        .file(&format!("{dev}/mem_info_vram_total"), "205822672896\n")
        // Used above total is a bad read, not a fact.
        .file(&format!("{dev}/mem_info_vram_used"), "305822672896\n")
        .file(&format!("{dev}/gpu_busy_percent"), "250\n")
        .dir(&format!("{dev}/hwmon"), &[hwmon])
        .file(&format!("{hwmon}/temp1_input"), "61000\n");
    let card = &collect(&host, &[])[0];
    assert_eq!(card.uuid, "AMD-0000:03:00.0");
    // No pci.ids on this host.
    assert_eq!(card.name, "AMD GPU 1002:74a1");
    assert_eq!(card.used, None);
    assert_eq!(card.utilization, None);
    assert_eq!(card.temperature, Some(61));

    let named = host
        .clone()
        .file(&format!("{dev}/product_name"), "AMD Instinct MI300X\n");
    assert_eq!(collect(&named, &[])[0].name, "AMD Instinct MI300X");
}

#[cfg(target_os = "linux")]
#[test]
fn a_card_without_any_identity_is_left_out() {
    let host = FakeHost::default()
        .dir("/sys/class/drm", &["/sys/class/drm/card0"])
        .file("/sys/class/drm/card0/device/vendor", "0x1002\n")
        .file("/sys/class/drm/card0/device/unique_id", "bad id\n");
    assert!(collect(&host, &[]).is_empty());
}

#[cfg(not(target_os = "linux"))]
#[test]
fn amd_collection_is_linux_only() {
    let host = rx9070(
        FakeHost::default().dir("/sys/class/drm", &["/sys/class/drm/card1"]),
        "card1",
        "5b7c23d1d3025b6c\n",
    );
    assert!(collect(&host, &[]).is_empty());
}
