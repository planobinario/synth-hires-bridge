use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};

use daemon_core::Result;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder,
};

use super::DaemonState;

pub struct TrayHandle {
    _tray: Option<tray_icon::TrayIcon>,
}

pub fn build_tray(
    _state: Arc<RwLock<DaemonState>>,
    _config_dir: std::path::PathBuf,
    _port: u16,
    ui_ctx: Arc<RwLock<Option<eframe::egui::Context>>>,
    ui_cmd_tx: tokio::sync::mpsc::Sender<crate::UiCmd>,
) -> Result<(TrayHandle, tokio::sync::oneshot::Receiver<()>)> {
    let (quit_tx, quit_rx) = oneshot::channel();
    let icon = load_icon();

    let menu = Menu::new();

    let status: &'static MenuItem = Box::leak(Box::new(MenuItem::new(
        "🟢 SynthHires Bridge Daemon — Activo",
        false,
        None,
    )));
    menu.append(status).ok();

    let sep: &'static PredefinedMenuItem = Box::leak(Box::new(PredefinedMenuItem::separator()));
    menu.append(sep).ok();

    let show_window_item: &'static MenuItem =
        Box::leak(Box::new(MenuItem::new("🖼 Mostrar Ventana", true, None)));
    let show_window_id = show_window_item.id();
    menu.append(show_window_item).ok();

    let disconnect_item: &'static MenuItem = Box::leak(Box::new(MenuItem::new(
        "🔌 Desvincular Dispositivo",
        true,
        None,
    )));
    let disconnect_id = disconnect_item.id();
    menu.append(disconnect_item).ok();

    let sep2: &'static PredefinedMenuItem = Box::leak(Box::new(PredefinedMenuItem::separator()));
    menu.append(sep2).ok();

    let quit_item: &'static MenuItem =
        Box::leak(Box::new(MenuItem::new("🚪 Salir del Bridge", true, None)));
    let quit_id = quit_item.id();
    menu.append(quit_item).ok();

    // NOTE (Linux): the appindicator backend requires the tray to be built
    // AND pumped on the MAIN thread, but build_tray is currently called from
    // the tokio background task. tray-icon's internal GTK usage panics there
    // and the Err branch below degrades gracefully (no tray icon on Linux).
    // PROPER FIX (future): build the tray on the main thread (e.g. inside
    // eframe's creation hook) and pump GTK events from BridgeApp::update().

    let tray_res = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .with_tooltip("SynthHires Bridge — 🟢 Activo (127.0.0.1)")
        .build();

    let tray = match tray_res {
        Ok(t) => {
            std::thread::spawn(move || {
                // MenuEvent runs on its own OS thread, not inside Tokio. A dedicated
                // runtime makes the async UI command channel reliable from this
                // thread and preserves the existing build_tray API.
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to create tray action runtime");

                while let Ok(event) = MenuEvent::receiver().recv() {
                    if event.id == quit_id {
                        let _ = quit_tx.send(());
                        std::process::exit(0);
                    }

                    if event.id == show_window_id {
                        runtime.block_on(async {
                            let ctx = ui_ctx.read().await;
                            if let Some(ctx) = ctx.as_ref() {
                                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Visible(true));
                                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Focus);
                                // Also ensure it's not minimized
                                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::InnerSize(
                                    eframe::egui::vec2(700.0, 500.0),
                                ));
                                tracing::info!("Visible and Focus commands sent to UI");
                            }
                        });
                    }

                    if event.id == disconnect_id {
                        runtime.block_on(async {
                            let _ = ui_cmd_tx.send(crate::UiCmd::Unpair).await;
                        });
                    }
                }
            });
            Some(t)
        }
        Err(e) => {
            tracing::warn!("Failed to initialize tray icon ({e}), continuing in headless mode");
            None
        }
    };

    let handle = TrayHandle { _tray: tray };
    Ok((handle, quit_rx))
}

pub fn show_background_notice() {
    use notify_rust::Notification;
    let _ = Notification::new()
        .summary("SynthHires Bridge")
        .body("Sigue activo en segundo plano. Haz clic en el icono de la bandeja para verlo o cerrarlo.")
        .icon("dialog-information")
        .show();
}

const TRAY_ICON_PNG: &[u8] = include_bytes!("../../../assets/icon.png");

fn load_icon() -> Icon {
    let img = image::load_from_memory(TRAY_ICON_PNG)
        .expect("failed to decode embedded tray icon")
        .into_rgba8();
    let (w, h) = img.dimensions();
    Icon::from_rgba(img.into_raw(), w, h).expect("failed to create tray icon from RGBA")
}
