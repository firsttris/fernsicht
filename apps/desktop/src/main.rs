//! The Fernsicht app: the client UI (apps/client-ui) in a window, with
//! the commands it calls. Sessions run in the native client's own window
//! (Vulkan, no webview in the picture's path); the app starts and ends
//! them and shows their overlay.

use std::sync::Arc;

use fernsicht_desktop::backend::{
    Backend, Device, SessionState, StreamSettings, ThisMachine, default_dir, keys_command,
};
use serde_json::Value;
use tauri::State;

type Shared<'a> = State<'a, Arc<Backend>>;

/// Runs blocking work (network, processes) off the UI thread; errors
/// become the message the UI shows.
async fn blocking<T: Send + 'static>(
    backend: &Shared<'_>,
    f: impl FnOnce(&Backend) -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    let backend = Arc::clone(backend);
    tauri::async_runtime::spawn_blocking(move || f(&backend))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn devices(backend: Shared<'_>) -> Result<Vec<Device>, String> {
    blocking(&backend, Backend::devices).await
}

#[tauri::command]
async fn pair(backend: Shared<'_>, address: String, pin: String) -> Result<Device, String> {
    blocking(&backend, move |b| b.pair(&address, &pin)).await
}

#[tauri::command]
async fn forget(backend: Shared<'_>, id: String) -> Result<(), String> {
    blocking(&backend, move |b| b.forget(&id)).await
}

#[tauri::command]
async fn connect(
    backend: Shared<'_>,
    id: String,
    settings: Option<StreamSettings>,
) -> Result<SessionState, String> {
    blocking(&backend, move |b| {
        b.connect(&id, &settings.unwrap_or_default())
    })
    .await
}

#[tauri::command]
async fn set_muted(backend: Shared<'_>, muted: bool) -> Result<(), String> {
    blocking(&backend, move |b| {
        b.command(if muted { "mute" } else { "unmute" })
    })
    .await
}

#[tauri::command]
async fn send_keys(backend: Shared<'_>, codes: Vec<u16>) -> Result<(), String> {
    blocking(&backend, move |b| b.command(&keys_command(&codes)?)).await
}

#[tauri::command]
async fn set_mode(backend: Shared<'_>, gaming: bool) -> Result<(), String> {
    blocking(&backend, move |b| {
        b.command(if gaming { "gaming" } else { "desktop" })
    })
    .await
}

#[tauri::command]
fn session(backend: Shared<'_>) -> SessionState {
    backend.session()
}

#[tauri::command]
async fn disconnect(backend: Shared<'_>) -> Result<(), String> {
    blocking(&backend, |b| {
        b.disconnect();
        Ok(())
    })
    .await
}

#[tauri::command]
async fn this_machine(backend: Shared<'_>) -> Result<ThisMachine, String> {
    blocking(&backend, |b| Ok(b.this_machine())).await
}

#[tauri::command]
async fn open_pairing(backend: Shared<'_>) -> Result<Value, String> {
    blocking(&backend, Backend::open_pairing).await
}

#[tauri::command]
async fn set_gpu_boost(backend: Shared<'_>, on: bool) -> Result<(), String> {
    blocking(&backend, move |b| b.set_gpu_boost(on)).await
}

/// "Diesen Rechner freigeben": installs the host from the AppImage.
#[tauri::command]
async fn share_this_machine(backend: Shared<'_>) -> Result<(), String> {
    blocking(&backend, |_| fernsicht_desktop::share::install()).await
}

/// "Freigabe beenden": removes the host service.
#[tauri::command]
async fn stop_sharing(backend: Shared<'_>) -> Result<(), String> {
    blocking(&backend, |_| fernsicht_desktop::share::uninstall()).await
}

#[tauri::command]
async fn unpair_from_host(backend: Shared<'_>, device: String) -> Result<(), String> {
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
            session,
            disconnect,
            this_machine,
            open_pairing,
            unpair_from_host,
            share_this_machine,
            stop_sharing,
            set_gpu_boost,
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
