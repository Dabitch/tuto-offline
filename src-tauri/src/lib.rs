mod db;
mod import;
mod ocr;
mod pdf;
mod query;
mod tools;

use std::sync::Mutex;

use import::AppState;
use tauri::{Manager, State};

#[tauri::command]
fn app_info(state: State<AppState>) -> import::AppInfo {
    import::app_info(&state)
}

#[tauri::command]
fn list_documents(state: State<AppState>) -> Result<Vec<db::DocumentSummary>, String> {
    let conn = state.db.lock().map_err(|error| error.to_string())?;
    db::list_documents(&conn)
}

#[tauri::command]
fn search_papers(state: State<AppState>, query: String) -> Result<Vec<db::SearchHit>, String> {
    let conn = state.db.lock().map_err(|error| error.to_string())?;
    db::search(&conn, &query)
}

#[tauri::command]
fn get_document(state: State<AppState>, id: i64) -> Result<db::DocumentDetail, String> {
    let conn = state.db.lock().map_err(|error| error.to_string())?;
    db::get_document(&conn, id)?.ok_or_else(|| "That paper is no longer in Tuto.".to_string())
}

#[tauri::command]
fn import_paths(
    app: tauri::AppHandle,
    state: State<AppState>,
    paths: Vec<String>,
) -> Result<import::ImportReport, String> {
    import::ingest_paths(&app, &state, paths)
}

#[tauri::command]
fn import_samples(
    app: tauri::AppHandle,
    state: State<AppState>,
) -> Result<import::ImportReport, String> {
    import::ingest_samples(&app, &state)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let dir = import::data_dir(app.handle()).map_err(std::io::Error::other)?;
            let conn = db::open(&dir.join("tuto.db")).map_err(std::io::Error::other)?;
            app.manage(AppState {
                db: Mutex::new(conn),
                data_dir: dir,
                tools: tools::Tools::detect(),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            list_documents,
            search_papers,
            get_document,
            import_paths,
            import_samples,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tuto");
}
