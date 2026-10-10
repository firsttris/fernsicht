//! What a host tells about itself when found in the LAN: its OS and GPU,
//! in words people know ("Bazzite", "Radeon RX 7700 XT / 7800 XT").

use std::path::Path;

/// The OS name from os-release text (`NAME=`, quotes removed).
pub fn os_name_from(os_release: &str) -> Option<String> {
    os_release.lines().find_map(|l| {
        let v = l.strip_prefix("NAME=")?.trim().trim_matches('"').trim();
        (!v.is_empty()).then(|| v.to_owned())
    })
}

/// This machine's OS name ("Linux" if unknown).
pub fn os_name() -> String {
    ["/etc/os-release", "/usr/lib/os-release"]
        .iter()
        .find_map(|p| os_name_from(&std::fs::read_to_string(p).ok()?))
        .unwrap_or_else(|| "Linux".into())
}

/// A GPU's name from a pci.ids database: the marketing name in brackets
/// if there is one ("Navi 32 [Radeon RX 7700 XT / 7800 XT]" → the part in
/// brackets), else the whole device name.
pub fn gpu_name_from(pci_ids: &str, vendor: u16, device: u16) -> Option<String> {
    let vendor = format!("{vendor:04x}");
    let device = format!("\t{device:04x}");
    let mut in_vendor = false;
    for line in pci_ids.lines() {
        if !line.starts_with('\t') && !line.starts_with('#') && !line.is_empty() {
            in_vendor = line.starts_with(&vendor);
            continue;
        }
        if in_vendor && let Some(rest) = line.strip_prefix(&device) {
            let name = rest.trim();
            let marketing = name
                .split_once('[')
                .and_then(|(_, b)| b.rsplit_once(']'))
                .map(|(m, _)| m.trim());
            return Some(marketing.unwrap_or(name).to_owned());
        }
    }
    None
}

/// The vendor's short name, for when the database does not know the GPU.
pub fn vendor_name(vendor: u16) -> &'static str {
    match vendor {
        0x1002 => "AMD",
        0x10de => "NVIDIA",
        0x8086 => "Intel",
        _ => "GPU",
    }
}

fn read_hex(path: &Path) -> Option<u16> {
    let s = std::fs::read_to_string(path).ok()?;
    u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

/// The name of the GPU behind a DRM node (`/dev/dri/renderD128`).
pub fn gpu_name(node: &str) -> Option<String> {
    let name = Path::new(node).file_name()?;
    let dev = Path::new("/sys/class/drm").join(name).join("device");
    let (vendor, device) = (
        read_hex(&dev.join("vendor"))?,
        read_hex(&dev.join("device"))?,
    );
    let ids = ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok());
    Some(
        ids.and_then(|ids| gpu_name_from(&ids, vendor, device))
            .unwrap_or_else(|| vendor_name(vendor).to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_names() {
        let fedora = "NAME=\"Fedora Linux\"\nVERSION=\"44\"\nPRETTY_NAME=\"Fedora Linux 44\"\n";
        assert_eq!(os_name_from(fedora).as_deref(), Some("Fedora Linux"));
        assert_eq!(os_name_from("NAME=Arch\n").as_deref(), Some("Arch"));
        assert_eq!(os_name_from("PRETTY_NAME=\"x\"\nNAME=\"\"\n"), None);
        assert!(!os_name().is_empty());
    }

    const IDS: &str = "\
# comment
1002  Advanced Micro Devices, Inc. [AMD/ATI]
\t7480  Navi 33 [Radeon RX 7600/7600 XT/7600M XT/7600S/7700S / PRO W7600]
\t747e  Navi 32 [Radeon RX 7700 XT / 7800 XT]
\t\t1002 0e3b  Radeon RX 7800 XT
10de  NVIDIA Corporation
\t1b80  GP104 [GeForce GTX 1080]
\t1234  Plain Name
8086  Intel Corporation
\t747e  Not the AMD one
";

    #[test]
    fn gpu_names() {
        assert_eq!(
            gpu_name_from(IDS, 0x1002, 0x747e).as_deref(),
            Some("Radeon RX 7700 XT / 7800 XT")
        );
        assert_eq!(
            gpu_name_from(IDS, 0x10de, 0x1b80).as_deref(),
            Some("GeForce GTX 1080")
        );
        assert_eq!(
            gpu_name_from(IDS, 0x10de, 0x1234).as_deref(),
            Some("Plain Name")
        );
        assert_eq!(
            gpu_name_from(IDS, 0x8086, 0x747e).as_deref(),
            Some("Not the AMD one")
        );
        assert_eq!(gpu_name_from(IDS, 0x1002, 0x1b80), None);
        assert_eq!(gpu_name_from(IDS, 0xabcd, 0x747e), None);
        assert_eq!(vendor_name(0x10de), "NVIDIA");
        assert_eq!(vendor_name(1), "GPU");
        assert_eq!(gpu_name("/dev/dri/nonexistent"), None);
        // Whatever this machine has: a name, never a panic.
        if Path::new("/sys/class/drm/renderD128").exists() {
            assert!(!gpu_name("/dev/dri/renderD128").unwrap().is_empty());
        }
    }
}
