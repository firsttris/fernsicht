//! "Diesen Rechner freigeben": the app installs the host it carries (in its
//! AppImage) as a system service, and removes it again.
//!
//! The host has to run as root and from boot on, so it cannot run out of
//! the AppImage, which is mounted for one user only. Installing unpacks the
//! AppImage to `/opt/fernsicht/app` and sets up `fernsicht-host.service`
//! (the unit from `packaging/`, pointed at the unpacked host and web
//! viewer); the service then runs on its own. Both steps need root: they
//! run through `pkexec`, which asks for the admin password.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use serde::Serialize;

pub const SERVICE: &str = "fernsicht-host.service";
pub const UNIT_PATH: &str = "/etc/systemd/system/fernsicht-host.service";
pub const INSTALL_DIR: &str = "/opt/fernsicht";

/// The unit `packaging/install-host.sh` installs, the one source of it.
const UNIT: &str = include_str!("../../../packaging/fernsicht-host.service");

/// The host service on this computer, as the app shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostService {
    /// The service is set up.
    pub installed: bool,
    /// It runs.
    pub active: bool,
    /// Version of the installed host.
    pub version: Option<String>,
    /// Version of the host this app carries; `None` outside an AppImage.
    pub bundled: Option<String>,
    /// This app can install (it runs from an AppImage).
    pub can_install: bool,
}

/// The unit for a host unpacked to `INSTALL_DIR/app`.
pub fn unit() -> String {
    let host = format!("{INSTALL_DIR}/app/usr/bin/fernsicht-host-agent");
    let viewer = format!("{INSTALL_DIR}/app/usr/lib/Fernsicht/viewer");
    UNIT.lines()
        .map(|l| {
            if l.starts_with("ExecStart=") {
                format!("ExecStart={host} --capture kms --encoder auto --input --web-root {viewer}")
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Run as root: `$1` the AppImage, `$2` the unit's text.
pub const INSTALL_SCRIPT: &str = r#"set -eu
appimage=$1
unit=$2
dir=/opt/fernsicht
new=$(mktemp -d /opt/.fernsicht-new.XXXXXX)
cp "$appimage" "$new/Fernsicht.AppImage"
chmod 755 "$new/Fernsicht.AppImage"
(cd "$new" && ./Fernsicht.AppImage --appimage-extract >/dev/null)
mv "$new/squashfs-root" "$new/app"
rm -f "$new/Fernsicht.AppImage"
chmod 755 "$new"
chmod -R a+rX "$new/app"
systemctl stop fernsicht-host.service 2>/dev/null || true
rm -rf "$dir"
mv "$new" "$dir"
if command -v restorecon >/dev/null 2>&1; then restorecon -R "$dir" || true; fi
printf '%s' "$unit" >/etc/systemd/system/fernsicht-host.service
chmod 644 /etc/systemd/system/fernsicht-host.service
systemctl daemon-reload
systemctl enable fernsicht-host.service
systemctl restart fernsicht-host.service
if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
    changed=0
    for proto in udp tcp; do
        if ! firewall-cmd --query-port="47800/$proto" >/dev/null 2>&1; then
            firewall-cmd --permanent --add-port="47800/$proto" >/dev/null
            changed=1
        fi
    done
    if [ "$changed" = 1 ]; then firewall-cmd --reload >/dev/null; fi
fi
"#;

/// Run as root: the service and the host go; keys and pairings stay in
/// /var/lib/fernsicht.
pub const UNINSTALL_SCRIPT: &str = r#"set -eu
systemctl disable --now fernsicht-host.service 2>/dev/null || true
rm -f /etc/systemd/system/fernsicht-host.service
rm -rf /opt/fernsicht /usr/local/bin/fernsicht-host-agent /usr/local/share/fernsicht
systemctl daemon-reload
"#;

/// "fernsicht-host-agent 0.1.0" → "0.1.0".
pub fn parse_version(out: &str) -> Option<String> {
    let v = out.split_whitespace().nth(1)?;
    v.chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit())
        .then(|| v.to_owned())
}

/// The host program a unit starts (its `ExecStart`).
pub fn exec_start(unit: &str) -> Option<PathBuf> {
    unit.lines()
        .find_map(|l| l.strip_prefix("ExecStart="))
        .and_then(|cmd| cmd.split_whitespace().next())
        .map(PathBuf::from)
}

fn version_of(program: &Path) -> Option<String> {
    let out = Command::new(program).arg("--version").output().ok()?;
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

/// The AppImage this app runs from, and the host next to the app in it.
fn bundle() -> Option<(PathBuf, PathBuf)> {
    let appimage = PathBuf::from(std::env::var_os("APPIMAGE")?);
    let host = std::env::current_exe()
        .ok()?
        .with_file_name("fernsicht-host-agent");
    (appimage.is_file() && host.is_file()).then_some((appimage, host))
}

/// What is installed here, and what this app could install.
pub fn status() -> HostService {
    let unit = std::fs::read_to_string(UNIT_PATH).ok();
    let active = Command::new("systemctl")
        .args(["is-active", "--quiet", SERVICE])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let bundle = bundle();
    HostService {
        installed: unit.is_some(),
        active,
        version: unit
            .as_deref()
            .and_then(exec_start)
            .and_then(|p| version_of(&p)),
        bundled: bundle.as_ref().and_then(|(_, host)| version_of(host)),
        can_install: bundle.is_some(),
    }
}

/// Runs `script` as root through pkexec (the desktop asks for the admin
/// password).
fn as_root(script: &str, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("pkexec")
        .args(["/bin/sh", "-c", script, "fernsicht"])
        .args(args)
        .output()
        .context("pkexec is missing")?;
    match out.status.code() {
        Some(0) => Ok(()),
        // Dismissed or not authorized.
        Some(126 | 127) => anyhow::bail!("cancelled"),
        _ => anyhow::bail!(
            "{}",
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .unwrap_or("failed")
        ),
    }
}

/// Installs (or updates) the host this AppImage carries as the service.
pub fn install() -> anyhow::Result<()> {
    let (appimage, _) = bundle().context("not running from the AppImage")?;
    let path = appimage.to_str().context("odd AppImage path")?;
    as_root(INSTALL_SCRIPT, &[path, &unit()])
}

/// Removes the service and the installed host.
pub fn uninstall() -> anyhow::Result<()> {
    as_root(UNINSTALL_SCRIPT, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_starts_the_unpacked_host_with_its_viewer() {
        let u = unit();
        let exec = u.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
        assert_eq!(
            exec,
            "ExecStart=/opt/fernsicht/app/usr/bin/fernsicht-host-agent --capture kms \
             --encoder auto --input --web-root /opt/fernsicht/app/usr/lib/Fernsicht/viewer"
        );
        // Everything else as in packaging/fernsicht-host.service.
        assert!(u.contains("StateDirectory=fernsicht"));
        assert!(u.contains("WantedBy=multi-user.target"));
        assert_eq!(u.matches("ExecStart=").count(), 1);
        assert_eq!(
            exec_start(&u).unwrap(),
            Path::new("/opt/fernsicht/app/usr/bin/fernsicht-host-agent")
        );
        assert_eq!(exec_start("[Unit]\n"), None);
    }

    #[test]
    fn versions_are_read() {
        assert_eq!(
            parse_version("fernsicht-host-agent 0.3.1\n").as_deref(),
            Some("0.3.1")
        );
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("error: something"), None);
    }

    #[test]
    fn the_scripts_are_valid_shell() {
        for script in [INSTALL_SCRIPT, UNINSTALL_SCRIPT] {
            let ok = Command::new("sh")
                .args(["-n", "-c", script])
                .status()
                .unwrap()
                .success();
            assert!(ok, "{script}");
        }
    }

    #[test]
    fn outside_an_appimage_nothing_can_be_installed() {
        // Tests do not run from an AppImage.
        let s = status();
        assert!(!s.can_install);
        assert_eq!(s.bundled, None);
        assert!(install().is_err());
    }
}
