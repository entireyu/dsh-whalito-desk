use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Emitter, State};
#[cfg(windows)]
use tauri::Manager;

use crate::state::{self, AppState, EnvInfo, ServerStatus, Settings};

/// 可克隆的服务器共享状态快照（只含 Arc 字段），用于把阻塞工作丢到 spawn_blocking 线程。
pub struct Shared {
    pub pid: Arc<Mutex<Option<u32>>>,
    pub stop: Arc<AtomicBool>,
    pub url: Arc<Mutex<Option<String>>>,
    pub logs: Arc<Mutex<VecDeque<String>>>,
    pub settings: Arc<Mutex<Settings>>,
    /// 已装 DSH 是否支持 `--no-open`（None = 未探测，进程级缓存）。
    pub no_open_supported: Arc<Mutex<Option<bool>>>,
    /// 安装/更新互斥锁（与 AppState.install_lock 同一把锁）。
    pub install_lock: Arc<Mutex<()>>,
}

impl Shared {
    pub fn from_state(st: &AppState) -> Self {
        Self {
            pid: Arc::clone(&st.pid),
            stop: Arc::clone(&st.stop_requested),
            url: Arc::clone(&st.server_url),
            logs: Arc::clone(&st.logs),
            settings: Arc::clone(&st.settings),
            no_open_supported: Arc::clone(&st.no_open_supported),
            install_lock: Arc::clone(&st.install_lock),
        }
    }

    fn node_dir(&self) -> Option<String> {
        self.settings.lock().unwrap().node_dir.clone()
    }

    fn registry(&self) -> String {
        self.settings.lock().unwrap().registry.trim().to_string()
    }

    /// DSH 版本偏好（normalize 后）：仅 "next" 走预发布通道，其余一律 latest。
    fn dsh_channel(&self) -> &'static str {
        state::normalize_dsh_channel(&self.settings.lock().unwrap().dsh_channel)
    }
}

/// 按版本偏好拼出 npm 安装/查询规格：`@deepseek-ai/dsh@latest` / `@deepseek-ai/dsh@next`。
fn dsh_update_spec(channel: &str) -> String {
    format!("@deepseek-ai/dsh@{channel}")
}

#[tauri::command]
pub async fn detect_env(st: State<'_, AppState>) -> Result<EnvInfo, String> {
    let node_dir = st.settings.lock().unwrap().node_dir.clone();
    tauri::async_runtime::spawn_blocking(move || state::detect_env(node_dir.as_deref()))
        .await
        .map_err(|e| e.to_string())
}

/// 当前平台标识（"windows" / "macos" / "linux"），供前端切换安装 UI。
#[tauri::command]
pub fn get_platform() -> &'static str {
    std::env::consts::OS
}

#[cfg(windows)]
fn winget_node(app: &AppHandle, shared: &Shared, upgrade: bool) -> Result<EnvInfo, String> {
    let label = if upgrade { "升级" } else { "安装" };
    state::push_log(
        &shared.logs,
        &format!("[系统] 开始通过 winget {label} Node.js LTS（可能弹出 UAC 授权窗口，请点击“是”）"),
    );
    let _ = app.emit("install-stage", "install");
    let args: Vec<&str> = if upgrade {
        vec![
            "upgrade",
            "--id",
            "OpenJS.NodeJS.LTS",
            "--silent",
            "--accept-package-agreements",
            "--accept-source-agreements",
            "--exact",
        ]
    } else {
        vec![
            "install",
            "OpenJS.NodeJS.LTS",
            "--silent",
            "--accept-package-agreements",
            "--accept-source-agreements",
        ]
    };
    let result = state::run_streaming(app, "winget", &args);
    if let Err(e) = result {
        state::push_log(&shared.logs, &format!("[系统] winget {label}失败：{e}"));
        let _ = app.emit("install-stage", "error");
        return Err(format!(
            "Node.js {label}失败。可尝试「自定义安装」或到 https://nodejs.org 手动下载 LTS 版。"
        ));
    }
    state::push_log(&shared.logs, &format!("[系统] Node.js {label}完成，正在重新检测"));
    let node_dir = shared.node_dir();
    Ok(state::detect_env(node_dir.as_deref()))
}

#[tauri::command]
pub async fn install_node(app: AppHandle, st: State<'_, AppState>) -> Result<EnvInfo, String> {
    let shared = Shared::from_state(&st);
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(windows)]
        {
            winget_node(&app, &shared, false)
        }
        #[cfg(target_os = "macos")]
        {
            install_node_tarball(&app, &shared, false)
        }
        #[cfg(all(not(windows), not(target_os = "macos")))]
        {
            Err("Linux 暂不支持一键安装 Node.js，请用系统包管理器安装 Node.js ≥ 22.19。".to_string())
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn upgrade_node(app: AppHandle, st: State<'_, AppState>) -> Result<EnvInfo, String> {
    let shared = Shared::from_state(&st);
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(windows)]
        {
            winget_node(&app, &shared, true)
        }
        #[cfg(target_os = "macos")]
        {
            install_node_tarball(&app, &shared, true)
        }
        #[cfg(all(not(windows), not(target_os = "macos")))]
        {
            Err("Linux 暂不支持一键升级 Node.js".to_string())
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// macOS 一键安装 / 升级 Node.js：下载官方 darwin 架构 tar.gz，
/// 解压到 ~/Library/Application Support 下鲸仔专属目录（免管理员），
/// 并把绝对路径回写 node_dir（GUI 应用无 shell PATH，不能依赖 PATH 定位）。
#[cfg(target_os = "macos")]
fn install_node_tarball(
    app: &AppHandle,
    shared: &Shared,
    upgrade: bool,
) -> Result<EnvInfo, String> {
    let label = if upgrade { "升级" } else { "安装" };
    let version = state::latest_node_lts_major22().unwrap_or_else(|| "22.19.0".to_string());
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => return Err(format!("不支持的处理器架构：{other}")),
    };
    let registry = shared.registry();
    let base = state::node_dist_base(&registry);
    let file = format!("node-v{version}-darwin-{arch}.tar.gz");
    let url = format!("{base}/v{version}/{file}");
    let tarball = std::env::temp_dir().join(&file);

    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let dest_dir = Path::new(&home)
        .join("Library")
        .join("Application Support")
        .join("com.deepseek.dsh-launcher")
        .join("node")
        .join(&version);
    let node_dir = dest_dir.join("bin");

    let _ = app.emit("install-stage", "download");
    state::push_log(
        &shared.logs,
        &format!("[系统] 正在下载 Node.js {version}（{url}）"),
    );
    state::download_file(&url, &tarball)?;

    let _ = app.emit("install-stage", "extract");
    state::push_log(
        &shared.logs,
        &format!("[系统] 下载完成，正在解压到 {}", dest_dir.display()),
    );
    let _ = std::fs::remove_dir_all(&dest_dir);
    std::fs::create_dir_all(&dest_dir).map_err(|e| format!("创建目录失败：{e}"))?;
    let status = std::process::Command::new("/usr/bin/tar")
        .args([
            "-xzf",
            tarball.to_str().unwrap_or(""),
            "--strip-components=1",
            "-C",
            dest_dir.to_str().unwrap_or(""),
        ])
        .status()
        .map_err(|e| format!("解压失败：{e}"))?;
    let _ = std::fs::remove_file(&tarball);
    if !status.success() {
        return Err("Node.js 压缩包解压失败".to_string());
    }

    // 回写 node_dir 并持久化（重启后仍可定位）
    let node_dir_str = node_dir.to_string_lossy().to_string();
    {
        let mut s = shared.settings.lock().unwrap();
        s.node_dir = Some(node_dir_str.clone());
    }
    let s = shared.settings.lock().unwrap().clone();
    state::save_settings(app, &s)?;

    state::push_log(&shared.logs, &format!("[系统] Node.js {label}完成，正在重新检测"));
    Ok(state::detect_env(Some(&node_dir_str)))
}

#[cfg(windows)]
fn install_node_nvm_inner(app: &AppHandle, shared: &Shared) -> Result<EnvInfo, String> {
    let nvm = state::detect_nvm().ok_or("未检测到 nvm，无法使用 nvm 安装 Node.js。")?;
    let version = state::latest_node_lts_major22().unwrap_or_else(|| "22.19.0".to_string());
    state::push_log(
        &shared.logs,
        &format!("[系统] 检测到 nvm，开始安装 Node.js {version}"),
    );
    let _ = app.emit("install-stage", "install");
    state::run_streaming(app, &nvm, &["install", &version])?;
    let _ = app.emit("install-stage", "use");
    state::push_log(
        &shared.logs,
        &format!("[系统] 正在切换 Node 版本（nvm use {version}，若需要管理员权限请确认）"),
    );
    state::run_streaming(app, &nvm, &["use", &version])?;
    state::push_log(&shared.logs, "[系统] nvm 安装并切换完成，正在重新检测");
    let node_dir = shared.node_dir();
    Ok(state::detect_env(node_dir.as_deref()))
}

#[tauri::command]
pub async fn install_node_nvm(app: AppHandle, st: State<'_, AppState>) -> Result<EnvInfo, String> {
    let shared = Shared::from_state(&st);
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(windows)]
        {
            install_node_nvm_inner(&app, &shared)
        }
        #[cfg(target_os = "macos")]
        {
            install_node_nvm_macos(&app, &shared)
        }
        #[cfg(all(not(windows), not(target_os = "macos")))]
        {
            Err("当前平台不支持 nvm 安装".to_string())
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// macOS：nvm 是 shell 函数，必须 source nvm.sh 后在同一 shell 里执行。
/// 安装并设为默认后，用 `nvm which 22` 解析出 node 绝对路径回写 node_dir
/// （stdout 最后一行即路径）。
#[cfg(target_os = "macos")]
fn install_node_nvm_macos(app: &AppHandle, shared: &Shared) -> Result<EnvInfo, String> {
    let nvm_sh = state::detect_nvm()
        .ok_or("未检测到 nvm（~/.nvm/nvm.sh），无法使用 nvm 安装 Node.js。")?;
    let script = format!(
        "source \"{nvm_sh}\" && nvm install 22 && nvm alias default 22 && nvm use 22 && printf '%s' \"$(nvm which 22)\""
    );
    state::push_log(
        &shared.logs,
        &format!("[系统] 检测到 nvm，开始安装 Node.js 22（{nvm_sh}）"),
    );
    let _ = app.emit("install-stage", "install");
    let out = state::run_output("zsh", &["-lc", &script])
        .or_else(|_| state::run_output("bash", &["-lc", &script]))?;
    let node_bin = out
        .lines()
        .next_back()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or("nvm 安装完成，但未能解析 Node 路径")?;
    let node_dir = Path::new(&node_bin)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .ok_or("nvm 安装完成，但未能解析 Node 目录")?;
    {
        let mut s = shared.settings.lock().unwrap();
        s.node_dir = Some(node_dir.clone());
    }
    let s = shared.settings.lock().unwrap().clone();
    state::save_settings(app, &s)?;
    state::push_log(&shared.logs, "[系统] nvm 安装并切换完成，正在重新检测");
    Ok(state::detect_env(Some(&node_dir)))
}

/// 切换到 nvm 中已安装的指定版本（只切换不下载，目标版本必须已安装）。
/// 版本号来自检测结果的 nvm_switch_version。切换后重新检测环境。
#[tauri::command]
pub async fn switch_node_nvm(
    app: AppHandle,
    st: State<'_, AppState>,
    version: String,
) -> Result<EnvInfo, String> {
    let shared = Shared::from_state(&st);
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(windows)]
        {
            switch_node_nvm_inner(&app, &shared, &version)
        }
        #[cfg(target_os = "macos")]
        {
            switch_node_nvm_macos(&app, &shared, &version)
        }
        #[cfg(all(not(windows), not(target_os = "macos")))]
        {
            Err("当前平台不支持 nvm 切换".to_string())
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(windows)]
fn switch_node_nvm_inner(
    app: &AppHandle,
    shared: &Shared,
    version: &str,
) -> Result<EnvInfo, String> {
    let nvm = state::detect_nvm().ok_or("未检测到 nvm，无法切换 Node 版本。")?;
    state::push_log(
        &shared.logs,
        &format!("[系统] 正在切换 Node 版本（nvm use {version}，若需要管理员权限请确认）"),
    );
    state::run_streaming(app, &nvm, &["use", version])?;
    state::push_log(&shared.logs, "[系统] nvm 切换完成，正在重新检测");
    let node_dir = shared.node_dir();
    Ok(state::detect_env(node_dir.as_deref()))
}

/// macOS：nvm 是 shell 函数，source nvm.sh 后在同一 shell 里
/// 设为默认并切换，再用 `nvm which <version>` 解析出 node 路径回写 node_dir。
#[cfg(target_os = "macos")]
fn switch_node_nvm_macos(
    app: &AppHandle,
    shared: &Shared,
    version: &str,
) -> Result<EnvInfo, String> {
    let nvm_sh = state::detect_nvm()
        .ok_or("未检测到 nvm（~/.nvm/nvm.sh），无法切换 Node 版本。")?;
    let script = format!(
        "source \"{nvm_sh}\" && nvm alias default {version} && nvm use {version} && printf '%s' \"$(nvm which {version})\""
    );
    state::push_log(
        &shared.logs,
        &format!("[系统] 正在切换 Node 版本（nvm use {version}）"),
    );
    let out = state::run_output("zsh", &["-lc", &script])
        .or_else(|_| state::run_output("bash", &["-lc", &script]))?;
    let node_bin = out
        .lines()
        .next_back()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or("nvm 切换完成，但未能解析 Node 路径")?;
    let node_dir = Path::new(&node_bin)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .ok_or("nvm 切换完成，但未能解析 Node 目录")?;
    {
        let mut s = shared.settings.lock().unwrap();
        s.node_dir = Some(node_dir.clone());
    }
    let s = shared.settings.lock().unwrap().clone();
    state::save_settings(app, &s)?;
    state::push_log(&shared.logs, "[系统] nvm 切换完成，正在重新检测");
    Ok(state::detect_env(Some(&node_dir)))
}

fn install_node_portable_inner(
    app: &AppHandle,
    shared: &Shared,
    dir: String,
) -> Result<EnvInfo, String> {
    let dir = dir.trim().to_string();
    if dir.is_empty() {
        return Err("请选择 Node.js 安装目录".to_string());
    }
    let version = state::latest_node_lts_major22().unwrap_or_else(|| "22.19.0".to_string());
    let registry = shared.registry();
    let base = state::node_dist_base(&registry);

    // 平台差异：包文件名与解压后 node 所在目录（Windows 保留原有 zip 行为）
    #[cfg(windows)]
    let (file, node_dir) = {
        let f = format!("node-v{version}-win-x64.zip");
        (f, PathBuf::from(&dir))
    };
    #[cfg(target_os = "macos")]
    let (file, node_dir) = {
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "x64",
            other => return Err(format!("不支持的处理器架构：{other}")),
        };
        let f = format!("node-v{version}-darwin-{arch}.tar.gz");
        (f, PathBuf::from(&dir).join("bin"))
    };
    #[cfg(all(not(windows), not(target_os = "macos")))]
    return Err("当前平台不支持自定义目录安装 Node.js".to_string());

    let url = format!("{base}/v{version}/{file}");
    let dl_path = std::env::temp_dir().join(&file);

    let _ = app.emit("install-stage", "download");
    state::push_log(
        &shared.logs,
        &format!("[系统] 正在下载 Node.js {version}（{url}）"),
    );
    state::download_file(&url, &dl_path)?;

    let _ = app.emit("install-stage", "extract");
    state::push_log(&shared.logs, &format!("[系统] 下载完成，正在解压到 {dir}"));
    #[cfg(windows)]
    state::extract_zip(&dl_path, Path::new(&dir))?;
    #[cfg(target_os = "macos")]
    {
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败：{e}"))?;
        let status = std::process::Command::new("/usr/bin/tar")
            .args([
                "-xzf",
                dl_path.to_str().unwrap_or(""),
                "--strip-components=1",
                "-C",
                Path::new(&dir).to_str().unwrap_or(""),
            ])
            .status()
            .map_err(|e| format!("解压失败：{e}"))?;
        if !status.success() {
            return Err("Node.js 压缩包解压失败".to_string());
        }
    }
    let _ = std::fs::remove_file(&dl_path);

    let node_dir_str = node_dir.to_string_lossy().to_string();
    {
        let mut s = shared.settings.lock().unwrap();
        s.node_dir = Some(node_dir_str.clone());
    }
    let s = shared.settings.lock().unwrap().clone();
    state::save_settings(app, &s)?;

    state::push_log(&shared.logs, "[系统] 便携版 Node.js 安装完成，正在重新检测");
    Ok(state::detect_env(Some(&node_dir_str)))
}

#[tauri::command]
pub async fn install_node_portable(
    app: AppHandle,
    st: State<'_, AppState>,
    dir: String,
) -> Result<EnvInfo, String> {
    let shared = Shared::from_state(&st);
    tauri::async_runtime::spawn_blocking(move || install_node_portable_inner(&app, &shared, dir))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn pick_node_dir(app: AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .blocking_pick_folder()
            .and_then(|fp| fp.into_path().ok().map(|p| p.to_string_lossy().to_string()))
    })
    .await
    .map_err(|e| e.to_string())
}

/// 通用目录选择器（工作目录 / 下载目录等）。取消返回 None。
#[tauri::command]
pub async fn pick_directory(app: AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .blocking_pick_folder()
            .and_then(|fp| fp.into_path().ok().map(|p| p.to_string_lossy().to_string()))
    })
    .await
    .map_err(|e| e.to_string())
}

fn install_dsh_inner(app: &AppHandle, shared: &Shared, spec: &str) -> Result<EnvInfo, String> {
    let node_dir = shared.node_dir();
    let env = state::detect_env(node_dir.as_deref());
    let node = env
        .node_path
        .clone()
        .ok_or("未检测到 Node.js，请先安装 Node.js。".to_string())?;
    let cli = state::npm_cli(&node).ok_or_else(|| {
        format!(
            "未找到 npm（node 位于 {node}，版本 {}），请确认 Node.js 安装完整。",
            env.version.as_deref().unwrap_or("未知")
        )
    })?;
    let install_prefix = env
        .install_prefix
        .clone()
        .ok_or("无法确定安装目录，请先安装 Node.js。".to_string())?;

    // 安装/更新前备份 DSH 配置与用户插件（best-effort，失败不阻断安装）。
    match crate::settings_plugin::backup_dsh_config(app) {
        Ok(Some(backup_dir)) => state::push_log(
            &shared.logs,
            &format!(
                "[系统] 已备份 DSH 配置与用户插件到 {}（若安装后出现问题可从这里恢复）",
                backup_dir.display()
            ),
        ),
        Ok(None) => {
            state::push_log(&shared.logs, "[系统] 无可备份的 DSH 配置，跳过备份");
        }
        Err(e) => state::push_log(&shared.logs, &format!("[系统] DSH 配置备份失败（不影响安装）：{e}")),
    }

    // 应用目录专用：重装 DSH 本体。不能整目录 remove_dir_all——
    // install_prefix 是 npm 全局前缀，用户可能在这里手动 `npm install -g
    // --prefix <install_prefix> <插件>` 装过第三方插件，整目录清空会连插件
    // 一起删掉（升级鲸仔后"以前安装的插件消失"的根因）。只清理 DSH 本体
    // 包与顶层入口，其余内容（用户插件）原样保留；npm 重装会补齐 DSH 依赖树。
    let dsh_pkg_dir = Path::new(&install_prefix)
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh");
    let _ = std::fs::create_dir_all(&install_prefix);

    // 旧安装先**暂存**（rename 而非删除）：npm install 若失败（网络/registry
    // 错误），用户原有的 DSH 安装不会丢失，自动回滚恢复；安装成功后才清理。
    // 暂存目录放在 prefix 根（node_modules 之外），npm 不会碰它。
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let stash_dir = Path::new(&install_prefix).join(format!(".dsh-reinstall-{ts}"));
    let stashed = stash_dsh_files(&dsh_pkg_dir, &stash_dir, Path::new(&install_prefix));

    let registry = shared.registry();

    let mut args: Vec<String> = vec![
        cli.to_string_lossy().to_string(),
        "install".to_string(),
        "-g".to_string(),
        "--prefix".to_string(),
        install_prefix.clone(),
        spec.to_string(),
        "--no-audit".to_string(),
        "--no-fund".to_string(),
        // macOS：让 npm 把 node 所在目录加入生命周期脚本的 PATH
        // （koffi 等原生依赖的安装脚本执行 `node ./cnoke.cjs` 需要找到 node）
        "--scripts-prepend-node-path=true".to_string(),
    ];
    if !registry.is_empty() {
        args.push("--registry".to_string());
        args.push(registry);
    }

    let _ = app.emit("install-stage", "install");
    state::push_log(
        &shared.logs,
        &format!("[系统] 旧版本已暂存（安装失败可自动回滚），开始安装 {spec} 到 {install_prefix}"),
    );
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    // node 所在目录置顶注入 PATH：koffi 等生命周期脚本 `node ./cnoke.cjs`
    // 必须能找到 node（macOS GUI 环境 PATH 极简，此处绝对兜底）。
    let node_bin_dir = Path::new(&node)
        .parent()
        .map(|p| p.to_string_lossy().to_string());
    let install_result =
        state::run_streaming_with_path(app, &node, &arg_refs, node_bin_dir.as_deref());
    if let Err(e) = &install_result {
        // 安装失败：恢复暂存的旧安装（npm 可能写入的半成品先清掉）。
        rollback_stash(&stash_dir, &stashed);
        state::push_log(
            &shared.logs,
            "[系统] 安装失败，已自动回滚到安装前的 DSH 版本",
        );
        return Err(format!("{e}\n安装失败，已自动回滚到安装前的版本，请检查网络后重试。"));
    }

    let _ = app.emit("install-stage", "verify");
    state::push_log(&shared.logs, "[系统] 安装完成，正在校验");

    // 显式校验：detect_env 会吞掉 dsh 启动报错，这里把真实错误返回给用户
    // （macOS 上 dsh 顶层 import 链含原生模块，--version 失败即安装不可用）
    if let Some(bin) = state::dsh_bin(&install_prefix) {
        let bin_s = bin.to_string_lossy().to_string();
        if let Err(e) = state::run_output(&node, &[&bin_s, "--version"]) {
            state::push_log(&shared.logs, &format!("[系统] Harness 校验失败：{e}"));
            rollback_stash(&stash_dir, &stashed);
            state::push_log(
                &shared.logs,
                "[系统] 校验失败，已自动回滚到安装前的 DSH 版本",
            );
            return Err(format!("Harness 已安装但校验失败：{e}\n已自动回滚到安装前的版本。"));
        }
    }

    // 安装成功：清理暂存的旧安装（旧版本已被新版本取代）。
    commit_stash(&stash_dir);

    Ok(state::detect_env(node_dir.as_deref()))
}

/// 把旧 DSH 包目录与入口 shim 暂存（rename）到 stash_dir，返回 (原位置, 暂存位置) 列表。
/// rename 失败（目录被占用等极端情况）时保持旧行为：删除并继续安装。
fn stash_dsh_files(
    dsh_pkg_dir: &Path,
    stash_dir: &Path,
    install_prefix: &Path,
) -> Vec<(PathBuf, PathBuf)> {
    let mut stashed: Vec<(PathBuf, PathBuf)> = Vec::new();
    if dsh_pkg_dir.exists() || ["dsh", "dsh.cmd", "dsh.ps1"]
        .iter()
        .any(|s| Path::new(install_prefix).join(s).exists())
    {
        // rename 不创建父目录：先建暂存目录，失败即降级为旧行为。
        if std::fs::create_dir_all(stash_dir).is_err() {
            let _ = std::fs::remove_dir_all(dsh_pkg_dir);
            for shim in ["dsh", "dsh.cmd", "dsh.ps1"] {
                let _ = std::fs::remove_file(Path::new(install_prefix).join(shim));
            }
            return stashed;
        }
    }
    if dsh_pkg_dir.exists() {
        let to = stash_dir.join("dsh-pkg");
        if std::fs::rename(dsh_pkg_dir, &to).is_ok() {
            stashed.push((dsh_pkg_dir.to_path_buf(), to));
        } else {
            let _ = std::fs::remove_dir_all(dsh_pkg_dir);
        }
    }
    for shim in ["dsh", "dsh.cmd", "dsh.ps1"] {
        let p = Path::new(install_prefix).join(shim);
        if p.exists() {
            let to = stash_dir.join(shim);
            if std::fs::rename(&p, &to).is_ok() {
                stashed.push((p.clone(), to));
            } else {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    stashed
}

/// 安装失败回滚：清掉 npm 可能写入的半成品后，把暂存的旧安装逐项恢复。
fn rollback_stash(stash_dir: &Path, stashed: &[(PathBuf, PathBuf)]) {
    for (orig, to) in stashed.iter().rev() {
        let _ = std::fs::remove_file(orig);
        let _ = std::fs::remove_dir_all(orig);
        let _ = std::fs::rename(to, orig);
    }
    let _ = std::fs::remove_dir_all(stash_dir);
}

/// 安装成功：删除暂存的旧安装。
fn commit_stash(stash_dir: &Path) {
    let _ = std::fs::remove_dir_all(stash_dir);
}

#[tauri::command]
pub async fn install_dsh(app: AppHandle, st: State<'_, AppState>) -> Result<EnvInfo, String> {
    let shared = Shared::from_state(&st);
    let app_for_sync = app.clone();
    let no_open_supported = Arc::clone(&shared.no_open_supported);
    // 首次安装固定 latest（稳定版）：版本偏好只影响检查与更新，
    // 避免预发布版直接用于全新环境；装好后可切偏好再更新。
    let env = tauri::async_runtime::spawn_blocking(move || {
        // 安装/更新互斥：全程持锁，start_server 的 try_lock 在此期间会拒绝启动。
        let _install_guard = shared.install_lock.lock().unwrap();
        // 防御：即使前端漏停（或服务器由外部启动），安装前也强制先停止，
        // 避免 Windows 下运行中的服务器锁定 node_modules 导致重装半途失败。
        let _ = stop_server_inner(&app, &shared);
        install_dsh_inner(&app, &shared, "@deepseek-ai/dsh")
    })
    .await
    .map_err(|e| e.to_string())??;
    // 安装/更新会清空应用目录，重同步鲸仔设置分区插件。
    let _ = crate::settings_plugin::ensure_settings_plugin(&app_for_sync);
    // 已装 DSH 变了（可能支持/不再支持 --no-open），失效缓存让下次启动重探。
    *no_open_supported.lock().unwrap() = None;
    Ok(env)
}

#[tauri::command]
pub async fn update_dsh(app: AppHandle, st: State<'_, AppState>) -> Result<EnvInfo, String> {
    let shared = Shared::from_state(&st);
    let app_for_sync = app.clone();
    let no_open_supported = Arc::clone(&shared.no_open_supported);
    let spec = dsh_update_spec(shared.dsh_channel());
    let env = tauri::async_runtime::spawn_blocking(move || {
        // 与 install_dsh 相同的互斥与停服防御（见上）。
        let _install_guard = shared.install_lock.lock().unwrap();
        let _ = stop_server_inner(&app, &shared);
        install_dsh_inner(&app, &shared, &spec)
    })
    .await
    .map_err(|e| e.to_string())??;
    let _ = crate::settings_plugin::ensure_settings_plugin(&app_for_sync);
    // 已装 DSH 变了（可能支持/不再支持 --no-open），失效缓存让下次启动重探。
    *no_open_supported.lock().unwrap() = None;
    Ok(env)
}

fn verify_dsh_inner(node_dir: Option<String>) -> Result<String, String> {
    let env = state::detect_env(node_dir.as_deref());
    let node = env.node_path.clone().ok_or("未检测到 Node.js。".to_string())?;
    let bin = state::resolve_dsh_bin(&env).ok_or("未安装 DeepSeek Harness。".to_string())?;
    let version = state::run_output(&node, &[bin.to_str().unwrap_or(""), "--version"])?;
    state::run_output(&node, &[bin.to_str().unwrap_or(""), "web", "--dump-default-config"])?;
    Ok(format!("DeepSeek Harness {version} 安装正常，可正常启动"))
}

#[tauri::command]
pub async fn verify_dsh(st: State<'_, AppState>) -> Result<String, String> {
    let node_dir = st.settings.lock().unwrap().node_dir.clone();
    tauri::async_runtime::spawn_blocking(move || verify_dsh_inner(node_dir))
        .await
        .map_err(|e| e.to_string())?
}

/// 查询 DSH 最新版本（`npm view <spec> version`，走所选镜像源）。
/// 任何一步失败（无 node / 无 npm / 网络错误）返回 None；供设置分区检查与
/// 后台每小时更新通知共用。
pub fn latest_dsh_version(
    node_dir: Option<String>,
    registry: &str,
    channel: &str,
) -> Option<String> {
    let env = state::detect_env(node_dir.as_deref());
    let node = env.node_path?;
    let cli = state::npm_cli(&node)?;
    let spec = dsh_update_spec(channel);
    let mut args: Vec<String> = vec![
        cli.to_string_lossy().to_string(),
        "view".to_string(),
        spec,
        "version".to_string(),
    ];
    if !registry.is_empty() {
        args.push("--registry".to_string());
        args.push(registry.to_string());
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    state::run_output(&node, &arg_refs)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[tauri::command]
pub async fn check_latest_version(st: State<'_, AppState>) -> Result<Option<String>, String> {
    let shared = Shared::from_state(&st);
    let node_dir = shared.node_dir();
    let registry = shared.registry();
    let channel = shared.dsh_channel().to_string();
    tauri::async_runtime::spawn_blocking(move || {
        Ok(latest_dsh_version(node_dir, &registry, &channel))
    })
    .await
    .map_err(|e| e.to_string())?
}

pub fn start_server_impl(app: &AppHandle, shared: &Shared) -> Result<ServerStatus, String> {
    if shared.pid.lock().unwrap().is_some() {
        return Err("服务器已在运行".to_string());
    }
    // 安装/更新互斥：install/update 持锁期间拒绝启动（托盘/外部触发也走这里）。
    let _install_guard = shared
        .install_lock
        .try_lock()
        .map_err(|_| "正在安装/更新 DeepSeek Harness，请稍候再启动".to_string())?;

    let (port, workspace) = {
        let s = shared.settings.lock().unwrap();
        (s.port, s.workspace_dir.clone())
    };

    // 端口上可能已有外部启动的服务器，避免重复启动
    let probe = format!("http://127.0.0.1:{port}");
    if state::health(&probe) {
        return Err(format!(
            "端口 {port} 已有服务器在运行（可能由外部启动），无需重复启动"
        ));
    }

    let env = state::detect_env(shared.node_dir().as_deref());
    if !env.dsh_installed {
        return Err("尚未安装 DeepSeek Harness，请先点击“安装/更新”。".to_string());
    }
    let node = env
        .node_path
        .clone()
        .ok_or("未检测到 Node.js，请先安装。".to_string())?;
    let bin = state::resolve_dsh_bin(&env).ok_or("找不到 dsh 入口文件，请重新安装 Harness。".to_string())?;
    let bin_s = bin.to_string_lossy().to_string();

    // DSH rc8+ 默认在 Web 就绪后自动打开系统浏览器；鲸仔以内嵌页面为主，
    // 追加 `--no-open` 抑制。老版本 dsh 不认识该参数（commander 遇到未知选项
    // 会报错退出导致启动失败），故首次启动用 `dsh web --help` 探测一次并缓存；
    // 探测失败（如老版本没有 web 子命令）按不支持处理，不影响启动。
    let no_open = {
        let cache = shared.no_open_supported.lock().unwrap();
        match *cache {
            Some(v) => v,
            None => {
                drop(cache);
                let probe =
                    state::run_output(&node, &[bin_s.as_str(), "web", "--help"]).unwrap_or_default();
                let v = probe.contains("--no-open");
                *shared.no_open_supported.lock().unwrap() = Some(v);
                v
            }
        }
    };

    // 启动前强制同步鲸仔设置分区插件（幂等），保证 Loader 能解析插件条目。
    crate::settings_plugin::ensure_settings_plugin(app)?;

    shared.stop.store(false, Ordering::SeqCst);
    *shared.url.lock().unwrap() = None;

    let mut cmd = std::process::Command::new(&node);
    // DSH 家目录与插件同步保持一致（测试构建使用隔离的 ~/.dsh-test）。
    cmd.env("DSH_HOME", crate::settings_plugin::dsh_home());
    // macOS GUI 进程 PATH 极简：注入用户 shell 的 PATH，保证 dsh 的技能 /
    // 子进程能找到 git 等常用工具；其他平台保持默认继承。
    if let Some(p) = state::effective_path() {
        cmd.env("PATH", p);
    }
    // 预留：把鲸仔主窗口句柄注入 DSH 子进程（DSH_DIALOG_OWNER_HWND）。
    // 不改动 DSH 源码；当前 DSH 版本忽略该变量，未来其原生目录选择器
    // 支持 owner 窗口后即可挂在鲸仔窗口下（任务栏图标跟随鲸仔）。
    #[cfg(windows)]
    if let Some(window) = app.get_webview_window("main") {
        if let Ok(hwnd) = window.hwnd() {
            cmd.env("DSH_DIALOG_OWNER_HWND", (hwnd.0 as usize).to_string());
        }
    }
    cmd.arg(&bin_s)
        .arg("web")
        .arg("--port")
        .arg(port.to_string());
    if no_open {
        cmd.arg("--no-open");
    }
    if let Some(ws) = workspace.filter(|w| !w.trim().is_empty()) {
        cmd.current_dir(&ws);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }

    let mut child = cmd.spawn().map_err(|e| format!("启动失败：{e}"))?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    *shared.pid.lock().unwrap() = Some(pid);
    state::refresh_tray(app, true);
    state::push_log(
        &shared.logs,
        &format!("[系统] 正在启动 dsh web --port {port}（pid {pid}）"),
    );

    let mut streams: Vec<Box<dyn std::io::Read + Send>> = Vec::new();
    if let Some(s) = stdout {
        streams.push(Box::new(s));
    }
    if let Some(s) = stderr {
        streams.push(Box::new(s));
    }
    for stream in streams {
        let app = app.clone();
        let logs = Arc::clone(&shared.logs);
        let url = Arc::clone(&shared.url);
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stream).lines() {
                match line {
                    Ok(line) => {
                        state::push_log(&logs, &line);
                        let _ = app.emit("log", &line);
                        if let Some(u) = state::extract_url(&line) {
                            let mut slot = url.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(u.clone());
                                drop(slot);
                                let _ = app.emit("server-url", &u);
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    let app2 = app.clone();
    let pid_slot = Arc::clone(&shared.pid);
    let url_slot = Arc::clone(&shared.url);
    let stop = Arc::clone(&shared.stop);
    let logs = Arc::clone(&shared.logs);
    std::thread::spawn(move || {
        let result = child.wait();
        let was_stopped = stop.load(Ordering::SeqCst);
        let mut p = pid_slot.lock().unwrap();
        let is_current = *p == Some(pid);
        if is_current {
            *p = None;
        }
        drop(p);
        if is_current {
            *url_slot.lock().unwrap() = None;
            if !was_stopped {
                let code = result.as_ref().ok().and_then(|s| s.code()).unwrap_or(-1);
                state::refresh_tray(&app2, false);
                state::push_log(&logs, &format!("[系统] 服务器进程已退出（退出码 {code}）"));
                let _ = app2.emit("server-exited", code);
            }
        }
    });

    // 就绪等待：轮询健康检查（不依赖 stdout 里 URL 的抽取），带超时。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if state::health(&probe) {
            ready = true;
            break;
        }
        if shared.pid.lock().unwrap().is_none() {
            return Err("服务器进程启动后立即退出，请查看日志".to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
    if !ready {
        return Err(format!(
            "服务器未能在 30 秒内就绪（{probe}），请查看日志"
        ));
    }

    {
        let mut slot = shared.url.lock().unwrap();
        if slot.is_none() {
            *slot = Some(probe.clone());
        }
    }

    Ok(ServerStatus {
        phase: "running".to_string(),
        url: Some(probe),
        pid: Some(pid),
    })
}

#[tauri::command]
pub async fn start_server(app: AppHandle, st: State<'_, AppState>) -> Result<ServerStatus, String> {
    let shared = Shared::from_state(&st);
    tauri::async_runtime::spawn_blocking(move || start_server_impl(&app, &shared))
        .await
        .map_err(|e| e.to_string())?
}

pub fn stop_server_inner(app: &AppHandle, shared: &Shared) -> Result<ServerStatus, String> {
    shared.stop.store(true, Ordering::SeqCst);
    let managed_pid = *shared.pid.lock().unwrap();
    let target_pid = if let Some(pid) = managed_pid {
        Some(pid)
    } else {
        // 外部启动的服务器：按端口定位进程
        let port = shared.settings.lock().unwrap().port;
        state::find_pid_on_port(port)
    };

    if let Some(pid) = target_pid {
        state::push_log(&shared.logs, &format!("[系统] 正在停止服务器（pid {pid}）"));
        #[cfg(windows)]
        let _ = state::run_output("taskkill", &["/PID", &pid.to_string(), "/T", "/F"]);
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .spawn();
        }
    } else {
        state::push_log(&shared.logs, "[系统] 未找到正在监听该端口的进程");
    }

    *shared.pid.lock().unwrap() = None;
    *shared.url.lock().unwrap() = None;
    state::refresh_tray(app, false);
    Ok(ServerStatus {
        phase: "stopped".to_string(),
        url: None,
        pid: None,
    })
}

#[tauri::command]
pub async fn stop_server(app: AppHandle, st: State<'_, AppState>) -> Result<ServerStatus, String> {
    let shared = Shared::from_state(&st);
    tauri::async_runtime::spawn_blocking(move || stop_server_inner(&app, &shared))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn restart_server(app: AppHandle, st: State<'_, AppState>) -> Result<ServerStatus, String> {
    let shared = Shared::from_state(&st);
    tauri::async_runtime::spawn_blocking(move || {
        let _ = stop_server_inner(&app, &shared);
        start_server_impl(&app, &shared)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn server_status(st: State<'_, AppState>) -> Result<ServerStatus, String> {
    let shared = Shared::from_state(&st);
    let port = shared.settings.lock().unwrap().port;
    tauri::async_runtime::spawn_blocking(move || state::status_from(&shared.pid, &shared.url, port))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn update_tray_state(app: AppHandle, running: bool) {
    state::refresh_tray(&app, running);
}

#[tauri::command]
pub fn get_logs(st: State<AppState>) -> Vec<String> {
    st.logs.lock().unwrap().iter().cloned().collect()
}

#[tauri::command]
pub fn get_settings(st: State<AppState>) -> Settings {
    st.settings.lock().unwrap().clone()
}

#[tauri::command]
pub fn save_settings(
    app: AppHandle,
    st: State<AppState>,
    value: Settings,
) -> Result<Settings, String> {
    {
        let mut s = st.settings.lock().unwrap();
        *s = value;
    }
    let s = st.settings.lock().unwrap().clone();
    state::save_settings(&app, &s)?;
    Ok(s)
}

#[tauri::command]
pub fn set_autostart(
    app: AppHandle,
    st: State<AppState>,
    enabled: bool,
) -> Result<bool, String> {
    state::set_autostart(enabled)?;
    st.settings.lock().unwrap().autostart = enabled;
    let s = st.settings.lock().unwrap().clone();
    state::save_settings(&app, &s)?;
    Ok(enabled)
}

#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    if url.is_empty() {
        return Err("没有可打开的地址".to_string());
    }
    #[cfg(windows)]
    std::process::Command::new("explorer")
        .arg(&url)
        .spawn()
        .map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    std::process::Command::new("open")
        .arg(&url)
        .spawn()
        .map_err(|e| e.to_string())?;
    #[cfg(all(not(windows), not(target_os = "macos")))]
    std::process::Command::new("xdg-open")
        .arg(&url)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 净化下载文件名：剔除路径分隔符、Windows 非法字符与控制字符，空名回退。
pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
        .collect();
    let cleaned = cleaned.trim_matches('.');
    if cleaned.is_empty() || cleaned == ".." {
        "session-log.zip".to_string()
    } else {
        cleaned.to_string()
    }
}

/// 目标路径去重：同名文件存在时递增为 `name (1).ext`，不覆盖既有文件。
pub fn unique_target(dir: &Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("download");
    let ext = Path::new(filename)
        .extension()
        .and_then(|s| s.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    for n in 1..=9999 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(filename)
}

/// 下载 URL 白名单：仅接受本机 DSH 服务器（回环地址）的 `/api/session.export`。
/// `base` 为记录的服务器地址；外部复用的服务器不会留下记录，此时回退到
/// 配置端口上的两种回环写法，避免合法导出被拒。
pub fn is_allowed_download_url(url: &str, base: Option<&str>, port: u16) -> bool {
    let matches = |candidate: &str| {
        let candidate = candidate.trim_end_matches('/');
        url.starts_with(candidate) && url[candidate.len()..].starts_with("/api/session.export")
    };
    if let Some(base) = base.filter(|b| {
        b.starts_with("http://127.0.0.1:") || b.starts_with("http://localhost:")
    }) {
        if matches(base) {
            return true;
        }
    }
    matches(&format!("http://127.0.0.1:{port}")) || matches(&format!("http://localhost:{port}"))
}

/// 把 DSH 会话日志导出下载到配置目录（设置里的下载目录，留空回退系统下载目录）。
/// 返回最终保存路径；前端据此弹提示并可「打开所在文件夹」。
#[tauri::command]
pub async fn whalito_download(
    st: State<'_, AppState>,
    url: String,
    filename: String,
) -> Result<String, String> {
    let settings = st.settings.lock().unwrap().clone();
    let base = st.server_url.lock().unwrap().clone();
    if !is_allowed_download_url(&url, base.as_deref(), settings.port) {
        return Err("仅允许下载本机 DSH 服务器导出的会话日志".to_string());
    }
    let dir = state::resolve_downloads_dir(&settings)?;
    let filename = sanitize_filename(&filename);

    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        // 同目录临时文件，下载完成后原子改名到唯一目标名。
        let temp = dir.join(format!(".dsh-download-{filename}.part"));
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(600))
            .build();
        let resp = agent
            .get(&url)
            .set("User-Agent", "whalito-download")
            .call()
            .map_err(|e| format!("下载会话日志失败：{e}"))?;
        let mut reader = resp.into_reader();
        let mut file = std::fs::File::create(&temp).map_err(|e| format!("创建临时文件失败：{e}"))?;
        if let Err(e) = std::io::copy(&mut reader, &mut file) {
            drop(file);
            let _ = std::fs::remove_file(&temp);
            return Err(format!("写入失败：{e}"));
        }
        drop(file);
        let target = unique_target(&dir, &filename);
        std::fs::rename(&temp, &target).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            format!("保存到 {} 失败：{e}", dir.display())
        })?;
        Ok(target.display().to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 在系统文件管理器里定位文件（Windows：资源管理器选中；macOS：访达揭示）。
#[tauri::command]
pub fn reveal_in_folder(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    if !p.is_file() {
        return Err("文件不存在".to_string());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer")
            .raw_arg(format!("/select,{}", p.display()))
            .spawn()
            .map_err(|e| format!("打开资源管理器失败：{e}"))?;
    }
    #[cfg(target_os = "macos")]
    std::process::Command::new("open")
        .args(["-R", &path])
        .spawn()
        .map_err(|e| format!("打开访达失败：{e}"))?;
    #[cfg(all(not(windows), not(target_os = "macos")))]
    return Err("当前平台暂不支持定位文件".to_string());
    Ok(())
}

/// 写入系统剪贴板（内嵌 DSH 页面右键菜单「复制 / 剪切」由父窗口代写）。
#[tauri::command]
pub fn clipboard_write(text: String) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("无法打开剪贴板：{e}"))?;
    cb.set_text(text).map_err(|e| format!("写入剪贴板失败：{e}"))
}

/// 读取系统剪贴板（内嵌 DSH 页面右键菜单「粘贴」由父窗口代读）。
#[tauri::command]
pub fn clipboard_read() -> Result<String, String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("无法打开剪贴板：{e}"))?;
    cb.get_text().map_err(|e| format!("读取剪贴板失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_roundtrip() {
        // 直接验证 arboard 在本机可用（复制/粘贴链路的关键依赖）。
        // 测试会写剪贴板：先保存原内容，结束后恢复，不污染用户剪贴板。
        let mut cb = arboard::Clipboard::new().expect("clipboard should be openable");
        let prev = cb.get_text().ok();
        let result = (|| -> Result<(), String> {
            cb.set_text("whalito-clipboard-roundtrip")
                .map_err(|e| format!("write failed: {e}"))?;
            let got = cb.get_text().map_err(|e| format!("read failed: {e}"))?;
            assert_eq!(got, "whalito-clipboard-roundtrip");
            Ok(())
        })();
        if let Some(p) = prev {
            let _ = cb.set_text(p);
        }
        result.expect("roundtrip should succeed");
    }

    #[test]
    fn dsh_update_spec_uses_selected_channel() {
        assert_eq!(dsh_update_spec("latest"), "@deepseek-ai/dsh@latest");
        assert_eq!(dsh_update_spec("next"), "@deepseek-ai/dsh@next");
    }

    #[test]
    fn sanitize_filename_strips_hostile_chars() {
        assert_eq!(sanitize_filename("dsh-session-a1_b-2.zip"), "dsh-session-a1_b-2.zip");
        assert_eq!(sanitize_filename("..\\..\\evil.zip"), "evil.zip");
        assert_eq!(sanitize_filename("a/b:c*d?.zip"), "abcd.zip");
        assert_eq!(sanitize_filename("   "), "session-log.zip");
        assert_eq!(sanitize_filename("..."), "session-log.zip");
    }

    #[test]
    fn unique_target_appends_counter_without_overwriting() {
        let dir = std::env::temp_dir().join(format!("whalito-dl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log.zip"), b"first").unwrap();
        let second = unique_target(&dir, "log.zip");
        assert_eq!(second.file_name().unwrap().to_str(), Some("log (1).zip"));
        assert!(!second.exists());
        std::fs::write(&second, b"second").unwrap();
        let third = unique_target(&dir, "log.zip");
        assert_eq!(third.file_name().unwrap().to_str(), Some("log (2).zip"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_url_allowlist_only_accepts_loopback_export() {
        let base = Some("http://127.0.0.1:3080");
        assert!(is_allowed_download_url(
            "http://127.0.0.1:3080/api/session.export?sessionId=a&includeDescendants=true",
            base,
            3080,
        ));
        assert!(is_allowed_download_url("http://localhost:3080/api/session.export?sessionId=a", Some("http://localhost:3080"), 3080));
        assert!(!is_allowed_download_url("http://127.0.0.1:3080/api/other", base, 3080));
        assert!(!is_allowed_download_url("http://evil.example/api/session.export", base, 3080));
        assert!(!is_allowed_download_url("https://127.0.0.1:3080/api/session.export", base, 3080));
        assert!(!is_allowed_download_url("http://127.0.0.1:3081/api/session.export", base, 3080));
    }

    #[test]
    fn download_url_allowlist_falls_back_to_configured_port_without_recorded_url() {
        // 外部复用的服务器没有记录地址：按配置端口 + 回环地址放行。
        assert!(is_allowed_download_url(
            "http://127.0.0.1:30080/api/session.export?sessionId=a",
            None,
            30080,
        ));
        assert!(is_allowed_download_url(
            "http://localhost:30080/api/session.export?sessionId=a",
            None,
            30080,
        ));
        assert!(!is_allowed_download_url("http://127.0.0.1:3080/api/session.export", None, 30080));
        assert!(!is_allowed_download_url("http://evil.example/api/session.export", None, 30080));
    }

    #[test]
    fn stash_rollback_restores_old_install() {
        // 安装失败回滚：暂存后旧 DSH 包目录与 shim 都被 rename 走，
        // rollback 把它们恢复到原位（并清掉 npm 可能写入的半成品）。
        let base = std::env::temp_dir().join(format!("whalito-stash-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let prefix = base.join("prefix");
        let dsh_pkg = prefix.join("node_modules").join("@deepseek-ai").join("dsh");
        std::fs::create_dir_all(&dsh_pkg).unwrap();
        std::fs::write(dsh_pkg.join("index.js"), "old dsh").unwrap();
        std::fs::write(prefix.join("dsh.cmd"), "@echo old").unwrap();
        std::fs::write(prefix.join("dsh.ps1"), "# old").unwrap();
        let stash_dir = prefix.join(".dsh-reinstall-test");

        let stashed = stash_dsh_files(&dsh_pkg, &stash_dir, &prefix);
        assert_eq!(stashed.len(), 3, "包目录 + 两个 shim 都应暂存");
        assert!(!dsh_pkg.exists(), "暂存后原位置不应有旧包");
        assert!(!prefix.join("dsh.cmd").exists());
        // 模拟 npm 写入了半成品。
        std::fs::create_dir_all(&dsh_pkg).unwrap();
        std::fs::write(dsh_pkg.join("index.js"), "half written").unwrap();
        std::fs::write(prefix.join("dsh.cmd"), "@echo half").unwrap();

        rollback_stash(&stash_dir, &stashed);
        assert_eq!(std::fs::read_to_string(dsh_pkg.join("index.js")).unwrap(), "old dsh");
        assert_eq!(std::fs::read_to_string(prefix.join("dsh.cmd")).unwrap(), "@echo old");
        assert!(prefix.join("dsh.ps1").exists());
        assert!(!stash_dir.exists(), "回滚后暂存目录应删除");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn stash_commit_discards_old_install() {
        // 安装成功：commit 删除暂存，旧安装不恢复。
        let base = std::env::temp_dir().join(format!("whalito-stash2-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let prefix = base.join("prefix");
        let dsh_pkg = prefix.join("node_modules").join("@deepseek-ai").join("dsh");
        std::fs::create_dir_all(&dsh_pkg).unwrap();
        std::fs::write(dsh_pkg.join("index.js"), "old").unwrap();
        let stash_dir = prefix.join(".dsh-reinstall-test");

        let stashed = stash_dsh_files(&dsh_pkg, &stash_dir, &prefix);
        assert_eq!(stashed.len(), 1);
        assert!(stash_dir.join("dsh-pkg").join("index.js").exists());
        commit_stash(&stash_dir);
        assert!(!stash_dir.exists());
        assert!(!dsh_pkg.exists(), "成功安装场景旧包不恢复（npm 已写新包）");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn stash_without_old_install_is_empty() {
        // 首次安装（无旧 DSH）：无暂存项，后续 commit/rollback 均为空操作。
        let base = std::env::temp_dir().join(format!("whalito-stash3-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let prefix = base.join("prefix");
        let dsh_pkg = prefix.join("node_modules").join("@deepseek-ai").join("dsh");
        let stash_dir = prefix.join(".dsh-reinstall-test");
        let stashed = stash_dsh_files(&dsh_pkg, &stash_dir, &prefix);
        assert!(stashed.is_empty());
        commit_stash(&stash_dir);
        rollback_stash(&stash_dir, &stashed);
        assert!(!stash_dir.exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
