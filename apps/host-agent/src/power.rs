//! Keeps the GPU clocked up while a session runs.
//!
//! Between frames a GPU drops to low clocks, and the next frame's encode
//! then starts slow: on the RX 7800 XT encoding 1440p took 10 ms instead of
//! 3–4. During a session the host raises the clock level and puts it back
//! afterwards, so the card only uses the extra power while someone watches.
//!
//! - AMD (amdgpu): `power_dpm_force_performance_level` = `high`.
//! - Intel (i915): the minimum clock raised to the boost clock.
//! - NVIDIA: left alone (no sysfs knob; it clocks down less while encoding).
//!
//! Writing these needs root (the service). The old values are also written
//! to a file under `/run/fernsicht`, so a host that crashed mid-session
//! puts them back when it starts again.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One sysfs value changed, and what it was.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub path: PathBuf,
    pub old: String,
}

/// The sysfs values to change for the GPU behind `render_node`, under
/// `sys` (`/sys`, or a stand-in in tests): (file, value) pairs.
pub fn plan(sys: &Path, render_node: &str) -> Vec<(PathBuf, String)> {
    let Some(name) = Path::new(render_node).file_name() else {
        return Vec::new();
    };
    let device = sys.join("class/drm").join(name).join("device");
    let Ok(device_real) = device.canonicalize() else {
        return Vec::new();
    };
    let vendor = std::fs::read_to_string(device.join("vendor")).unwrap_or_default();
    match vendor.trim() {
        "0x1002" => {
            let level = device.join("power_dpm_force_performance_level");
            if level.exists() {
                vec![(level, "high".into())]
            } else {
                Vec::new()
            }
        }
        "0x8086" => {
            // i915 keeps its clock limits on the card node, not the device.
            let Some(card) = cards_of(sys, &device_real) else {
                return Vec::new();
            };
            let boost = std::fs::read_to_string(card.join("gt_boost_freq_mhz"))
                .or_else(|_| std::fs::read_to_string(card.join("gt_RP0_freq_mhz")));
            match boost {
                Ok(mhz) if card.join("gt_min_freq_mhz").exists() => {
                    vec![(card.join("gt_min_freq_mhz"), mhz.trim().to_owned())]
                }
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// The `cardN` node of a GPU device.
fn cards_of(sys: &Path, device: &Path) -> Option<PathBuf> {
    std::fs::read_dir(sys.join("class/drm"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("card") && !n.contains('-'))
        })
        .find(|p| p.join("device").canonicalize().ok().as_deref() == Some(device))
}

/// Raised clocks for the life of the value; dropped, the old ones return.
#[derive(Debug)]
pub struct GpuBoost {
    saved: Vec<Saved>,
    record: Option<PathBuf>,
}

impl GpuBoost {
    /// Applies `plan`, noting the old values in `record` (a file) first.
    /// Values that cannot be written are skipped (not root, no such knob).
    pub fn apply(plan: Vec<(PathBuf, String)>, record: Option<PathBuf>) -> Self {
        let mut saved = Vec::new();
        for (path, value) in plan {
            let Ok(old) = std::fs::read_to_string(&path) else {
                continue;
            };
            let old = old.trim().to_owned();
            if old == value {
                continue;
            }
            // Noted before writing: a crash right after the write still
            // finds the old value.
            saved.push(Saved {
                path: path.clone(),
                old,
            });
            write_record(record.as_deref(), &saved);
            match std::fs::write(&path, &value) {
                Ok(()) => log::info!("GPU: {} = {value} for the session", path.display()),
                Err(e) => {
                    log::info!("GPU: cannot raise {} ({e})", path.display());
                    saved.pop();
                    write_record(record.as_deref(), &saved);
                }
            }
        }
        Self { saved, record }
    }

    /// Whether anything was changed.
    pub fn active(&self) -> bool {
        !self.saved.is_empty()
    }
}

impl Drop for GpuBoost {
    fn drop(&mut self) {
        restore(&self.saved);
        if let Some(r) = &self.record {
            let _ = std::fs::remove_file(r);
        }
    }
}

fn restore(saved: &[Saved]) {
    for s in saved.iter().rev() {
        match std::fs::write(&s.path, &s.old) {
            Ok(()) => log::info!("GPU: {} = {} again", s.path.display(), s.old),
            Err(e) => log::warn!("GPU: restoring {}: {e}", s.path.display()),
        }
    }
}

fn write_record(record: Option<&Path>, saved: &[Saved]) {
    if let Some(r) = record
        && let Ok(json) = serde_json::to_string(saved)
    {
        if let Some(dir) = r.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(r, json);
    }
}

/// Puts back what a host that ended without restoring left in `record`.
pub fn recover(record: &Path) {
    let Ok(text) = std::fs::read_to_string(record) else {
        return;
    };
    if let Ok(saved) = serde_json::from_str::<Vec<Saved>>(&text) {
        log::info!("GPU: restoring clocks from an earlier run");
        restore(&saved);
    }
    let _ = std::fs::remove_file(record);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in /sys with one GPU of `vendor`.
    fn fake_sys(vendor: &str) -> PathBuf {
        // One stand-in per call: tests run in parallel.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sys =
            std::env::temp_dir().join(format!("fernsicht-sys-{}-{vendor}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sys);
        let dev = sys.join("devices/pci/0000:03:00.0");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("vendor"), format!("{vendor}\n")).unwrap();
        std::fs::write(dev.join("power_dpm_force_performance_level"), "auto\n").unwrap();
        let drm = sys.join("class/drm");
        for node in ["renderD128", "card1"] {
            std::fs::create_dir_all(drm.join(node)).unwrap();
            std::os::unix::fs::symlink(&dev, drm.join(node).join("device")).unwrap();
        }
        // A connector, which is no card.
        std::fs::create_dir_all(drm.join("card1-DP-1")).unwrap();
        std::fs::write(drm.join("card1/gt_min_freq_mhz"), "300\n").unwrap();
        std::fs::write(drm.join("card1/gt_boost_freq_mhz"), "1450\n").unwrap();
        sys
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap().trim().to_owned()
    }

    #[test]
    fn amd_runs_high_during_the_session_and_auto_after() {
        let sys = fake_sys("0x1002");
        let level = sys.join("devices/pci/0000:03:00.0/power_dpm_force_performance_level");
        let record = sys.join("run/gpu-power.json");
        let p = plan(&sys, "/dev/dri/renderD128");
        assert_eq!(p.len(), 1);
        let boost = GpuBoost::apply(p, Some(record.clone()));
        assert!(boost.active());
        assert_eq!(read(&level), "high");
        assert!(record.exists(), "old value noted for a crash");
        drop(boost);
        assert_eq!(read(&level), "auto");
        assert!(!record.exists());
        let _ = std::fs::remove_dir_all(&sys);
    }

    #[test]
    fn intel_raises_its_minimum_clock() {
        let sys = fake_sys("0x8086");
        let min = sys.join("class/drm/card1/gt_min_freq_mhz");
        let boost = GpuBoost::apply(plan(&sys, "/dev/dri/renderD128"), None);
        assert_eq!(read(&min), "1450");
        drop(boost);
        assert_eq!(read(&min), "300");
        let _ = std::fs::remove_dir_all(&sys);
    }

    #[test]
    fn other_gpus_and_odd_nodes_are_left_alone() {
        let sys = fake_sys("0x10de");
        assert!(plan(&sys, "/dev/dri/renderD128").is_empty());
        assert!(plan(&sys, "/dev/dri/renderD129").is_empty());
        assert!(plan(&sys, "/").is_empty());
        // Already high: nothing to change, nothing to restore.
        let sys2 = fake_sys("0x1002");
        let level = sys2.join("devices/pci/0000:03:00.0/power_dpm_force_performance_level");
        std::fs::write(&level, "high\n").unwrap();
        let b = GpuBoost::apply(plan(&sys2, "/dev/dri/renderD128"), None);
        assert!(!b.active());
        // A knob that cannot be written is skipped.
        let b = GpuBoost::apply(vec![(sys2.join("missing/file"), "high".into())], None);
        assert!(!b.active());
        let _ = std::fs::remove_dir_all(&sys);
        let _ = std::fs::remove_dir_all(&sys2);
    }

    #[test]
    fn a_crashed_host_puts_the_clocks_back_on_start() {
        let sys = fake_sys("0x1002");
        let level = sys.join("devices/pci/0000:03:00.0/power_dpm_force_performance_level");
        let record = sys.join("run/gpu-power.json");
        let boost = GpuBoost::apply(plan(&sys, "/dev/dri/renderD128"), Some(record.clone()));
        // The host dies without dropping it.
        std::mem::forget(boost);
        assert_eq!(read(&level), "high");
        recover(&record);
        assert_eq!(read(&level), "auto");
        assert!(!record.exists());
        recover(&record); // nothing there: fine
        let _ = std::fs::remove_dir_all(&sys);
    }
}
