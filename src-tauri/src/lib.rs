use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager,
};

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
    app: Option<&AppHandle>,
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
    // headless 模式（右键菜单触发）没有 app，只记录步骤不推送事件
    if let Some(app) = app {
        let _ = app.emit("rebuild-progress", &rec);
    }
    steps.push(rec);
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
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
    perform_rebuild(Some(&app))
}

/// 重建核心流程：GUI 模式传入 app 推送进度事件；右键菜单 headless 模式传 None
fn perform_rebuild(app: Option<&AppHandle>) -> Result<RebuildResult, String> {
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

    // 路径解析成功后才置运行标记，避免异常提前返回时窗口永久无法关闭（GUI 模式才有状态）
    let running_flag = app.map(|app| app.state::<RebuildRunning>());
    if let Some(flag) = &running_flag {
        flag.0.store(true, Ordering::SeqCst);
    }

    // ---- 步骤 1：结束 explorer.exe，释放缓存文件句柄 ----
    record(app, &mut steps, 1, step1, "running", "正在执行 taskkill /f /im explorer.exe ...".into());
    match kill_explorer() {
        Ok(0) => record(app, &mut steps, 1, step1, "success", "已强制结束 explorer.exe".into()),
        Ok(128) => record(app, &mut steps, 1, step1, "warning", "explorer.exe 当前未在运行，无需结束".into()),
        Ok(code) => record(
            app,
            &mut steps,
            1,
            step1,
            "failed",
            format!("taskkill 返回错误码 {code}，进程可能仍在运行"),
        ),
        Err(e) => record(app, &mut steps, 1, step1, "failed", e),
    }

    // 等待系统释放缓存文件句柄
    thread::sleep(Duration::from_millis(800));

    // ---- 步骤 2：清理 Explorer 图标缓存目录 ----
    record(
        app,
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
        record(app, &mut steps, 2, step2, status, msg);
    } else {
        record(
            app,
            &mut steps,
            2,
            step2,
            "warning",
            format!("缓存目录不存在: {}", explorer_dir.display()),
        );
    }

    // ---- 步骤 3：删除 IconCache.db ----
    record(app, &mut steps, 3, step3, "running", "正在删除 IconCache.db ...".into());
    if icon_cache_db.is_file() {
        match remove_file_force(&icon_cache_db) {
            Ok(_) => {
                total_deleted += 1;
                record(app, &mut steps, 3, step3, "success", "已删除 IconCache.db".into());
            }
            Err(e) => {
                failed_files.push(format!("{}: {e}", icon_cache_db.display()));
                record(app, &mut steps, 3, step3, "failed", format!("删除失败: {e}"));
            }
        }
    } else {
        record(app, &mut steps, 3, step3, "warning", "IconCache.db 不存在，可能已被清理".into());
    }

    // ---- 步骤 4：重启 explorer.exe ----
    record(app, &mut steps, 4, step4, "running", "正在启动 explorer.exe ...".into());
    match Command::new("explorer.exe").spawn() {
        Ok(_) => record(app, &mut steps, 4, step4, "success", "explorer.exe 已重新启动".into()),
        Err(e) => record(
            app,
            &mut steps,
            4,
            step4,
            "failed",
            format!("启动失败: {e}，请打开任务管理器手动运行 explorer.exe"),
        ),
    }

    if let Some(flag) = &running_flag {
        flag.0.store(false, Ordering::SeqCst);
    }

    let success = steps.iter().all(|s| s.status != "failed");
    Ok(RebuildResult {
        success,
        total_deleted,
        failed_files,
        steps,
    })
}

/// 当前用户右键菜单（桌面 / 文件夹空白处）注册表集成，全程免管理员权限
mod context_menu {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteTreeW, RegOpenKeyExW, RegSetKeyValueW, HKEY, HKEY_CURRENT_USER,
        KEY_READ, REG_SZ,
    };

    /// 桌面与文件夹空白处右键共用 Directory\Background\shell 注册表位置
    const SHELL_KEY: &str = "Software\\Classes\\Directory\\Background\\shell\\IconCacheRebuilder";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// 写入一条 REG_SZ；name 为 None 时写键的默认值
    fn set_sz(subkey: &str, name: Option<&str>, value: &str) -> Result<(), String> {
        let subkey_w = wide(subkey);
        let name_w = name.map(wide);
        let value_w = wide(value);
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                subkey_w.as_ptr(),
                name_w.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
                REG_SZ,
                value_w.as_ptr().cast(),
                (value_w.len() * 2) as u32,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(format!("写注册表失败（错误码 {status}）: {subkey}"))
        }
    }

    pub fn install() -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| format!("无法获取程序路径: {e}"))?;
        let exe_str = exe.to_string_lossy().to_string();
        // 菜单显示名（键默认值）→ 图标 → 点击执行的命令
        set_sz(SHELL_KEY, None, "重建图标缓存")?;
        set_sz(SHELL_KEY, Some("Icon"), &format!("{exe_str},0"))?;
        set_sz(
            &format!("{SHELL_KEY}\\command"),
            None,
            &format!("\"{exe_str}\" --rebuild"),
        )?;
        Ok(())
    }

    pub fn uninstall() {
        let subkey_w = wide(SHELL_KEY);
        unsafe {
            RegDeleteTreeW(HKEY_CURRENT_USER, subkey_w.as_ptr());
        }
    }

    pub fn installed() -> bool {
        let subkey_w = wide(SHELL_KEY);
        let mut hkey: HKEY = Default::default();
        let status = unsafe {
            RegOpenKeyExW(HKEY_CURRENT_USER, subkey_w.as_ptr(), 0, KEY_READ, &mut hkey)
        };
        if status == 0 {
            unsafe { RegCloseKey(hkey) };
            true
        } else {
            false
        }
    }
}

#[tauri::command]
fn is_context_menu_enabled() -> bool {
    context_menu::installed()
}

#[tauri::command]
fn set_context_menu(enabled: bool) -> Result<bool, String> {
    if enabled {
        context_menu::install()?;
    } else {
        context_menu::uninstall();
    }
    Ok(enabled)
}

/// WorkBuddy 桌面右键菜单残留清理（键位来源：用户提供的 RemoveWorkBuddyMenu.reg）
/// 注册位置 HKLM×2 + HKCU×2 共 4 处；HKLM 处删除需管理员权限（runas 提权自身完成）
mod workbuddy_menu {
    use serde::Serialize;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteTreeW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
        KEY_READ,
    };
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    /// 待清理的 4 个注册表键：(根键, 子键路径, 界面展示名)
    const KEYS: [(HKEY, &str, &str); 4] = [
        (
            HKEY_LOCAL_MACHINE,
            "SOFTWARE\\Classes\\Directory\\Background\\shell\\WorkBuddy",
            "系统级（HKLM）· 桌面/文件夹空白处",
        ),
        (
            HKEY_LOCAL_MACHINE,
            "SOFTWARE\\Classes\\Directory\\shell\\WorkBuddy",
            "系统级（HKLM）· 文件夹",
        ),
        (
            HKEY_CURRENT_USER,
            "Software\\Classes\\Directory\\Background\\shell\\WorkBuddy",
            "当前用户（HKCU）· 桌面/文件夹空白处",
        ),
        (
            HKEY_CURRENT_USER,
            "Software\\Classes\\Directory\\shell\\WorkBuddy",
            "当前用户（HKCU）· 文件夹",
        ),
    ];

    #[derive(Clone, Serialize)]
    pub struct Status {
        /// 仍存在的注册键展示名
        pub found: Vec<String>,
        /// 存在系统级（HKLM）键，删除需要管理员权限
        pub needs_admin: bool,
    }

    #[derive(Serialize)]
    pub struct RemoveResult {
        pub removed: Vec<String>,
        pub missing: Vec<String>,
        pub failed: Vec<String>,
        /// 有键删除失败（当前权限不足），需提权（UAC）重试
        pub needs_elevation: bool,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    fn key_exists(hive: HKEY, subkey: &str) -> bool {
        let subkey_w = wide(subkey);
        let mut hkey: HKEY = Default::default();
        let status = unsafe { RegOpenKeyExW(hive, subkey_w.as_ptr(), 0, KEY_READ, &mut hkey) };
        if status == 0 {
            unsafe { RegCloseKey(hkey) };
            true
        } else {
            false
        }
    }

    pub fn check() -> Status {
        let mut found = Vec::new();
        let mut needs_admin = false;
        for (hive, path, label) in KEYS {
            if key_exists(hive, path) {
                found.push(label.to_string());
                if hive == HKEY_LOCAL_MACHINE {
                    needs_admin = true;
                }
            }
        }
        Status { found, needs_admin }
    }

    /// 以当前权限删除 4 处键位：HKCU 直接成功；HKLM 通常返回 5（拒绝访问）需要提权
    pub fn remove() -> RemoveResult {
        let mut removed = Vec::new();
        let mut missing = Vec::new();
        let mut failed = Vec::new();
        let mut needs_elevation = false;
        for (hive, path, label) in KEYS {
            if !key_exists(hive, path) {
                missing.push(label.to_string());
                continue;
            }
            let status = unsafe { RegDeleteTreeW(hive, wide(path).as_ptr()) };
            if status == 0 {
                removed.push(label.to_string());
            } else {
                needs_elevation = true;
                failed.push(format!("{label}（错误码 {status}）"));
            }
        }
        RemoveResult {
            removed,
            missing,
            failed,
            needs_elevation,
        }
    }

    /// 请求管理员权限（UAC）：以 runas 启动自身 --remove-workbuddy 完成清理。
    /// ShellExecuteW 异步返回，这里轮询注册表等待清理完成（用户确认 UAC 需要时间，最长 120 秒）
    pub fn remove_elevated() -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| format!("无法获取程序路径: {e}"))?;
        let exe_str = exe.to_string_lossy().to_string();
        let verb = wide("runas");
        let file = wide(&exe_str);
        let params = wide("--remove-workbuddy");
        let status = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                params.as_ptr(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        } as isize;
        // 返回值 <= 32 为失败；5 = SE_ERR_ACCESSDENIED，通常是在 UAC 窗口点了"否"
        if status <= 32 {
            return Err(if status == 5 {
                "已取消管理员授权（UAC），系统级注册未清理".to_string()
            } else {
                format!("请求管理员权限失败（错误码 {status}）")
            });
        }
        for _ in 0..240 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if check().found.is_empty() {
                return Ok(());
            }
        }
        Err("等待清理完成超时：请稍后在界面重新检测确认".to_string())
    }
}

#[tauri::command]
fn check_workbuddy_menu() -> workbuddy_menu::Status {
    workbuddy_menu::check()
}

#[tauri::command]
fn remove_workbuddy_menu() -> workbuddy_menu::RemoveResult {
    workbuddy_menu::remove()
}

#[tauri::command]
fn remove_workbuddy_menu_elevated() -> Result<(), String> {
    workbuddy_menu::remove_elevated()
}

/// 应用内一键发版：对选中项目目录执行 提交 → 推送 → 打标签 → 推送标签，
/// 推送标签后 GitHub Actions 自动构建并发布新版本
mod release {
    use serde::Serialize;
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::process::Command;
    use tauri::{AppHandle, Emitter};

    use super::StepRecord;

    #[derive(Clone, Serialize)]
    pub struct ReleaseInfo {
        /// 仓库最新的版本标签（如 v0.9.0）
        pub current_tag: String,
        pub branch: String,
        /// 建议的下一版本号（补丁位 +1），界面可手动修改
        pub next_version: String,
        /// 工作区待提交的改动数量
        pub changed_files: usize,
        /// 改动预览（最多 8 条，git status --porcelain 输出）
        pub changes_preview: Vec<String>,
    }

    #[derive(Serialize)]
    pub struct ReleaseResult {
        pub success: bool,
        pub tag: String,
        pub branch: String,
        /// 本次是否产生了新提交（工作区无改动时跳过提交）
        pub committed: bool,
        pub steps: Vec<StepRecord>,
    }

    /// 在指定目录执行 git 命令，成功返回 stdout，失败返回 stderr（便于定位原因）
    fn git(dir: &PathBuf, args: &[&str]) -> Result<String, String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .creation_flags(super::CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("无法启动 git（请确认已安装并在 PATH 中）: {e}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if err.is_empty() {
                format!("git {} 失败（退出码 {:?}）", args.join(" "), output.status.code())
            } else {
                err
            })
        }
    }

    /// 发布进度步骤事件（与界面 releaseSteps 1~5 一一对应）
    fn emit_step(
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
        let _ = app.emit("release-progress", &rec);
        steps.push(rec);
    }

    /// 规范化版本号：允许 v0.9.1 / 0.9.1 两种输入，统一为 v 前缀的 vX.Y.Z
    fn normalize_version(input: &str) -> Result<String, String> {
        let v = input
            .trim()
            .trim_start_matches(['v', 'V'])
            .to_string();
        let parts: Vec<&str> = v.split('.').collect();
        if parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        {
            Ok(format!("v{v}"))
        } else {
            Err(format!("版本号格式应为 vX.Y.Z（如 v0.9.1），当前输入: {}", input.trim()))
        }
    }

    /// 在最新标签基础上补丁位 +1，作为下一版本的默认建议
    fn bump_patch(tag: &str) -> String {
        let t = tag.trim_start_matches(['v', 'V']);
        let parts: Vec<u32> = t.split('.').filter_map(|p| p.parse().ok()).collect();
        if parts.len() == 3 {
            format!("v{}.{}.{}", parts[0], parts[1], parts[2] + 1)
        } else {
            format!("v{t}.1")
        }
    }

    /// 读取仓库发版信息：最新标签、分支、建议版本号、待提交改动
    pub fn info(dir: &str) -> Result<ReleaseInfo, String> {
        let dir = PathBuf::from(dir);
        if !dir.is_dir() {
            return Err(format!("目录不存在: {}", dir.display()));
        }
        git(&dir, &["rev-parse", "--is-inside-work-tree"])
            .map_err(|_| format!("{} 不是 git 仓库", dir.display()))?;
        let branch = git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
        // describe 取 HEAD 可达的最近标签；失败（如浅克隆）则按创建时间取最新
        let current_tag = match git(&dir, &["describe", "--tags", "--abbrev=0"]) {
            Ok(t) if !t.is_empty() => t,
            _ => {
                let tags = git(&dir, &["tag", "--sort=-creatordate"])?;
                tags.lines().next().unwrap_or_default().to_string()
            }
        };
        if current_tag.is_empty() {
            return Err("仓库尚无版本标签，无法推算下一版本号".into());
        }
        let status = git(&dir, &["status", "--porcelain"])?;
        let lines: Vec<String> = status
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        Ok(ReleaseInfo {
            changed_files: lines.len(),
            changes_preview: lines.iter().take(8).cloned().collect(),
            current_tag: current_tag.clone(),
            branch,
            next_version: bump_patch(&current_tag),
        })
    }

    pub fn run(
        app: &AppHandle,
        dir: String,
        version: String,
        message: String,
        notes: String,
    ) -> Result<ReleaseResult, String> {
        let dir_path = PathBuf::from(dir.trim());
        let tag = normalize_version(&version)?;
        let message = if message.trim().is_empty() {
            format!("chore: release {tag}")
        } else {
            message.trim().to_string()
        };
        // 发布说明写入附注标签，GitHub Actions 读取标签注释作为 Release 页面说明；留空回退提交说明
        let notes = if notes.trim().is_empty() {
            message.clone()
        } else {
            notes.trim().to_string()
        };
        let mut steps: Vec<StepRecord> = Vec::new();
        let s1 = "环境校验";
        let s2 = "提交改动";
        let s3 = "推送代码";
        let s4 = "创建附注标签";
        let s5 = "推送标签";

        // ---- 步骤 1：环境校验（git 可用、是仓库、标签不冲突） ----
        emit_step(app, &mut steps, 1, s1, "running", format!("检查仓库与标签 {tag} ..."));
        let branch = match git(&dir_path, &["rev-parse", "--abbrev-ref", "HEAD"]) {
            Ok(b) => b,
            Err(e) => {
                emit_step(app, &mut steps, 1, s1, "failed", e.clone());
                return Err(e);
            }
        };
        match git(&dir_path, &["tag", "-l", &tag]) {
            Ok(existing) if !existing.is_empty() => {
                let msg = format!("标签 {tag} 已存在，请填写更高的版本号");
                emit_step(app, &mut steps, 1, s1, "failed", msg.clone());
                return Err(msg);
            }
            Ok(_) => {}
            Err(e) => {
                emit_step(app, &mut steps, 1, s1, "failed", e.clone());
                return Err(e);
            }
        }
        emit_step(
            app,
            &mut steps,
            1,
            s1,
            "success",
            format!("git 就绪 · 分支 {branch} · 标签 {tag} 可用"),
        );

        // ---- 步骤 2：提交改动（无改动时跳过，标签指向当前提交） ----
        let status = git(&dir_path, &["status", "--porcelain"])?;
        let changed = status.lines().filter(|l| !l.trim().is_empty()).count();
        let committed = changed > 0;
        if changed == 0 {
            emit_step(app, &mut steps, 2, s2, "warning", "工作区无改动，跳过提交".into());
        } else {
            emit_step(
                app,
                &mut steps,
                2,
                s2,
                "running",
                format!("git add .（{changed} 处改动）"),
            );
            if let Err(e) = git(&dir_path, &["add", "."]).and_then(|_| {
                git(&dir_path, &["commit", "-m", &message])
            }) {
                emit_step(app, &mut steps, 2, s2, "failed", e.clone());
                return Err(format!("提交失败: {e}"));
            }
            emit_step(
                app,
                &mut steps,
                2,
                s2,
                "success",
                format!("已提交 {changed} 处改动：{message}"),
            );
        }

        // ---- 步骤 3：推送代码到 origin（需网络，SteamSpeed 代理下较快） ----
        emit_step(
            app,
            &mut steps,
            3,
            s3,
            "running",
            format!("正在推送 {branch} 到 origin（需网络，可能较慢）..."),
        );
        if let Err(e) = git(&dir_path, &["push", "origin", &branch]) {
            let msg = format!("推送失败: {e}。请确认已开启 SteamSpeed 后重试（本地提交已保留，无需重复提交）");
            emit_step(app, &mut steps, 3, s3, "failed", msg.clone());
            return Err(msg);
        }
        emit_step(
            app,
            &mut steps,
            3,
            s3,
            "success",
            format!("代码已推送到 origin/{branch}"),
        );

        // ---- 步骤 4：创建附注标签（注释即 GitHub Release 页面的发布说明） ----
        if let Err(e) = git(&dir_path, &["tag", "-a", &tag, "-m", &notes]) {
            emit_step(app, &mut steps, 4, s4, "failed", e.clone());
            return Err(format!("创建标签失败: {e}"));
        }
        emit_step(
            app,
            &mut steps,
            4,
            s4,
            "success",
            format!("已创建附注标签 {tag}（说明：{notes}）"),
        );

        // ---- 步骤 5：推送标签（触发 GitHub Actions 自动构建发布） ----
        emit_step(
            app,
            &mut steps,
            5,
            s5,
            "running",
            format!("正在推送标签 {tag}（触发 GitHub Actions 自动构建）..."),
        );
        match git(&dir_path, &["push", "origin", &tag]) {
            Ok(_) => {
                emit_step(
                    app,
                    &mut steps,
                    5,
                    s5,
                    "success",
                    "标签已推送，Actions 构建中，约 7~10 分钟后自动发布".into(),
                );
                Ok(ReleaseResult {
                    success: true,
                    tag,
                    branch,
                    committed,
                    steps,
                })
            }
            Err(e) => {
                // 推送失败则回滚本地标签，避免下次发版时标签冲突校验卡住
                let _ = git(&dir_path, &["tag", "-d", &tag]);
                let msg = format!(
                    "标签推送失败: {e}。已回滚本地标签，请确认网络后重试（代码已推送成功，重试时将跳过提交）"
                );
                emit_step(app, &mut steps, 5, s5, "failed", msg.clone());
                Err(msg)
            }
        }
    }
}

#[tauri::command]
fn get_release_info(project_dir: String) -> Result<release::ReleaseInfo, String> {
    release::info(&project_dir)
}

#[tauri::command]
async fn release_version(
    app: AppHandle,
    project_dir: String,
    version: String,
    message: String,
    notes: String,
) -> Result<release::ReleaseResult, String> {
    release::run(&app, project_dir, version, message, notes)
}

/// 原生 MessageBox 弹窗（右键菜单 headless 模式的结果反馈）
fn show_message_box(title: &str, text: &str, error: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };
    let title_w = wide(title);
    let text_w = wide(text);
    let icon = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    unsafe {
        // TOPMOST + SETFOREGROUND：explorer 刚被重启，确保弹窗置顶可见
        MessageBoxW(
            Default::default(),
            text_w.as_ptr(),
            title_w.as_ptr(),
            MB_OK | icon | MB_TOPMOST | MB_SETFOREGROUND,
        );
    }
}

/// 右键菜单触发的无窗口模式：直接重建，结果弹窗反馈后退出
fn run_headless_rebuild() {
    let result = perform_rebuild(None);
    let (text, error) = match &result {
        Ok(r) if r.success => (
            format!(
                "重建完成，共清理 {} 个缓存文件。\n桌面与任务栏已恢复，图标显示异常通常随之消失。",
                r.total_deleted
            ),
            false,
        ),
        Ok(r) => {
            let mut lines = format!(
                "重建结束：已清理 {} 个文件，{} 个删除失败（可能被其他进程占用）。",
                r.total_deleted,
                r.failed_files.len()
            );
            for f in r.failed_files.iter().take(5) {
                lines.push_str(&format!("\n失败: {f}"));
            }
            if r.failed_files.len() > 5 {
                lines.push_str(&format!("\n... 等共 {} 个失败项", r.failed_files.len()));
            }
            lines.push_str("\n\n可关闭占用缓存的程序后重试。");
            (lines, true)
        }
        Err(e) => (format!("重建失败: {e}"), true),
    };
    show_message_box("系统优化工具", &text, error);
    std::process::exit(0);
}

/// 提权清理模式（--remove-workbuddy）：删除全部 WorkBuddy 键位后弹窗反馈退出
/// 与界面的 remove_workbuddy_menu_elevated 命令配合：runas 启动自身并轮询注册表确认
fn run_headless_remove_workbuddy() {
    let result = workbuddy_menu::remove();
    let (text, error) = if result.failed.is_empty() {
        (
            format!(
                "WorkBuddy 右键菜单清理完成。\n共删除 {} 处注册（{} 处原本不存在）。",
                result.removed.len(),
                result.missing.len()
            ),
            false,
        )
    } else {
        (
            format!(
                "WorkBuddy 右键菜单清理结束：{} 处已删除，{} 处失败。\n{}",
                result.removed.len(),
                result.failed.len(),
                result.failed.join("\n")
            ),
            true,
        )
    };
    show_message_box("系统优化工具", &text, error);
    std::process::exit(0);
}

/// 恢复并聚焦主窗口（托盘左键单击 / 托盘菜单「显示主界面」）
fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 右键菜单静默模式：不创建窗口，直接重建后弹窗反馈
    if std::env::args().any(|a| a == "--rebuild") {
        run_headless_rebuild();
        return;
    }
    // 管理员提权清理 WorkBuddy 菜单：删除注册表键后弹窗反馈退出
    if std::env::args().any(|a| a == "--remove-workbuddy") {
        run_headless_remove_workbuddy();
        return;
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(RebuildRunning(AtomicBool::new(false)))
        .setup(|app| {
            // 系统托盘：窗口最小化后隐藏到托盘，从这里恢复或退出
            let show_item = MenuItem::with_id(app, "tray-show", "显示主界面", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "tray-quit", "退出", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show_item, &quit_item])?;
            TrayIconBuilder::with_id("main-tray")
                .icon(
                    app.default_window_icon()
                        .expect("missing default window icon")
                        .clone(),
                )
                .tooltip("系统优化工具")
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "tray-show" => show_main_window(app),
                    "tray-quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // 左键单击托盘图标：恢复主窗口
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                let running = window.state::<RebuildRunning>().0.load(Ordering::SeqCst);
                if running {
                    // 重建期间禁止关闭窗口，避免 explorer 已被结束但未重启
                    api.prevent_close();
                }
            }
            tauri::WindowEvent::Resized(_) => {
                // 最小化时隐藏窗口（最小化到托盘）：任务栏不保留，从托盘图标恢复
                if window.is_minimized().unwrap_or(false) {
                    let _ = window.hide();
                }
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            get_cache_info,
            rebuild_icon_cache,
            set_context_menu,
            is_context_menu_enabled,
            check_workbuddy_menu,
            remove_workbuddy_menu,
            remove_workbuddy_menu_elevated,
            get_release_info,
            release_version
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
