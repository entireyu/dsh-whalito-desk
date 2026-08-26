//! dsh-market 插件市场集成（鲸仔 0.5.0）。
//!
//! 市场界面、浏览/搜索、安装执行、热挂载全部由 dsh-market 插件自带
//! （出现在 DSH 设置页），鲸仔只负责两件事：
//! 1. 启动前幂等预装：web profile 的 `dsh.profile.bundles` 缺 dshmarket 且用户
//!    从未装过（或主动卸载过）时，执行 `dsh plugin --profile web add dshmarket`，
//!    保证 DSH 首次启动即带市场、无需二次重启。
//! 2. 自重启接管：dsh-market 热挂载失败时会自重启（detached helper 用相同启动
//!    命令拉起新进程再杀掉旧进程）。鲸仔是 DSH 进程的外部管理者，旧 pid 退出后
//!    必须按端口定位新进程接管，否则会误报「服务器已停止」、托盘/停止/自动重启
//!    全部失效。接管判定由 `decide_takeover` / `takeover_pid` 提供，wait 线程
//!    （commands.rs）在旧进程退出时调用。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::commands::Shared;
use crate::settings_plugin::{dsh_home, profile_dir};
use crate::state::{self, AppState};

/// dsh-market 的 npm 包名（awesome 官方站点每个插件的「在 dsh-market 中安装」
/// 按钮即 `dsh plugin --profile web add dshmarket`）。
pub const MARKET_PKG: &str = "dshmarket";

/// 自重启接管时端口探测的重试次数与间隔。
/// dsh-market 的 detached helper 在旧进程退出、端口释放后才拉起新进程
/// （Windows 还带 300ms TIME_WAIT 缓冲），从退出到新进程可服务通常 1~3 秒；
/// 500ms × 20 = 10 秒窗口覆盖慢启动，超时按普通退出处理（external 兜底）。
pub const TAKEOVER_RETRIES: usize = 20;
pub const TAKEOVER_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// 市场持久状态（app_config_dir/plugin-market/state.json）。
/// `installed_once`：鲸仔曾自动装过 dshmarket。用户之后主动卸载（bundles 中
/// 消失）→ 鲸仔不再自动装回；面板可显式「重新安装」（force 清除该标记）。
#[derive(Serialize, Deserialize, Default, Clone, Copy)]
pub struct MarketState {
    #[serde(default)]
    pub installed_once: bool,
}

fn market_state_path(app: &AppHandle) -> PathBuf {
    match app.path().app_config_dir() {
        Ok(dir) => dir.join("plugin-market").join("state.json"),
        Err(_) => std::env::temp_dir().join("whalito-market-state.json"),
    }
}

pub fn load_market_state(app: &AppHandle) -> MarketState {
    let path = market_state_path(app);
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_market_state(app: &AppHandle, state: &MarketState) -> Result<(), String> {
    let path = market_state_path(app);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
    }
    let text = serde_json::to_string(state).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("写入 {} 失败：{e}", path.display()))
}

/// 读取 profile 的 `dsh.profile.bundles`（dsh plugin 维护的层列表）。
/// 实现已移到 settings_plugin（与 patch 同步共用），此处仅为引用别名。
pub use crate::settings_plugin::installed_bundles;

/// `dsh plugin --profile web add <pkg>` 的 argv（bin 为 lib/bin.js 路径，
/// 由 node 直接执行，绕开 Windows shim 与 PATH 问题）。
pub fn market_add_args(bin: &str, pkg: &str) -> Vec<String> {
    vec![
        bin.to_string(),
        "plugin".to_string(),
        "--profile".to_string(),
        "web".to_string(),
        "add".to_string(),
        pkg.to_string(),
    ]
}

/// 预装判定（纯函数）：需要执行安装 = DSH 已装 && bundles 无 dshmarket &&
/// 用户从未装过（或主动卸载后未显式重装）。
pub fn should_install(bundles: &[String], installed_once: bool, dsh_installed: bool) -> bool {
    dsh_installed && !bundles.iter().any(|b| b == MARKET_PKG) && !installed_once
}

/// 自重启接管判定（纯函数）：旧进程退出且**非用户停止**时，端口仍有服务且
/// 能找到监听进程 → 接管该 pid；否则不接管（正常退出/external 兜底由调用方处理）。
pub fn decide_takeover(was_stopped: bool, port_alive: bool, pid_on_port: Option<u32>) -> Option<u32> {
    if was_stopped || !port_alive {
        return None;
    }
    pid_on_port
}

/// 组合式探测：端口健康 + 按端口定位新进程（供 wait 线程调用）。
/// health 探测通过说明端口上已有 HTTP 服务（新 DSH 已起来），find_pid_on_port
/// 给出其 pid；两处竞态窗口由调用方的重试循环覆盖。
pub fn takeover_pid(port: u16, was_stopped: bool) -> Option<u32> {
    let probe = format!("http://127.0.0.1:{port}");
    decide_takeover(
        was_stopped,
        state::health(&probe),
        state::find_pid_on_port(port),
    )
}

/// 执行 dsh CLI（node 直接运行 lib/bin.js），流式输出到运行日志与
/// `market-progress` 事件。与服务器启动保持同一套环境语义：
///   - DSH_HOME：与 settings_plugin 一致（测试构建 → ~/.dsh-test 隔离）
///   - PATH：node 所在目录前置 + effective_path（GUI 无 shell PATH 兜底）
///   - npm_config_registry：用户设置的 npm 镜像（dsh 内部 pnpm 读 npm config）
fn run_market_cli(app: &AppHandle, shared: &Shared, node: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd = std::process::Command::new(node);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd.env("DSH_HOME", dsh_home());
    let sep = if std::env::consts::OS == "windows" { ";" } else { ":" };
    let mut parts: Vec<String> = Vec::new();
    if let Some(dir) = Path::new(node).parent() {
        parts.push(dir.to_string_lossy().into_owned());
    }
    if let Some(p) = state::effective_path() {
        parts.push(p);
    }
    if !parts.is_empty() {
        cmd.env("PATH", parts.join(sep));
    }
    let registry = shared.registry();
    if !registry.is_empty() {
        cmd.env("npm_config_registry", registry);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }

    let mut child = cmd.spawn().map_err(|e| format!("无法启动 dsh CLI：{e}"))?;
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
        let logs = Arc::clone(&shared.logs);
        let collected = Arc::clone(&collected);
        handles.push(std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stream).lines() {
                match line {
                    Ok(line) => {
                        collected.lock().unwrap().push(line.clone());
                        state::push_log(&logs, &line);
                        let _ = app.emit("market-progress", &line);
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
            "dsh CLI 退出码 {}:\n{combined}",
            status.code().unwrap_or(-1)
        ))
    }
}

/// 启动前幂等预装 dsh-market（best-effort，失败不阻断 DSH 启动）。
/// 返回 true = 本次执行了安装（成功）；false = 无需安装（已装 / 用户曾卸载 /
/// 环境未就绪）。错误会先写入运行日志再返回 Err，调用方 `let _ =` 即可。
pub fn ensure_market_plugin(app: &AppHandle, shared: &Shared) -> Result<bool, String> {
    let env = state::detect_env(shared.node_dir().as_deref());
    if !env.dsh_installed {
        return Ok(false);
    }
    let node = env
        .node_path
        .clone()
        .ok_or("未检测到 Node.js，请先安装 Node.js。".to_string())?;
    let prefix = state::app_prefix_dir().to_string_lossy().into_owned();
    let bin = state::dsh_bin(&prefix).ok_or("找不到 dsh CLI 入口，请重新安装 Harness。".to_string())?;

    let bundles = installed_bundles(&profile_dir());
    let installed_once = load_market_state(app).installed_once;
    if bundles.iter().any(|b| b == MARKET_PKG) {
        // R2：环境里已有 dshmarket（用户机器预装 / 手动装的）→ 记 installedOnce，
        // 否则用户之后卸载它，下次启动 ensure 会把它装回来（违背「卸载后不装回」）。
        save_market_state(app, &MarketState { installed_once: true })?;
        return Ok(false);
    }
    if !should_install(&bundles, installed_once, true) {
        return Ok(false);
    }

    let args = market_add_args(&bin.to_string_lossy(), MARKET_PKG);
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    state::push_log(
        &shared.logs,
        &format!("[系统] 首次准备插件市场：安装 {MARKET_PKG}（之后如主动卸载，鲸仔不会再自动装回）"),
    );
    if let Err(e) = run_market_cli(app, shared, &node, &arg_refs) {
        // 失败时给出汇总日志（调用方 best-effort 会吞掉 Err，用户至少能在
        // 运行日志看到明确结论：市场未就绪 + 原因尾部）。
        state::push_log(
            &shared.logs,
            &format!("[系统] 插件市场 {MARKET_PKG} 安装失败（不影响服务器，可稍后在面板「检查 / 安装」重试）：{e}"),
        );
        return Err(e);
    }
    save_market_state(app, &MarketState { installed_once: true })?;
    state::push_log(
        &shared.logs,
        &format!("[系统] 插件市场 {MARKET_PKG} 安装完成，DSH 设置页可见「插件市场」"),
    );
    Ok(true)
}

/// 面板「插件市场」状态行（只读，轻量；DSH 是否已装由前端 env 快照提供）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MarketStatusView {
    /// web profile 的 bundles 中已有 dshmarket。
    pub installed: bool,
    /// 鲸仔曾自动装过（installedOnce 标记；为 true 且未安装 = 用户主动卸载过）。
    pub installed_once: bool,
}

#[tauri::command]
pub fn market_status(app: AppHandle, _st: State<'_, AppState>) -> MarketStatusView {
    MarketStatusView {
        installed: installed_bundles(&profile_dir()).iter().any(|b| b == MARKET_PKG),
        installed_once: load_market_state(&app).installed_once,
    }
}

/// 面板「插件市场」入口：force=true 时清除 installedOnce（用户显式要求重装），
/// 再走常规 ensure；返回人类可读的状态说明。
#[tauri::command]
pub async fn sync_market_plugin(app: AppHandle, st: State<'_, AppState>, force: bool) -> Result<String, String> {
    let shared = Shared::from_state(&st);
    tauri::async_runtime::spawn_blocking(move || {
        if force {
            let _ = save_market_state(&app, &MarketState { installed_once: false });
        }
        match ensure_market_plugin(&app, &shared) {
            Ok(true) => Ok("插件市场安装完成，DSH 设置页可见「插件市场」；如服务器已在运行，重启后生效。".to_string()),
            Ok(false) => {
                let installed = installed_bundles(&profile_dir())
                    .iter()
                    .any(|b| b == MARKET_PKG);
                if installed {
                    Ok("插件市场已就绪（DSH 设置页可见「插件市场」）。".to_string())
                } else if load_market_state(&app).installed_once {
                    Ok("插件市场已卸载，鲸仔不会自动装回；如需重新安装请再点一次「重新安装」。".to_string())
                } else {
                    Ok("环境未就绪：请先安装 Node.js 与 DeepSeek Harness，再试。".to_string())
                }
            }
            Err(e) => {
                state::push_log(&shared.logs, &format!("[系统] 插件市场准备失败（不影响服务器）：{e}"));
                Err(format!("插件市场准备失败：{e}"))
            }
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bundles_from_profile_package_json() {
        let base = std::env::temp_dir().join(format!("whalito-mkt-bundles-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let profile = base.join("profiles").join("web");
        fs::create_dir_all(&profile).unwrap();
        // 正常：含 dshmarket 与其他 bundle。
        fs::write(
            profile.join("package.json"),
            r#"{"name":"dsh-profile-web","dependencies":{"dshmarket":"1.2.3","x":"^1.0.0"},"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base","dshmarket","x"]}}}"#,
        )
        .unwrap();
        let b = installed_bundles(&profile);
        assert!(b.iter().any(|x| x == "dshmarket"));
        assert!(b.iter().any(|x| x == "@deepseek-ai/dsh-base"));
        assert_eq!(b.len(), 3);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn bundles_empty_when_file_missing_or_malformed() {
        let base = std::env::temp_dir().join(format!("whalito-mkt-nofile-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let profile = base.join("profiles").join("web");
        assert!(installed_bundles(&profile).is_empty());
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("package.json"), "not json").unwrap();
        assert!(installed_bundles(&profile).is_empty());
        fs::write(
            profile.join("package.json"),
            r#"{"dsh":{"profile":{}}}"#,
        )
        .unwrap();
        assert!(installed_bundles(&profile).is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn builds_market_add_argv() {
        let args = market_add_args("C:/dsh/bin.js", "dshmarket");
        assert_eq!(
            args,
            vec![
                "C:/dsh/bin.js",
                "plugin",
                "--profile",
                "web",
                "add",
                "dshmarket",
            ]
        );
    }

    #[test]
    fn should_install_requires_all_conditions() {
        let installed = vec!["dshmarket".to_string()];
        // 已装 → 不装。
        assert!(!should_install(&installed, false, true));
        // 用户曾装过/主动卸载 → 不装。
        assert!(!should_install(&[], true, true));
        // DSH 未装 → 不装。
        assert!(!should_install(&[], false, false));
        // 全新环境 → 装。
        assert!(should_install(&[], false, true));
        // 装的列表里没有但 installed_once 也没写 → 装。
        assert!(should_install(&["other".to_string()], false, true));
    }

    #[test]
    fn decide_takeover_rules() {
        // 用户主动停止 → 不接管。
        assert_eq!(decide_takeover(true, true, Some(42)), None);
        // 端口已死 → 不接管。
        assert_eq!(decide_takeover(false, false, Some(42)), None);
        // 端口活但找不到进程 → 不接管（external 兜底）。
        assert_eq!(decide_takeover(false, true, None), None);
        // 端口活 + 找到新进程 → 接管。
        assert_eq!(decide_takeover(false, true, Some(99)), Some(99));
    }

    #[test]
    fn market_state_roundtrip() {
        let state = MarketState { installed_once: true };
        let text = serde_json::to_string(&state).unwrap();
        let back: MarketState = serde_json::from_str(&text).unwrap();
        assert!(back.installed_once);
        // 缺省字段 → 默认 false（兼容旧状态文件）。
        let missing: MarketState = serde_json::from_str(r#"{}"#).unwrap_or_default();
        assert!(!missing.installed_once);
    }
}
