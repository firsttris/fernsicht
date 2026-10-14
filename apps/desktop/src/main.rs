//! The Fernsicht app: the client UI (web/client-ui) in a window, with
//! the commands it calls. Sessions run in the native client's own window
//! (Vulkan, no webview in the picture's path); the app starts and ends
//! them and shows their overlay.

use std::sync::Arc;

use fernsicht_client::{Failure, FailureCode, failure_of};
use fernsicht_desktop::backend::{
    Backend, Device, SessionState, StreamSettings, ThisMachine, default_dir, keys_command,
};
use fernsicht_host_agent::control::PairingOpened;
use tauri::State;

type Shared<'a> = State<'a, Arc<Backend>>;

/// What a command answers: its value, or a failure with a code the UI
/// tells apart.
type Answer<T> = Result<T, Failure>;

/// Runs blocking work (network, processes) off the UI thread.
async fn blocking<T: Send + 'static>(
    backend: &Shared<'_>,
    f: impl FnOnce(&Backend) -> anyhow::Result<T> + Send + 'static,
) -> Answer<T> {
    let backend = Arc::clone(backend);
    tauri::async_runtime::spawn_blocking(move || f(&backend))
        .await
        .map_err(|e| Failure::new(FailureCode::Other, e.to_string()))?
        .map_err(|e| failure_of(&e))
}

#[tauri::command]
async fn devices(backend: Shared<'_>) -> Answer<Vec<Device>> {
    blocking(&backend, Backend::devices).await
}

#[tauri::command]
async fn pair(backend: Shared<'_>, address: String, pin: String) -> Answer<Device> {
    blocking(&backend, move |b| b.pair(&address, &pin)).await
}

#[tauri::command]
async fn forget(backend: Shared<'_>, id: String) -> Answer<()> {
    blocking(&backend, move |b| b.forget(&id)).await
}

#[tauri::command]
async fn connect(
    backend: Shared<'_>,
    id: String,
    settings: Option<StreamSettings>,
) -> Answer<SessionState> {
    blocking(&backend, move |b| {
        b.connect(&id, &settings.unwrap_or_default())
    })
    .await
}

#[tauri::command]
async fn set_muted(backend: Shared<'_>, muted: bool) -> Answer<()> {
    blocking(&backend, move |b| {
        b.command(if muted { "mute" } else { "unmute" })
    })
    .await
}

#[tauri::command]
async fn send_keys(backend: Shared<'_>, codes: Vec<u16>) -> Answer<()> {
    blocking(&backend, move |b| b.command(&keys_command(&codes)?)).await
}

#[tauri::command]
async fn select_monitor(backend: Shared<'_>, index: u8) -> Answer<()> {
    blocking(&backend, move |b| b.command(&format!("monitor {index}"))).await
}

#[tauri::command]
async fn set_mode(backend: Shared<'_>, gaming: bool) -> Answer<()> {
    blocking(&backend, move |b| {
        b.command(if gaming { "gaming" } else { "desktop" })
    })
    .await
}

#[tauri::command]
async fn session(backend: Shared<'_>) -> Answer<SessionState> {
    blocking(&backend, |b| Ok(b.session())).await
}

#[tauri::command]
async fn disconnect(backend: Shared<'_>) -> Answer<()> {
    blocking(&backend, |b| {
        b.disconnect();
        Ok(())
    })
    .await
}

#[tauri::command]
async fn this_machine(backend: Shared<'_>) -> Answer<ThisMachine> {
    blocking(&backend, |b| Ok(b.this_machine())).await
}

#[tauri::command]
async fn open_pairing(backend: Shared<'_>) -> Answer<PairingOpened> {
    blocking(&backend, Backend::open_pairing).await
}

#[tauri::command]
async fn set_gpu_boost(backend: Shared<'_>, on: bool) -> Answer<()> {
    blocking(&backend, move |b| b.set_gpu_boost(on)).await
}

/// "Diesen Rechner freigeben": installs the host from the AppImage.
#[tauri::command]
async fn share_this_machine(backend: Shared<'_>) -> Answer<()> {
    blocking(&backend, |_| fernsicht_desktop::share::install()).await
}

/// "Freigabe beenden": removes the host service.
#[tauri::command]
async fn stop_sharing(backend: Shared<'_>) -> Answer<()> {
    blocking(&backend, |_| fernsicht_desktop::share::uninstall()).await
}

#[tauri::command]
async fn set_port_mapping(backend: Shared<'_>, on: bool) -> Answer<()> {
    blocking(&backend, move |b| b.set_port_mapping(on)).await
}

#[tauri::command]
async fn set_rendezvous(backend: Shared<'_>, url: String) -> Answer<()> {
    blocking(&backend, move |b| b.set_rendezvous(&url)).await
}

#[tauri::command]
async fn unpair_from_host(backend: Shared<'_>, device: String) -> Answer<()> {
    blocking(&backend, move |b| b.unpair_from_host(&device)).await
}

fn main() {
    // WebKitGTK's DMA-BUF renderer shows an empty window with NVIDIA's
    // driver (tauri-apps/tauri#9394); its fallback renders fine.
    if std::path::Path::new("/proc/driver/nvidia").exists()
        && std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none()
    {
        // SAFETY: first thing in main, before any other thread exists.
        unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let backend = Arc::new(Backend::new(default_dir()));
    log::info!("client program: {}", backend.client.display());
    let on_exit = Arc::clone(&backend);
    tauri::Builder::default()
        .manage(backend)
        .invoke_handler(tauri::generate_handler![
            devices,
            pair,
            forget,
            connect,
            set_muted,
            set_mode,
            send_keys,
            select_monitor,
            session,
            disconnect,
            this_machine,
            open_pairing,
            unpair_from_host,
            share_this_machine,
            stop_sharing,
            set_gpu_boost,
            set_rendezvous,
            set_port_mapping,
        ])
        .build(tauri::generate_context!())
        .expect("starting the app")
        .run(move |_, event| {
            // Closing the app ends a session it started.
            if let tauri::RunEvent::Exit = event {
                on_exit.disconnect();
            }
        });
}
