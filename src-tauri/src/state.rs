use std::{
    collections::{HashSet, VecDeque},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tauri::menu::MenuItem;
use tauri::tray::TrayIcon;
use tauri::{AppHandle, Emitter, Manager};

pub const LOG_CAP: usize = 2000;

/// Node.js 最低可用版本（含）：< 22.19.0 视为不可用。
pub const MIN_NODE_VERSION: (u64, u64, u64) = (22, 19, 0);

/// 测试构建开关（编译期）：build.rs 在 WHALITO_TEST_BUILD=1 时发出
/// cfg(whalito_test)；生产构建无此 cfg，测试分支被编译器直接剔除（零残留）。
pub const TEST_BUILD: bool = cfg!(whalito_test);

/// DSH 服务器默认端口：生产 3080，测试构建 30080。
pub const DEFAULT_PORT: u16 = if TEST_BUILD { 30080 } else { 3080 };

#[derive(Default)]
pub struct AppState {
    pub pid: Arc<Mutex<Option<u32>>>,
    pub stop_requested: Arc<AtomicBool>,
    pub server_url: Arc<Mutex<Option<String>>>,
    pub logs: Arc<Mutex<VecDeque<String>>>,
    pub settings: Arc<Mutex<Settings>>,
    pub quitting: Arc<AtomicBool>,
    pub tray: Mutex<Option<TrayIcon>>,
    pub tray_start: Mutex<Option<MenuItem<tauri::Wry>>>,
    pub tray_stop: Mutex<Option<MenuItem<tauri::Wry>>>,
    /// 桌宠读取器：true 表示请求停止（应用退出时置位）。
    pub pet_stop: Arc<AtomicBool>,
    /// 最近一次桌宠状态快照（JSON），供 pet_status 命令即时返回。
    pub pet_snapshot: Arc<Mutex<Option<serde_json::Value>>>,
    /// 桌宠待查看通知（任务完成 / 被阻塞 / 被中断 / 失败）；打开主界面后清除。
    pub pet_notice: Arc<Mutex<Option<crate::pet::PetNotice>>>,
    /// 流侧（turn/end error）已通知失败的会话 id：轮询据此不重复发「已完成」。
    pub pet_failed_sessions: Arc<Mutex<HashSet<String>>>,
    /// 主窗口是否处于聚焦状态：聚焦视为用户正在查看，抑制桌宠"任务完成"通知。
    pub main_open: Arc<AtomicBool>,
    /// 已装 DSH 的 `web` 命令是否支持 `--no-open`（启动服务器时抑制自动开浏览器）。
    /// `None` = 尚未探测；进程级缓存，`install_dsh`/`update_dsh` 成功后置回 None 重探。
    pub no_open_supported: Arc<Mutex<Option<bool>>>,
    /// 安装/更新互斥锁：`install_dsh`/`update_dsh` 在阻塞线程全程持有；
    /// `start_server` 尝试获取（try_lock），失败即返回「正在安装/更新，请稍候」，
    /// 杜绝安装中途启动服务器导致的文件竞争/半装。
    pub install_lock: Arc<Mutex<()>>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub port: u16,
    pub registry: String,
    /// DSH 版本偏好（npm 发布标签）："latest"（稳定版，默认）/ "next"（预发布版）。
    /// 只影响「检查更新」与「更新到新版本」；首次安装固定稳定版。
    #[serde(default = "default_dsh_channel")]
    pub dsh_channel: String,
    pub autostart: bool,
    pub auto_restart: bool,
    pub workspace_dir: Option<String>,
    /// 用户自定义的 Node.js 安装目录（便携版），检测时优先使用该目录下的 node.exe。
    pub node_dir: Option<String>,
    /// 是否显示桌宠（托盘可切换）。
    #[serde(default = "default_pet_enabled")]
    pub pet_enabled: bool,
    /// DSH 会话导出等下载的保存目录；留空回退系统下载目录。
    pub download_dir: Option<String>,
}

fn default_pet_enabled() -> bool {
    true
}

fn default_dsh_channel() -> String {
    "latest".to_string()
}

/// DSH 版本偏好归一化：仅 "next" 视为预发布通道，其余（含空串/脏值）一律 latest。
pub fn normalize_dsh_channel(v: &str) -> &'static str {
    if v.trim() == "next" {
        "next"
    } else {
        "latest"
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            registry: "https://registry.npmjs.org".to_string(),
            dsh_channel: "latest".to_string(),
            autostart: false,
            auto_restart: true,
            workspace_dir: None,
            node_dir: None,
            pet_enabled: true,
            download_dir: None,
        }
    }
}

/// 解析下载目录：设置里的自定义目录（非空）优先，否则系统下载目录，
/// 再否则用户主目录。目录会按需创建，创建失败时返回错误。
pub fn resolve_downloads_dir(settings: &Settings) -> Result<std::path::PathBuf, String> {
    let dir = settings
        .download_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(dirs::download_dir)
        .or_else(dirs::home_dir)
        .ok_or_else(|| "无法确定下载目录".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建下载目录 {}：{e}", dir.display()))?;
    Ok(dir)
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EnvInfo {
    pub found: bool,
    pub version: Option<String>,
    pub node_path: Option<String>,
    pub npm_prefix: Option<String>,
    pub install_prefix: Option<String>,
    pub dsh_installed: bool,
    pub dsh_version: Option<String>,
    pub dsh_bin: Option<String>,
    /// 是否已安装且版本 >= MIN_NODE_VERSION。
    pub node_version_ok: bool,
    /// 是否已安装但版本不可用（缺失 / 解析失败 / 低于最低版本）。
    pub node_too_old: bool,
    pub nvm_found: bool,
    pub nvm_path: Option<String>,
    /// 当前 Node 不可用（缺失 / 版本过低）时，nvm 中已安装、可直接切换到的
    /// 最高合格版本（如 "22.19.0"）。有值时前端应优先提示「切换版本」而非安装。
    pub nvm_switch_version: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub phase: String,
    pub url: Option<String>,
    pub pid: Option<u32>,
}

impl Default for EnvInfo {
    fn default() -> Self {
        Self {
            found: false,
            version: None,
            node_path: None,
            npm_prefix: None,
            install_prefix: None,
            dsh_installed: false,
            dsh_version: None,
            dsh_bin: None,
            node_version_ok: false,
            node_too_old: false,
            nvm_found: false,
            nvm_path: None,
            nvm_switch_version: None,
        }
    }
}

impl Default for ServerStatus {
    fn default() -> Self {
        Self {
            phase: "stopped".to_string(),
            url: None,
            pid: None,
        }
    }
}

/// 解析 `v22.19.0` / `22.19.0` 这类版本号；非法输入返回 None。
pub fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let s = v.trim().trim_start_matches('v');
    let mut parts = s.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = parts
        .next()
        .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
        .filter(|p| !p.is_empty())
        .and_then(|p| p.parse::<u64>().ok())
        .unwrap_or(0);
    Some((major, minor, patch))
}

pub fn run_output(program: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // macOS：GUI 进程 PATH 极简，子进程（npm 生命周期脚本、git 等）找不到工具
    if let Some(p) = child_path() {
        cmd.env("PATH", p);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| format!("无法运行 {program}: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = if stdout.is_empty() {
            stderr
        } else if stderr.is_empty() {
            stdout
        } else {
            format!("{stdout}\n{stderr}")
        };
        Err(if msg.is_empty() {
            format!("{program} 退出码 {}", out.status.code().unwrap_or(-1))
        } else {
            msg
        })
    }
}

pub fn run_streaming(app: &AppHandle, program: &str, args: &[&str]) -> Result<String, String> {
    run_streaming_with_path(app, program, args, None)
}

/// 流式执行，可额外把某个目录置于子进程 PATH 最前。
/// macOS 安装 npm 包时用 node 所在目录做前缀：koffi 等原生依赖的
/// 生命周期脚本执行 `node ./cnoke.cjs`，即使 shell PATH 捕获失败 /
/// npm flag 未生效，也一定能找到 node。
pub fn run_streaming_with_path(
    app: &AppHandle,
    program: &str,
    args: &[&str],
    prepend_path: Option<&str>,
) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = prepend_path {
        parts.push(p.to_string());
    }
    if let Some(p) = child_path() {
        parts.push(p);
    }
    if !parts.is_empty() {
        cmd.env("PATH", parts.join(":"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("无法启动 {program}: {e}"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let collected = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut handles = Vec::new();
    let mut streams: Vec<Box<dyn std::io::Read + Send>> = Vec::new();
    if let Some(s) = stdout {
        streams.push(Box::new(s));
    }
    if let Some(s) = stderr {
        streams.push(Box::new(s));
    }
    for stream in streams {
        let app = app.clone();
        let collected = Arc::clone(&collected);
        handles.push(std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stream).lines() {
                match line {
                    Ok(line) => {
                        collected.lock().unwrap().push(line.clone());
                        let _ = app.emit("log", &line);
                    }
                    Err(_) => break,
                }
            }
        }));
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    for h in handles {
        let _ = h.join();
    }
    let combined = collected.lock().unwrap().join("\n");
    if status.success() {
        Ok(combined)
    } else {
        Err(format!(
            "{program} 退出码 {}:\n{combined}",
            status.code().unwrap_or(-1)
        ))
    }
}

pub fn push_log(logs: &Mutex<VecDeque<String>>, line: &str) {
    let mut q = logs.lock().unwrap();
    q.push_back(line.to_string());
    while q.len() > LOG_CAP {
        q.pop_front();
    }
}

pub fn extract_url(line: &str) -> Option<String> {
    let idx = line.find("http")?;
    let rest = &line[idx..];
    let url: String = rest
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'' && *c != ',')
        .collect();
    let url = url.trim_end_matches('.').trim_end_matches(')').to_string();
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(url)
    } else {
        None
    }
}

pub fn health(url: &str) -> bool {
    ureq::get(url)
        .timeout(std::time::Duration::from_millis(800))
        .call()
        .is_ok()
}

pub fn status_from(
    pid: &Mutex<Option<u32>>,
    url: &Mutex<Option<String>>,
    port: u16,
) -> ServerStatus {
    let pid_val = *pid.lock().unwrap();
    let url_val = url.lock().unwrap().clone();
    let (phase, url_val) = match (pid_val, url_val) {
        (Some(_), None) => ("starting".to_string(), None),
        (Some(_), Some(u)) => {
            if health(&u) {
                ("running".to_string(), Some(u))
            } else {
                ("error".to_string(), Some(u))
            }
        }
        (None, _) => {
            // 应用未启动服务器：探测配置端口上是否已有外部运行的实例
            let probe = format!("http://127.0.0.1:{port}");
            if health(&probe) {
                ("external".to_string(), Some(probe))
            } else {
                ("stopped".to_string(), None)
            }
        }
    };
    ServerStatus {
        phase,
        url: url_val,
        pid: pid_val,
    }
}

#[cfg(windows)]
pub fn find_pid_on_port(port: u16) -> Option<u32> {
    let out = run_output("netstat", &["-ano", "-p", "tcp"]).ok()?;
    let suffix = format!(":{port}");
    for line in out.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 5
            && parts[0].eq_ignore_ascii_case("tcp")
            && parts[3].eq_ignore_ascii_case("listening")
            && parts[1].ends_with(&suffix)
        {
            if let Ok(pid) = parts[4].parse::<u32>() {
                return Some(pid);
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
pub fn find_pid_on_port(port: u16) -> Option<u32> {
    let port_arg = format!("-iTCP:{port}");
    let out = run_output("lsof", &["-nP", &port_arg, "-sTCP:LISTEN", "-t"]).ok()?;
    out.lines().next()?.trim().parse::<u32>().ok()
}

#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn find_pid_on_port(_port: u16) -> Option<u32> {
    None
}

/// 收集 node 候选路径（按平台与优先级排序，供 detect_env 逐个探测）。
pub fn node_candidates(node_dir: Option<&str>) -> Vec<String> {
    #[cfg(windows)]
    {
        windows_node_candidates(node_dir)
    }
    #[cfg(target_os = "macos")]
    {
        macos_node_candidates(node_dir)
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        unix_node_candidates(node_dir)
    }
}

/// Windows：用户目录 → where.exe → Program Files 兜底。
#[cfg(windows)]
fn windows_node_candidates(node_dir: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(dir) = node_dir {
        let p = Path::new(dir).join("node.exe");
        if p.exists() {
            out.push(p.to_string_lossy().to_string());
        }
    }
    if let Ok(o) = run_output("where.exe", &["node"]) {
        if let Some(first) = o
            .lines()
            .next()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            out.push(first);
        }
    }
    for p in [
        "C:\\Program Files\\nodejs\\node.exe",
        "C:\\Program Files (x86)\\nodejs\\node.exe",
    ] {
        if Path::new(p).exists() {
            out.push(p.to_string());
        }
    }
    out
}

/// macOS：GUI 应用从 Finder/Dock 启动时 PATH 极简（/usr/bin:/bin:/usr/sbin:/sbin），
/// 因此必须靠绝对路径前缀链探测，PATH 只作最后兜底。
#[cfg(target_os = "macos")]
fn macos_node_candidates(node_dir: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    // 1. 用户自定义 / 便携 / 鲸仔 tar 包安装目录（安装后回写，重启后依然可靠）
    if let Some(dir) = node_dir {
        let p = Path::new(dir).join("node");
        if p.exists() {
            out.push(p.to_string_lossy().to_string());
        }
    }
    // 2. nvm：current 软链优先，然后 versions 目录按版本降序
    let nvm_base = std::env::var("NVM_DIR")
        .map(PathBuf::from)
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".nvm")));
    if let Some(base) = nvm_base {
        let cur = base.join("current").join("bin").join("node");
        if cur.exists() {
            out.push(cur.to_string_lossy().to_string());
        }
        let versions = base.join("versions").join("node");
        if let Ok(rd) = std::fs::read_dir(&versions) {
            let mut found: Vec<(String, (u64, u64, u64))> = rd
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let ver = parse_semver(&name)?;
                    Some((name, ver))
                })
                .collect();
            found.sort_by(|a, b| b.1.cmp(&a.1));
            for (name, _) in found {
                let p = versions.join(&name).join("bin").join("node");
                if p.exists() {
                    out.push(p.to_string_lossy().to_string());
                }
            }
        }
    }
    // 3. fnm / volta（尽力检测）
    if let Ok(home) = std::env::var("HOME") {
        let h = PathBuf::from(&home);
        for base in [
            h.join(".local").join("share").join("fnm").join("node-versions"),
            h.join("Library")
                .join("Application Support")
                .join("fnm")
                .join("node-versions"),
        ] {
            if let Ok(rd) = std::fs::read_dir(&base) {
                for e in rd.flatten() {
                    let p = e.path().join("installation").join("bin").join("node");
                    if p.exists() {
                        out.push(p.to_string_lossy().to_string());
                    }
                }
            }
        }
        let volta = h.join(".volta").join("bin").join("node");
        if volta.exists() {
            out.push(volta.to_string_lossy().to_string());
        }
    }
    // 4. Homebrew 前缀（Apple Silicon / Intel）
    for p in ["/opt/homebrew/bin/node", "/usr/local/bin/node"] {
        if Path::new(p).exists() {
            out.push(p.to_string());
        }
    }
    // 5. Xcode 命令行工具自带 node（通常过旧，由 pick_best_node 的版本判断降级）
    if Path::new("/usr/bin/node").exists() {
        out.push("/usr/bin/node".to_string());
    }
    // 6. PATH 兜底
    if let Some(p) = find_in_path("node") {
        out.push(p);
    }
    out
}

/// Linux：用户目录 → PATH。
#[cfg(all(not(windows), not(target_os = "macos")))]
fn unix_node_candidates(node_dir: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(dir) = node_dir {
        let p = Path::new(dir).join("node");
        if p.exists() {
            out.push(p.to_string_lossy().to_string());
        }
    }
    if let Some(p) = find_in_path("node") {
        out.push(p);
    }
    out
}

/// 在 PATH 中查找可执行文件（校验可执行位；仅 unix 使用）。
#[cfg(not(windows))]
pub fn find_in_path(exe: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(exe);
        let executable = std::fs::metadata(&candidate)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        if executable {
            return Some(candidate.to_string_lossy().to_string());
        }
    }
    None
}

/// 从 (路径, 版本) 候选中选择最优节点：按传入顺序，取第一个版本满足最低要求的；
/// 否则第一个能跑出任意版本的；再否则第一个候选。
pub fn pick_best_node(cands: &[(String, Option<String>)]) -> Option<String> {
    let mut any_version: Option<&str> = None;
    let mut first: Option<&str> = None;
    for (p, v) in cands {
        if first.is_none() {
            first = Some(p.as_str());
        }
        let Some(vs) = v else { continue };
        if any_version.is_none() {
            any_version = Some(p.as_str());
        }
        if parse_semver(vs).is_some_and(|t| t >= MIN_NODE_VERSION) {
            return Some(p.clone());
        }
    }
    any_version.or(first).map(|s| s.to_string())
}

pub fn npm_cli(node_path: &str) -> Option<PathBuf> {
    let dir = Path::new(node_path).parent()?;
    // 布局一：与 node 同级的 node_modules/npm（Windows 安装器 / 便携 zip）
    let portable = dir.join("node_modules").join("npm").join("bin").join("npm-cli.js");
    if portable.exists() {
        return Some(portable);
    }
    // 布局二：符号链接解析后的真实路径，沿祖先目录逐级向上探测两种挂载点：
    //   <ancestor>/node_modules/npm（nvm-windows 的 versions/vX/node_modules）
    //   <ancestor>/lib/node_modules/npm（POSIX 全局模块目录）
    // 各级安装方式的离地高度（从真实 node 所在目录向上）：
    //   - nvm / fnm / volta / 便携 tar 包：1 级（<prefix>/lib/node_modules/npm）
    //   - macOS 官方 pkg：2 级（node 在 lib/node_modules/node/bin，
    //     npm 在 lib/node_modules/npm）
    //   - Homebrew（Cellar）：4 级（node 在 Cellar/node/<ver>/bin，
    //     npm 挂在 prefix 层 /opt/homebrew|/usr/local/lib/node_modules/npm）
    // 取 6 级覆盖全部并留余量；每级只做两次 exists() 探测，开销可忽略。
    let real = std::fs::canonicalize(node_path).ok()?;
    let mut ancestor = real.parent()?.to_path_buf();
    for _ in 0..6 {
        for candidate in [
            ancestor
                .join("node_modules")
                .join("npm")
                .join("bin")
                .join("npm-cli.js"),
            ancestor
                .join("lib")
                .join("node_modules")
                .join("npm")
                .join("bin")
                .join("npm-cli.js"),
        ] {
            if candidate.exists() {
                return Some(candidate);
            }
        }
        if !ancestor.pop() {
            break;
        }
    }
    None
}

pub fn dsh_bin(npm_prefix: &str) -> Option<PathBuf> {
    // npm 全局安装布局因平台而异：Windows 是 <prefix>/node_modules，
    // POSIX（macOS/Linux）是 <prefix>/lib/node_modules——两种都探测。
    let root = Path::new(npm_prefix);
    for candidate in [
        root.join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib")
            .join("bin.js"),
        root.join("lib")
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib")
            .join("bin.js"),
    ] {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// 应用专用 npm 安装前缀（用户目录内，隔离且免管理员权限）。
pub fn app_prefix_dir() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var("LOCALAPPDATA")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string());
        Path::new(&base).join("dsh-launcher").join("npm")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        Path::new(&home)
            .join("Library")
            .join("Application Support")
            .join("com.deepseek.dsh-launcher")
            .join("npm")
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        Path::new(&home).join(".local").join("share").join("dsh-launcher").join("npm")
    }
}

pub fn resolve_dsh_bin(env: &EnvInfo) -> Option<PathBuf> {
    env.dsh_bin.as_ref().map(PathBuf::from)
}

/// 探测 nvm：Windows 找 nvm-windows 可执行文件；macOS 找 nvm.sh
/// （nvm 是 shell 函数，`which` 找不到，必须按目录探测）。
pub fn detect_nvm() -> Option<String> {
    #[cfg(windows)]
    {
        if let Ok(out) = run_output("where.exe", &["nvm"]) {
            if let Some(first) = out.lines().next() {
                let s = first.trim();
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
        if let Ok(home) = std::env::var("NVM_HOME") {
            let p = Path::new(&home).join("nvm.exe");
            if p.exists() {
                return Some(p.to_string_lossy().to_string());
            }
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            let p = Path::new(&appdata).join("nvm").join("nvm.exe");
            if p.exists() {
                return Some(p.to_string_lossy().to_string());
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(dir) = std::env::var("NVM_DIR") {
            let p = Path::new(&dir).join("nvm.sh");
            if p.exists() {
                return Some(p.to_string_lossy().to_string());
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            let p = Path::new(&home).join(".nvm").join("nvm.sh");
            if p.exists() {
                return Some(p.to_string_lossy().to_string());
            }
        }
        None
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        None
    }
}

/// 从 nvm 版本目录（目录名形如 `v22.19.0`，忽略其他文件/目录）解析已安装版本，
/// 按版本降序返回。
fn nvm_versions_in_dir(base: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(base) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if parse_semver(&name).is_some() {
                out.push(name);
            }
        }
    }
    out.sort_by(|a, b| parse_semver(b).cmp(&parse_semver(a)));
    out
}

/// 扫描 nvm 已安装的 Node 版本（按目录名 `v<semver>` 识别），按版本降序返回。
/// Windows：nvm-windows 版本目录 `<NVM_HOME>\v<version>`；
/// macOS：nvm 版本目录 `~/.nvm/versions/node/v<version>`。
pub fn nvm_installed_versions() -> Vec<String> {
    #[cfg(windows)]
    {
        let base = std::env::var("NVM_HOME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("APPDATA")
                    .ok()
                    .map(|a| Path::new(&a).join("nvm"))
            });
        base.as_deref().map(nvm_versions_in_dir).unwrap_or_default()
    }
    #[cfg(target_os = "macos")]
    {
        let base = std::env::var("NVM_DIR")
            .map(PathBuf::from)
            .ok()
            .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".nvm")));
        base.map(|b| nvm_versions_in_dir(&b.join("versions").join("node")))
            .unwrap_or_default()
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        Vec::new()
    }
}

/// nvm 中已安装且满足最低版本要求的最高版本（无需下载，直接 `nvm use` 即可）。
pub fn nvm_switch_version() -> Option<String> {
    nvm_installed_versions()
        .into_iter()
        .find(|v| parse_semver(v).is_some_and(|t| t >= MIN_NODE_VERSION))
}

/// 从 nodejs.org 的 index.json 解析最新的 22.x 版本号（如 "22.19.0"）。失败返回 None。
pub fn latest_node_lts_major22() -> Option<String> {
    let resp = ureq::get("https://nodejs.org/dist/index.json")
        .timeout(Duration::from_secs(20))
        .call()
        .ok()?;
    let mut reader = resp.into_reader();
    let mut body = String::new();
    reader.read_to_string(&mut body).ok()?;
    let arr: serde_json::Value = serde_json::from_str(&body).ok()?;
    for item in arr.as_array()? {
        let v = item.get("version")?.as_str()?;
        if v.starts_with("v22.") {
            return Some(v.trim_start_matches('v').to_string());
        }
    }
    None
}

/// 依据 npm 镜像源推断 Node 分发下载基地址（国内 npmmirror 自动切换）。
pub fn node_dist_base(registry: &str) -> String {
    if registry.contains("npmmirror") {
        "https://npmmirror.com/mirrors/node".to_string()
    } else {
        "https://nodejs.org/dist".to_string()
    }
}

pub fn download_file(url: &str, dest: &Path) -> Result<(), String> {
    let resp = ureq::get(url)
        .timeout(Duration::from_secs(600))
        .call()
        .map_err(|e| format!("下载失败：{e}"))?;
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| format!("创建文件失败：{e}"))?;
    std::io::copy(&mut reader, &mut file).map_err(|e| format!("写入失败：{e}"))?;
    Ok(())
}

pub fn extract_zip(zip_path: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("打开压缩包失败：{e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("解析压缩包失败：{e}"))?;
    std::fs::create_dir_all(dest).map_err(|e| format!("创建目录失败：{e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("读取压缩包条目失败：{e}"))?;
        // enclosed_name 防止 zip-slip 路径穿越
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).ok();
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let mut o = std::fs::File::create(&out).map_err(|e| format!("创建文件失败：{e}"))?;
            std::io::copy(&mut entry, &mut o).map_err(|e| format!("解压失败：{e}"))?;
        }
    }
    Ok(())
}

pub fn refresh_tray(app: &AppHandle, running: bool) {
    let st = app.state::<AppState>();
    let start_item = st.tray_start.lock().unwrap().clone();
    let stop_item = st.tray_stop.lock().unwrap().clone();
    if let Some(item) = start_item {
        let _ = item.set_enabled(!running);
        let _ = item.set_text(if running { "服务器运行中" } else { "启动服务器" });
    }
    if let Some(item) = stop_item {
        let _ = item.set_enabled(running);
    }
}

/// 检测环境。`node_dir` 为用户自定义的 Node 目录（优先于 PATH / 系统安装）。
pub fn detect_env(node_dir: Option<&str>) -> EnvInfo {
    // 预热：首次检测时捕获一次用户 shell PATH（macOS 约 0.3–0.5s，之后走缓存）。
    // 之后 npm 安装（koffi 等生命周期脚本需要 node）与 dsh 子进程才能找到工具。
    #[cfg(target_os = "macos")]
    {
        let _ = shell_path();
    }

    let version_on_path = run_output("node", &["--version"]).ok();

    let candidates = node_candidates(node_dir);
    let probed: Vec<(String, Option<String>)> = candidates
        .iter()
        .map(|p| (p.clone(), run_output(p, &["--version"]).ok()))
        .collect();
    let node_path = pick_best_node(&probed);

    let node = node_path.clone().unwrap_or_else(|| "node".to_string());

    let version = node_path
        .as_ref()
        .and_then(|p| {
            probed
                .iter()
                .find(|(c, _)| c == p)
                .and_then(|(_, v)| v.clone())
        })
        .or(version_on_path);

    let npm_prefix = npm_cli(&node)
        .and_then(|cli| run_output(&node, &[cli.to_str().unwrap_or(""), "prefix", "-g"]).ok())
        .filter(|s| !s.is_empty());

    // 应用专用安装目录（始终使用，隔离且免管理员权限）
    let install_prefix = {
        let dir = app_prefix_dir();
        let _ = std::fs::create_dir_all(&dir);
        Some(dir.to_string_lossy().to_string())
    };

    // 依次尝试：应用目录 → 全局前缀；以 `dsh --version` 能否成功为准（排除残缺安装）
    let mut dsh_bin_path: Option<PathBuf> = None;
    let mut dsh_version: Option<String> = None;
    for prefix in [install_prefix.as_deref(), npm_prefix.as_deref()]
        .into_iter()
        .flatten()
    {
        if let Some(bin) = dsh_bin(prefix) {
            if let Ok(v) = run_output(&node, &[bin.to_str().unwrap_or(""), "--version"]) {
                dsh_bin_path = Some(bin);
                dsh_version = Some(v);
                break;
            }
        }
    }

    let found = node_path.is_some() && version.is_some();
    let version_tuple = version.as_deref().and_then(parse_semver);
    let node_version_ok = found && version_tuple.is_some_and(|v| v >= MIN_NODE_VERSION);
    let node_too_old = found && !node_version_ok;

    let nvm_path = detect_nvm();

    // 当前 Node 不可用（缺失 / 版本过低）时，探测 nvm 里能否直接切换：
    // 有则前端优先提示「切换到 Node X」而不是让用户重新安装。
    let nvm_switch_version = if !node_version_ok {
        nvm_switch_version()
    } else {
        None
    };

    EnvInfo {
        found,
        version,
        node_path,
        npm_prefix,
        install_prefix,
        dsh_installed: dsh_bin_path.is_some(),
        dsh_version,
        dsh_bin: dsh_bin_path.map(|p| p.to_string_lossy().to_string()),
        node_version_ok,
        node_too_old,
        nvm_found: nvm_path.is_some(),
        nvm_path,
        nvm_switch_version,
    }
}

pub fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("config.json"))
}

pub fn load_settings(app: &AppHandle) -> Settings {
    if let Ok(path) = config_path(app) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(s) = serde_json::from_str::<Settings>(&text) {
                return s;
            }
        }
    }
    Settings::default()
}

pub fn save_settings(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let path = config_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())
}

/// 更新弹窗静默状态：各目标「暂不更新」到的时间戳（epoch 毫秒）。
/// 独立于 Settings 持久化（save_settings 是整体替换、内嵌设置分区的字段集
/// 不含这两项，放 Settings 里会被设置保存清掉）。
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct UpdateSnooze {
    #[serde(default)]
    pub dsh: Option<u64>,
    #[serde(default)]
    pub whalito: Option<u64>,
}

fn update_snooze_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("update_snooze.json"))
}

/// 从指定路径读取（纯函数，便于单测）；文件缺失 / 损坏一律回退「未静默」。
fn read_update_snooze_file(path: &Path) -> UpdateSnooze {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<UpdateSnooze>(&t).ok())
        .unwrap_or_default()
}

/// 原子写（先写临时文件再 rename，避免半写损坏旧数据）；纯函数，便于单测。
fn write_update_snooze_file(path: &Path, s: &UpdateSnooze) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// 读取静默状态（应用配置目录）；文件缺失 / 损坏一律回退「未静默」。
pub fn load_update_snooze(app: &AppHandle) -> UpdateSnooze {
    update_snooze_path(app)
        .map(|p| read_update_snooze_file(&p))
        .unwrap_or_default()
}

/// 原子写静默状态到应用配置目录。
pub fn save_update_snooze(app: &AppHandle, s: &UpdateSnooze) -> Result<(), String> {
    write_update_snooze_file(&update_snooze_path(app)?, s)
}

#[cfg(windows)]
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        .map_err(|e| e.to_string())?;
    if enabled {
        key.set_value(
            "DshLauncher",
            &format!("\"{}\" --autostart", exe.display()),
        )
        .map_err(|e| e.to_string())?;
    } else {
        let _ = key.delete_value("DshLauncher");
    }
    Ok(())
}

/// macOS：写 / 删用户级 LaunchAgents plist（无需管理员权限）。
#[cfg(target_os = "macos")]
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|e| format!("无法确定用户目录：{e}"))?;
    let agents = Path::new(&home).join("Library").join("LaunchAgents");
    let plist = agents.join("com.deepseek.dsh-launcher.plist");
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&agents)
            .map_err(|e| format!("创建 LaunchAgents 目录失败：{e}"))?;
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>com.deepseek.dsh-launcher</string>\n\t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{}</string>\n\t\t<string>--autostart</string>\n\t</array>\n\t<key>RunAtLoad</key>\n\t<true/>\n</dict>\n</plist>\n",
            exe.display()
        );
        std::fs::write(&plist, xml).map_err(|e| format!("写入自启配置失败：{e}"))
    } else {
        let _ = std::fs::remove_file(&plist);
        Ok(())
    }
}

#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn set_autostart(_enabled: bool) -> Result<(), String> {
    Err("当前平台不支持开机自启".to_string())
}

/// macOS：缓存一次用户 shell 的完整 PATH。
#[cfg(target_os = "macos")]
static SHELL_PATH: std::sync::OnceLock<Mutex<Option<String>>> = std::sync::OnceLock::new();

/// 捕获用户 shell 的 PATH（Finder 启动的 GUI 应用环境极简，需主动读取）。
#[cfg(target_os = "macos")]
fn capture_shell_path() -> Option<String> {
    for shell in ["zsh", "bash"] {
        if let Ok(out) = run_output(shell, &["-lc", "printf %s \"$PATH\""]) {
            let s = out.trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// 进程内只捕获一次（首次约 0.3–0.5s，之后即时返回）。
/// 注意：捕获期间【不能持有锁】——capture_shell_path 会经 run_output → child_path
/// 再次访问本缓存，持锁会导致同线程重入死锁。
#[cfg(target_os = "macos")]
pub fn shell_path() -> Option<String> {
    let slot = SHELL_PATH.get_or_init(|| Mutex::new(None));
    // 快速路径：缓存已就绪
    if let Some(cached) = slot.lock().ok().and_then(|g| g.clone()) {
        return Some(cached);
    }
    // 慢路径：无锁捕获，完成后回填（并发重复捕获无害，内容一致）
    let captured = capture_shell_path();
    if let Some(s) = captured.clone() {
        if let Ok(mut g) = slot.lock() {
            *g = Some(s);
        }
    }
    captured
}

/// 启动子进程时应注入的 PATH：macOS 合并 GUI 环境 PATH 与 shell PATH
/// （dsh 的技能/子进程需要能找到 git 等工具）；其他平台返回 None（保持默认继承）。
pub fn effective_path() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut parts: Vec<String> = Vec::new();
        if let Ok(p) = std::env::var("PATH") {
            parts.push(p);
        }
        if let Some(s) = shell_path() {
            parts.push(s);
        }
        Some(parts.join(":"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// 所有子进程共用的 PATH：macOS 合并 GUI 进程 PATH 与「已捕获」的用户 shell PATH
/// （未捕获时仅返回当前 PATH——捕获流程自身也会走 run_output，必须避免递归/死锁）。
/// 用 try_lock：缓存正在被写入（捕获中）时退回当前 PATH，绝不阻塞。
/// 供 run_output / run_streaming 统一注入；其他平台返回 None。
pub fn child_path() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let cached = SHELL_PATH
            .get()
            .and_then(|slot| slot.try_lock().ok())
            .and_then(|g| g.clone());
        let mut parts: Vec<String> = Vec::new();
        if let Ok(p) = std::env::var("PATH") {
            parts.push(p);
        }
        if let Some(s) = cached {
            parts.push(s);
        }
        Some(parts.join(":"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[allow(dead_code)]
fn _unused_order() -> Ordering {
    Ordering::SeqCst
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_dsh_channel() {
        assert_eq!(normalize_dsh_channel("next"), "next");
        assert_eq!(normalize_dsh_channel(" next "), "next");
        assert_eq!(normalize_dsh_channel("latest"), "latest");
        assert_eq!(normalize_dsh_channel(""), "latest");
        assert_eq!(normalize_dsh_channel("beta"), "latest");
        assert_eq!(normalize_dsh_channel("NEXT"), "latest");
    }

    #[test]
    fn settings_deserialize_defaults_missing_dsh_channel_to_latest() {
        // 旧版 config.json 没有 dshChannel 字段：必须回退 latest，保持旧行为。
        let old = r#"{"port":3080,"registry":"https://registry.npmjs.org","autostart":false,"autoRestart":true,"petEnabled":true}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.dsh_channel, "latest");

        // 显式 next 必须保留。
        let with_next = r#"{"port":3080,"registry":"https://registry.npmjs.org","dshChannel":"next","autostart":false,"autoRestart":true,"petEnabled":true}"#;
        let s2: Settings = serde_json::from_str(with_next).unwrap();
        assert_eq!(s2.dsh_channel, "next");
    }

    #[test]
    fn parses_semver() {
        assert_eq!(parse_semver("v22.19.0"), Some((22, 19, 0)));
        assert_eq!(parse_semver("22.19.0"), Some((22, 19, 0)));
        assert_eq!(parse_semver("20.0.0"), Some((20, 0, 0)));
        assert_eq!(parse_semver("22.19"), Some((22, 19, 0)));
        assert_eq!(parse_semver("abc"), None);
        assert_eq!(parse_semver(""), None);
    }

    #[test]
    fn nvm_versions_in_dir_lists_only_version_dirs_sorted_desc() {
        let base = std::env::temp_dir().join(format!("whalito-nvm-versions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        for d in ["v22.19.0", "v18.20.4", "v24.1.0", "notes.txt", ".git"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        assert_eq!(
            nvm_versions_in_dir(&base),
            vec!["v24.1.0", "v22.19.0", "v18.20.4"]
        );
        // 空 / 不存在目录：空列表
        assert!(nvm_versions_in_dir(&base.join("nope")).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn nvm_switch_version_picks_highest_qualified() {
        // 直接验证筛选逻辑（目录扫描本身由 nvm_versions_in_dir 测试覆盖）：
        // 只对合格版本（>= MIN_NODE_VERSION）生效，且取最高。
        let versions = vec!["v18.20.4", "v22.19.0", "v24.1.0"];
        let picked = versions
            .into_iter()
            .find(|v| parse_semver(v).is_some_and(|t| t >= MIN_NODE_VERSION));
        assert_eq!(picked, Some("v22.19.0"));
        // 全部低于要求时无可切换版本
        let too_old = vec!["v18.20.4", "v16.0.0"];
        assert!(too_old
            .into_iter()
            .find(|v| parse_semver(v).is_some_and(|t| t >= MIN_NODE_VERSION))
            .is_none());
    }

    #[test]
    fn update_snooze_roundtrip_and_corruption_fallback() {
        let path = std::env::temp_dir().join(format!("whalito-snooze-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.tmp"));
        // 缺失 → 未静默
        assert_eq!(read_update_snooze_file(&path).dsh, None);
        let s = UpdateSnooze {
            dsh: Some(123),
            whalito: None,
        };
        write_update_snooze_file(&path, &s).unwrap();
        let loaded = read_update_snooze_file(&path);
        assert_eq!(loaded.dsh, Some(123));
        assert_eq!(loaded.whalito, None);
        // 损坏 → 回退未静默，不 panic
        std::fs::write(&path, "{broken").unwrap();
        assert_eq!(read_update_snooze_file(&path).dsh, None);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.tmp"));
    }

    #[test]
    fn compares_against_min() {
        assert!(parse_semver("v22.19.0").unwrap() >= MIN_NODE_VERSION);
        assert!(parse_semver("v22.20.1").unwrap() >= MIN_NODE_VERSION);
        assert!(parse_semver("v20.0.0").unwrap() < MIN_NODE_VERSION);
    }

    #[test]
    fn picks_first_candidate_with_ok_version() {
        let cands = vec![
            ("old-node".to_string(), Some("v20.0.0".to_string())),
            ("good-node".to_string(), Some("v22.19.0".to_string())),
            ("best-node".to_string(), Some("v24.1.0".to_string())),
        ];
        assert_eq!(pick_best_node(&cands).as_deref(), Some("good-node"));
    }

    #[test]
    fn picks_any_version_when_none_ok() {
        let cands = vec![
            ("old-node".to_string(), Some("v20.0.0".to_string())),
            ("broken-node".to_string(), None),
        ];
        assert_eq!(pick_best_node(&cands).as_deref(), Some("old-node"));
    }

    #[test]
    fn picks_first_when_no_version_at_all() {
        let cands = vec![
            ("a-node".to_string(), None),
            ("b-node".to_string(), None),
        ];
        assert_eq!(pick_best_node(&cands).as_deref(), Some("a-node"));
    }

    #[test]
    fn picks_none_for_empty_candidates() {
        assert!(pick_best_node(&[]).is_none());
    }

    #[test]
    fn npm_cli_finds_nearby_layouts() {
        let base = std::env::temp_dir().join(format!("whalito-npmcli-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        // POSIX 常规布局：<prefix>/bin/node + <prefix>/lib/node_modules/npm（Homebrew / nvm / tar 包）
        let posix = base.join("posix");
        let node = posix.join("bin").join("node");
        let npm_cli_path = posix
            .join("lib")
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npm-cli.js");
        std::fs::create_dir_all(node.parent().unwrap()).unwrap();
        std::fs::create_dir_all(npm_cli_path.parent().unwrap()).unwrap();
        std::fs::write(&node, "x").unwrap();
        std::fs::write(&npm_cli_path, "x").unwrap();
        // macOS 上 /var → /private/var 等符号链接会让 canonicalize 结果带前缀，
        // 期望值同样解析后再比较。
        let expected = std::fs::canonicalize(&npm_cli_path).unwrap();
        assert_eq!(
            npm_cli(node.to_str().unwrap())
                .map(|p| std::fs::canonicalize(&p).unwrap().to_string_lossy().to_string()),
            Some(expected.to_string_lossy().to_string())
        );

        // Windows 便携布局：node 同目录 node_modules/npm
        let win = base.join("win");
        let win_node = win.join("node.exe");
        let win_cli = win
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npm-cli.js");
        std::fs::create_dir_all(win_cli.parent().unwrap()).unwrap();
        std::fs::write(&win_node, "x").unwrap();
        std::fs::write(&win_cli, "x").unwrap();
        let expected_win = std::fs::canonicalize(&win_cli).unwrap();
        assert_eq!(
            npm_cli(win_node.to_str().unwrap())
                .map(|p| std::fs::canonicalize(&p).unwrap().to_string_lossy().to_string()),
            Some(expected_win.to_string_lossy().to_string())
        );

        // 空目录：找不到
        let empty = base.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(npm_cli(empty.join("node").to_str().unwrap()).is_none());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// macOS 官方 nodejs.org pkg 布局：/usr/local/bin/node 是符号链接，
    /// 真实路径在 lib/node_modules/node/bin/node，npm-cli.js 在
    /// lib/node_modules/npm/bin（node 上两级前缀的 lib 下）。
    #[cfg(unix)]
    #[test]
    fn npm_cli_finds_macos_official_pkg_layout() {
        let base = std::env::temp_dir().join(format!("whalito-npmcli-pkg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("usr").join("local");
        let bin_link = root.join("bin").join("node");
        let real_node = root
            .join("lib")
            .join("node_modules")
            .join("node")
            .join("bin")
            .join("node");
        let npm_cli_path = root
            .join("lib")
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npm-cli.js");
        std::fs::create_dir_all(bin_link.parent().unwrap()).unwrap();
        std::fs::create_dir_all(real_node.parent().unwrap()).unwrap();
        std::fs::create_dir_all(npm_cli_path.parent().unwrap()).unwrap();
        std::fs::write(&real_node, "x").unwrap();
        std::fs::write(&npm_cli_path, "x").unwrap();
        std::os::unix::fs::symlink(&real_node, &bin_link).unwrap();
        let expected = std::fs::canonicalize(&npm_cli_path).unwrap();
        assert_eq!(
            npm_cli(bin_link.to_str().unwrap())
                .map(|p| std::fs::canonicalize(&p).unwrap().to_string_lossy().to_string()),
            Some(expected.to_string_lossy().to_string())
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Homebrew 布局：/opt/homebrew/bin/node 是符号链接，真实二进制在
    /// Cellar/node/<ver>/bin/node，npm 却挂在 prefix 层 lib/node_modules/npm
    /// （离真实 node 4 级）——旧实现只查 Cellar 内部导致「未找到 npm」。
    #[cfg(unix)]
    #[test]
    fn npm_cli_finds_homebrew_prefix_layout() {
        let base = std::env::temp_dir().join(format!("whalito-npmcli-brew-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let prefix = base.join("opt").join("homebrew");
        let bin_link = prefix.join("bin").join("node");
        let real_node = prefix
            .join("Cellar")
            .join("node")
            .join("25.3.0")
            .join("bin")
            .join("node");
        let npm_cli_path = prefix
            .join("lib")
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npm-cli.js");
        std::fs::create_dir_all(bin_link.parent().unwrap()).unwrap();
        std::fs::create_dir_all(real_node.parent().unwrap()).unwrap();
        std::fs::create_dir_all(npm_cli_path.parent().unwrap()).unwrap();
        std::fs::write(&real_node, "x").unwrap();
        std::fs::write(&npm_cli_path, "x").unwrap();
        std::os::unix::fs::symlink(&real_node, &bin_link).unwrap();
        let expected = std::fs::canonicalize(&npm_cli_path).unwrap();
        assert_eq!(
            npm_cli(bin_link.to_str().unwrap())
                .map(|p| std::fs::canonicalize(&p).unwrap().to_string_lossy().to_string()),
            Some(expected.to_string_lossy().to_string())
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn dsh_bin_finds_both_npm_layouts() {
        let base = std::env::temp_dir().join(format!("whalito-dshbin-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // Windows 布局：<prefix>/node_modules/...
        let win = base.join("win");
        let win_bin = win
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib")
            .join("bin.js");
        std::fs::create_dir_all(win_bin.parent().unwrap()).unwrap();
        std::fs::write(&win_bin, "x").unwrap();
        assert_eq!(
            dsh_bin(win.to_str().unwrap()).map(|p| p.to_string_lossy().to_string()),
            Some(win_bin.to_string_lossy().to_string())
        );
        // POSIX 布局：<prefix>/lib/node_modules/...
        let posix = base.join("posix");
        let posix_bin = posix
            .join("lib")
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib")
            .join("bin.js");
        std::fs::create_dir_all(posix_bin.parent().unwrap()).unwrap();
        std::fs::write(&posix_bin, "x").unwrap();
        assert_eq!(
            dsh_bin(posix.to_str().unwrap()).map(|p| p.to_string_lossy().to_string()),
            Some(posix_bin.to_string_lossy().to_string())
        );
        // 空目录：找不到
        let empty = base.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(dsh_bin(empty.to_str().unwrap()).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
