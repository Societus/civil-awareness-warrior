// Tauri entry: window close-to-tray wiring + tray menu (Show / Settings… /
// Pause / Quit).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod contributor;
mod hardware;
mod settings;
mod sidecar;

use std::sync::Arc;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, WindowEvent};

fn main() {
    tauri::Builder::default()
        // "Can I AI?" advisor links open in the system browser through the
        // opener plugin; the capability scopes it to huggingface.co only
        // (capabilities/default.json). No other URL can be opened.
        .plugin(tauri_plugin_opener::init())
        // Launch-at-login (card t_f9279e25): the plugin owns the OS entry —
        // HKCU Run value (Windows), XDG autostart .desktop (Linux,
        // AppImage-aware), ~/Library/LaunchAgents LaunchAgent (macOS; the
        // README autostart section covers headless/console-login caveats).
        // No args are passed to the autostarted launch; every call is
        // Rust-side so no webview permission is added.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .setup(|app| {
            let handle = app.handle().clone();

            // App data dir is the work dir: credentials.json, settings.json,
            // staging/, manifests/ all land there (credentials at 0600).
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let settings = settings::load_or_create(&data_dir)?;

            // Launch-at-login startup reconcile: re-apply the persisted
            // intent on EVERY startup instead of trusting the OS entry as
            // found. enable() re-points the entry at the binary actually
            // running (a hand-deleted entry re-registers; a stale entry
            // left by an older path/dev build is corrected); disable() is
            // a cheap no-op when nothing is registered. Failures log and
            // never trap the app — a session that refuses the entry write
            // (headless macOS over ssh, §README) still runs fine when the
            // human starts it.
            {
                use tauri_plugin_autostart::ManagerExt;
                let manager = handle.autolaunch();
                let r = if settings.autostart {
                    manager.enable()
                } else {
                    manager.disable()
                };
                if let Err(e) = r {
                    eprintln!("autostart startup reconcile failed: {e}");
                }
            }
            // AppState behind an Arc: background tasks and the lifecycle
            // tests share it; the tray handlers reach it via managed state.
            let state = Arc::new(contributor::AppState::new(
                data_dir,
                settings,
                Some(handle.clone()),
            ));
            app.manage(state.clone());

            // Tray menu: Show / Settings… / Pause (toggles to Resume at
            // runtime) / Quit.
            let show = MenuItem::with_id(&handle, "show", "Show", true, None::<&str>)?;
            let open_settings =
                MenuItem::with_id(&handle, "settings", "Settings…", true, None::<&str>)?;
            let pause = MenuItem::with_id(&handle, "pause", "Resume", true, None::<&str>)?;
            let quit = MenuItem::with_id(&handle, "quit", "Quit", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(&handle)?;
            let menu = Menu::with_items(&handle, &[&show, &open_settings, &pause, &sep, &quit])?;

            TrayIconBuilder::with_id(contributor::TRAY_ID)
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("LandHunter Contributor")
                .icon(
                    handle
                        .default_window_icon()
                        .cloned()
                        .expect("bundle icon missing in tauri.conf.json"),
                )
                .on_menu_event(|app_handle, event| match event.id().as_ref() {
                    "show" => {
                        if let Some(w) = app_handle.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "settings" => {
                        // Settings… shows the window (it may be hidden in
                        // the tray) and tells the webview to open the
                        // settings dialog.
                        if let Some(w) = app_handle.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                        let _ = app_handle.emit("lh-settings", ());
                    }
                    "pause" => contributor::toggle_pause(app_handle),
                    "quit" => contributor::graceful_shutdown_and_exit(app_handle),
                    _ => {}
                })
                .build(&handle)?;

            contributor::start_background_tasks(&state);

            // First coordinator health probe off the startup path.
            tauri::async_runtime::spawn({
                let s = state.clone();
                async move {
                    contributor::poll_health(&s).await;
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Minimize-to-tray: the close button hides the window; the app
            // (and any active contributor loop) keeps running in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            contributor::get_status,
            contributor::start_contributor,
            contributor::pause_contributor,
            contributor::resume_contributor,
            contributor::register_client,
            contributor::unregister_identity,
            contributor::check_coordinator,
            contributor::get_settings_view,
            contributor::save_settings,
            contributor::community_stats,
            contributor::download_local_model,
            contributor::delete_local_model,
            contributor::check_local_model,
            contributor::check_binary_update,
            contributor::install_binary_update,
            contributor::remove_installed_binary,
            hardware::check_hardware,
            contributor::quit_app
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // Backstop for every shutdown route: whatever quit path fired
            // (tray Quit, window exit, ExitRequested from handle.exit), a
            // live sidecar child must never survive the app. graceful_shutdown
            // already stops it; this covers routes that skip it.
            if let tauri::RunEvent::ExitRequested { .. } = event {
                contributor::emergency_kill_sidecar(app_handle);
            }
        });
}
