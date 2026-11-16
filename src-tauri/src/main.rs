// Tauri entry: window close-to-tray wiring + tray menu (Show / Pause / Quit).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod contributor;
mod settings;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, WindowEvent};

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();

            // App data dir is the work dir: credentials.json, settings.json,
            // staging/, manifests/ all land there (credentials at 0600).
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let settings = settings::load_or_create(&data_dir)?;
            app.manage(contributor::AppState::new(data_dir, settings));

            // Tray menu: Show / Pause (toggles to Resume at runtime) / Quit.
            let show = MenuItem::with_id(&handle, "show", "Show", true, None::<&str>)?;
            let pause = MenuItem::with_id(&handle, "pause", "Resume", true, None::<&str>)?;
            let quit = MenuItem::with_id(&handle, "quit", "Quit", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(&handle)?;
            let menu = Menu::with_items(&handle, &[&show, &pause, &sep, &quit])?;

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
                    "pause" => contributor::toggle_pause(app_handle),
                    "quit" => contributor::graceful_shutdown_and_exit(app_handle),
                    _ => {}
                })
                .build(&handle)?;

            contributor::set_pause_label(&handle, false);
            contributor::start_background_tasks(&handle);

            // First coordinator health probe off the startup path.
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                contributor::poll_health(&h).await;
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
            contributor::check_coordinator,
            contributor::get_settings_view,
            contributor::save_settings,
            contributor::quit_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
