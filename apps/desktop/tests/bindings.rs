//! The TypeScript types of what the app's UI and the web viewer get are
//! generated from the Rust types (ts-rs) and checked in. This test fails
//! when they are out of date; `UPDATE_BINDINGS=1 cargo test --test
//! bindings` writes them anew.

use std::path::{Path, PathBuf};

use fernsicht_client::Failure;
use fernsicht_desktop::backend::{Device, SessionState, StreamSettings, ThisMachine};
use fernsicht_host_agent::control::PairingOpened;
use ts_rs::{Config, ExportError, TS};

/// What the app's commands answer and take.
fn app_types(cfg: &Config) -> Result<(), ExportError> {
    Device::export_all(cfg)?;
    SessionState::export_all(cfg)?;
    ThisMachine::export_all(cfg)?;
    StreamSettings::export_all(cfg)?;
    PairingOpened::export_all(cfg)?;
    Failure::export_all(cfg)
}

type Export = fn(&Config) -> Result<(), ExportError>;

const SETS: [(&str, Export); 2] = [
    ("apps/client-ui/src/lib/bindings", app_types),
    (
        "web/viewer/src/lib/bindings",
        fernsicht_host_agent::export_viewer_types,
    ),
];

/// The files in `dir`, by name.
fn files(dir: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (name, std::fs::read_to_string(e.path()).unwrap_or_default())
        })
        .collect();
    out.sort();
    out
}

#[test]
fn typescript_types_are_up_to_date() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let update = std::env::var_os("UPDATE_BINDINGS").is_some();
    for (i, (dir, export)) in SETS.into_iter().enumerate() {
        let fresh =
            std::env::temp_dir().join(format!("fernsicht-bindings-{}-{i}", std::process::id()));
        let _ = std::fs::remove_dir_all(&fresh);
        // JSON numbers, not BigInt: no value here comes near 2^53.
        export(&Config::new().with_large_int("number").with_out_dir(&fresh)).unwrap();
        let target = repo.join(dir);
        if update {
            let _ = std::fs::remove_dir_all(&target);
            std::fs::create_dir_all(&target).unwrap();
            for (name, text) in files(&fresh) {
                std::fs::write(target.join(name), text).unwrap();
            }
        } else {
            assert!(
                files(&fresh) == files(&target),
                "{dir} is out of date: UPDATE_BINDINGS=1 cargo test --test bindings \
                 (in apps/desktop)"
            );
        }
        let _ = std::fs::remove_dir_all(&fresh);
    }
}
