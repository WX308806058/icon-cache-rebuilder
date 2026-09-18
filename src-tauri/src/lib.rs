use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

/// 隐藏子进程的控制台窗口，避免 taskkill / attrib 弹出黑框
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// 重建任务运行标记：运行期间阻止窗口关闭，防止 explorer 被杀死后未重启
struct RebuildRunning(AtomicBool);

#[derive(Clone, Serialize)]
struct StepRecord {
    step: u8,
    name: String,
    status: String,
    message: String,
}

#[derive(Serialize)]
struct RebuildResult {
    success: bool,
    total_deleted: usize,
    failed_files: Vec<String>,
    steps: Vec<StepRecord>,
}

#[derive(Serialize)]
struct CacheInfo {
    explorer_dir: String,
    file_count: u64,
    total_size: u64,
    icon_cache_db: Option<u64>,
}

fn record(
    app: &AppHandle,
    steps: &mut Vec<StepRecord>,
    step: u8,
    name: &str,
    status: &str,
    message: String,
) {
    let rec = StepRecord {
        step,
        name: name.to_string(),
        status: status.to_string(),
        message,
    };
    let _ = app.emit("rebuild-progress", &rec);
    steps.push(rec);
}

fn local_appdata() -> Result<PathBuf, String> {
    std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .map_err(|_| "无法读取 LOCALAPPDATA 环境变量".to_string())
}

/// 对应 bat 第 1 行：taskkill /f /im explorer.exe
/// 返回 taskkill 退出码：0 成功，128 进程不存在，其他为失败
fn kill_explorer() -> Result<i32, String> {
    let output = Command::new("taskkill")
        .args(["/f", "/im", "explorer.exe"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("无法启动 taskkill: {e}"))?;
    Ok(output.status.code().unwrap_or(-1))
}

/// 删除单个文件；若因只读/系统/隐藏属性失败，先清除属性再重试（对应 bat 中 del 的 /f /a 参数）
fn remove_file_force(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let _ = Command::new("attrib")
                .args(["-r", "-s", "-h"])
                .arg(path)
                .creation_flags(CREATE_NO_WINDOW)
                .status();
            fs::remove_file(path).map_err(|e| format!("{e}"))
        }
        Err(e) => Err(format!("{e}")),
    }
}

/// 递归删除目录下所有文件、保留目录结构（对应 bat 第 2 行 del 的 /f /s /q 语义）
fn clear_directory_files(dir: &Path) -> (usize, Vec<String>) {
    fn walk(dir: &Path, deleted: &mut usize, failed: &mut Vec<String>) {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                failed.push(format!("读取目录 {} 失败: {e}", dir.display()));
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, deleted, failed);
            } else {
                match remove_file_force(&path) {
                    Ok(_) => *deleted += 1,
                    Err(e) => failed.push(format!("{}: {e}", path.display())),
                }
            }
        }
    }
    let mut deleted = 0;
    let mut failed = Vec::new();
    walk(dir, &mut deleted, &mut failed);
    (deleted, failed)
}

/// 递归统计目录内文件数与总大小
fn collect_dir_stats(dir: &Path) -> (u64, u64) {
    fn walk(dir: &Path, file_count: &mut u64, total_size: &mut u64) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, file_count, total_size);
                } else if let Ok(meta) = entry.metadata() {
                    *file_count += 1;
                    *total_size += meta.len();
                }
            }
        }
    }
    let mut file_count = 0;
    let mut total_size = 0;
    walk(dir, &mut file_count, &mut total_size);
    (file_count, total_size)
}

#[tauri::command]
fn get_cache_info() -> Result<CacheInfo, String> {
    let base = local_appdata()?;
    let explorer_dir = base.join("Microsoft").join("Windows").join("Explorer");
    let (file_count, total_size) = if explorer_dir.is_dir() {
        collect_dir_stats(&explorer_dir)
    } else {
        (0, 0)
    };
    let icon_cache_db = base.join("IconCache.db").metadata().ok().map(|m| m.len());
    Ok(CacheInfo {
        explorer_dir: explorer_dir.display().to_string(),
        file_count,
        total_size,
        icon_cache_db,
    })
}

#[tauri::command]
async fn rebuild_icon_cache(app: AppHandle) -> Result<RebuildResult, String> {
    let mut steps: Vec<StepRecord> = Vec::new();
    let mut total_deleted: usize = 0;
    let mut failed_files: Vec<String> = Vec::new();

    let base = local_appdata()?;
    let explorer_dir = base.join("Microsoft").join("Windows").join("Explorer");
    let icon_cache_db = base.join("IconCache.db");
    let step1 = "结束 explorer.exe 进程";
    let step2 = "清理 Explorer 图标缓存目录";
    let step3 = "删除 IconCache.db";
    let step4 = "重启 explorer.exe";

    // 路径解析成功后才置运行标记，避免异常提前返回时窗口永久无法关闭
    let running_flag = app.state::<RebuildRunning>();
    running_flag.0.store(true, Ordering::SeqCst);

    // ---- 步骤 1：结束 explorer.exe，释放缓存文件句柄 ----
    record(&app, &mut steps, 1, step1, "running", "正在执行 taskkill /f /im explorer.exe ...".into());
    match kill_explorer() {
        Ok(0) => record(&app, &mut steps, 1, step1, "success", "已强制结束 explorer.exe".into()),
        Ok(128) => record(&app, &mut steps, 1, step1, "warning", "explorer.exe 当前未在运行，无需结束".into()),
        Ok(code) => record(
            &app,
            &mut steps,
            1,
            step1,
            "failed",
            format!("taskkill 返回错误码 {code}，进程可能仍在运行"),
        ),
        Err(e) => record(&app, &mut steps, 1, step1, "failed", e),
    }

    // 等待系统释放缓存文件句柄
    thread::sleep(Duration::from_millis(800));

    // ---- 步骤 2：清理 Explorer 图标缓存目录 ----
    record(
        &app,
        &mut steps,
        2,
        step2,
        "running",
        format!("正在删除 {} 下的缓存文件 ...", explorer_dir.display()),
    );
    if explorer_dir.is_dir() {
        let (deleted, failed) = clear_directory_files(&explorer_dir);
        total_deleted += deleted;
        failed_files.extend(failed);
        let failed_count = failed_files.len();
        let (status, msg) = if failed_count == 0 {
            ("success", format!("已删除 {deleted} 个缓存文件"))
        } else {
            (
                "failed",
                format!("已删除 {deleted} 个文件，{failed_count} 个删除失败（可能被其他进程占用）"),
            )
        };
        record(&app, &mut steps, 2, step2, status, msg);
    } else {
        record(
            &app,
            &mut steps,
            2,
            step2,
            "warning",
            format!("缓存目录不存在: {}", explorer_dir.display()),
        );
    }

    // ---- 步骤 3：删除 IconCache.db ----
    record(&app, &mut steps, 3, step3, "running", "正在删除 IconCache.db ...".into());
    if icon_cache_db.is_file() {
        match remove_file_force(&icon_cache_db) {
            Ok(_) => {
                total_deleted += 1;
                record(&app, &mut steps, 3, step3, "success", "已删除 IconCache.db".into());
            }
            Err(e) => {
                failed_files.push(format!("{}: {e}", icon_cache_db.display()));
                record(&app, &mut steps, 3, step3, "failed", format!("删除失败: {e}"));
            }
        }
    } else {
        record(&app, &mut steps, 3, step3, "warning", "IconCache.db 不存在，可能已被清理".into());
    }

    // ---- 步骤 4：重启 explorer.exe ----
    record(&app, &mut steps, 4, step4, "running", "正在启动 explorer.exe ...".into());
    match Command::new("explorer.exe").spawn() {
        Ok(_) => record(&app, &mut steps, 4, step4, "success", "explorer.exe 已重新启动".into()),
        Err(e) => record(
            &app,
            &mut steps,
            4,
            step4,
            "failed",
            format!("启动失败: {e}，请打开任务管理器手动运行 explorer.exe"),
        ),
    }

    running_flag.0.store(false, Ordering::SeqCst);

    let success = steps.iter().all(|s| s.status != "failed");
    Ok(RebuildResult {
        success,
        total_deleted,
        failed_files,
        steps,
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(RebuildRunning(AtomicBool::new(false)))
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let running = window.state::<RebuildRunning>().0.load(Ordering::SeqCst);
                if running {
                    // 重建期间禁止关闭窗口，避免 explorer 已被结束但未重启
                    api.prevent_close();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![get_cache_info, rebuild_icon_cache])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
