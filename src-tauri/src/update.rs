//! 鲸仔版本信息与更新：检查走 GitHub releases（404 回退 tags），
//! 一键更新 = 下载对应平台变体的安装包（Windows NSIS / macOS dmg）→
//! 静默安装到当前目录 → 自动重启。
//! DSH 版本由现有 EnvInfo.dsh_version 与 check_latest_version（npm + 镜像源）提供。

#[cfg(windows)]
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::state;
use crate::state::{parse_semver, push_log, AppState, TEST_BUILD};

/// GitHub 仓库 API（发布源）。
const REPO_API: &str = "https://api.github.com/repos/entireyu/dsh-whalito-desk";
/// 更新进度事件（主窗口转发给内嵌设置分区）。
pub const UPDATE_EVENT: &str = "whalito-update";

/// 鲸仔版本信息。latest/url 仅在执行过更新检查后才有值。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WhalitoVersionInfo {
    pub current: String,
    pub test_build: bool,
    pub latest: Option<String>,
    pub update_available: bool,
    /// 该版本是否有适用于当前变体的安装包资产（测试版不上传 GitHub，通常为 false）。
    pub auto_update: bool,
    pub url: Option<String>,
}

/// 后台更新弹窗通知：目标 + 当前/最新版本 + 更新日志 + 详情链接。
/// 由每小时的后台检查生成，经 `update-available` 事件发给主窗口。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UpdateNotice {
    /// "dsh" | "whalito"
    pub target: String,
    pub current: String,
    pub latest: String,
    /// 更新日志（whalito 来自 GitHub release body；DSH 无可靠来源时为 None）。
    pub changelog: Option<String>,
    pub url: Option<String>,
}

/// 静默期：暂不更新后 24 小时内不再提醒。
pub const SNOOZE_MILLIS: u64 = 24 * 60 * 60 * 1000;

/// 更新标记（持久化在配置目录）：更新链启动前写入目标版本，
/// 应用重启后对比当前版本判断更新是否成功。
#[derive(Serialize, Deserialize, Clone)]
pub struct UpdateMarker {
    pub from: String,
    pub to: String,
}

/// 更新结果（重启后读标记得到，一次性消费）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UpdateMarkerResult {
    /// 当前运行版本 == 标记目标版本 → true（更新成功）。
    pub success: bool,
    pub from: String,
    pub to: String,
}

/// DSH 包主页（无独立更新日志来源时作为详情链接）。
const DSH_HOMEPAGE: &str = "https://github.com/deepseek-ai/deepseek-harness#readme";

/// 最新发布信息（版本 + 资产列表 + 发布页地址 + 发布说明）。
struct ReleaseInfo {
    version: String,
    assets: Vec<AssetInfo>,
    url: Option<String>,
    body: Option<String>,
}

#[derive(Clone)]
struct AssetInfo {
    name: String,
    url: String,
}

/// 当前版本（编译期），不含测试标记（标记由前端按 test_build 拼接）。
pub fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 当前平台标识（"windows" / "macos" / "linux"），用于选择更新资产变体。
pub fn platform_str() -> &'static str {
    std::env::consts::OS
}

/// 版本比较：latest 严格大于 current 视为有更新；任一侧解析失败视为无更新。
pub fn is_update_available(current: &str, latest: &str) -> bool {
    match (parse_semver(current), parse_semver(latest)) {
        (Some(c), Some(l)) => l > c,
        _ => false,
    }
}

/// 无网络版本信息（供握手快照使用）。
#[tauri::command]
pub fn whalito_version_info() -> WhalitoVersionInfo {
    WhalitoVersionInfo {
        current: current_version(),
        test_build: TEST_BUILD,
        latest: None,
        update_available: false,
        auto_update: false,
        url: None,
    }
}

/// 检查鲸仔更新（GitHub releases/latest，404 时回退 tags 列表第一个 tag）。
#[tauri::command]
pub fn whalito_check_update() -> Result<WhalitoVersionInfo, String> {
    let info = fetch_release_info()?;
    let current = current_version();
    let update_available = is_update_available(&current, &info.version);
    // 测试版安装包不上传 GitHub：无匹配资产时 auto_update=false，前端隐藏「立即更新」。
    let auto_update = update_available && pick_asset_for(&info.assets, platform_str(), TEST_BUILD).is_some();
    Ok(WhalitoVersionInfo {
        current,
        test_build: TEST_BUILD,
        latest: Some(info.version),
        update_available,
        auto_update,
        url: info.url,
    })
}

/// 当前 Unix 毫秒时间戳。
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 自动检查开关（0.5.1）：按目标读取设置。关闭后后台每小时检查不再查询
/// 该目标（省网络请求）；手动「检查更新」（whalito_check_update /
/// check_latest_version）不受影响，始终可用。
pub fn auto_check_enabled(settings: &state::Settings, target: &str) -> bool {
    match target {
        "dsh" => settings.dsh_auto_check_update,
        "whalito" => settings.whalito_auto_check_update,
        _ => true,
    }
}

/// 后台每小时调用的更新检查：汇总 DSH 与鲸仔的可用更新（含更新日志），
/// 跳过处于静默期（暂不更新）的目标与关闭了自动检查的目标；
/// 单个来源网络失败只跳过该目标，不影响其余。
pub fn check_update_notices(app: &AppHandle) -> Vec<UpdateNotice> {
    let snooze = state::load_update_snooze(app);
    let now = now_millis();
    let mut out = Vec::new();

    // 自动检查开关 + DSH 检查参数：合并一次读取（DSH 分支共用，避免重复加锁）。
    let settings = {
        let st = app.state::<AppState>();
        let guard = st.settings.lock().unwrap();
        let s = guard.clone();
        s
    };
    let auto_whalito = auto_check_enabled(&settings, "whalito");
    let auto_dsh = auto_check_enabled(&settings, "dsh");

    // —— 鲸仔 ——
    let whalito_snoozed = snooze.whalito.map(|t| t > now).unwrap_or(false);
    if auto_whalito && !whalito_snoozed {
        if let Ok(info) = fetch_release_info() {
            let current = current_version();
            // 与设置分区语义一致：仅当有匹配当前变体的安装包资产才提示
            //（测试构建不上传 GitHub，无资产则不打扰）。
            if is_update_available(&current, &info.version)
                && pick_asset_for(&info.assets, platform_str(), TEST_BUILD).is_some()
            {
                out.push(UpdateNotice {
                    target: "whalito".into(),
                    current,
                    latest: info.version.clone(),
                    changelog: info.body,
                    url: info.url,
                });
            }
        }
    }

    // —— DSH ——
    let dsh_snoozed = snooze.dsh.map(|t| t > now).unwrap_or(false);
    if auto_dsh && !dsh_snoozed {
        let (node_dir, registry, channel) = (
            settings.node_dir.clone(),
            settings.registry.clone(),
            state::normalize_dsh_channel(&settings.dsh_channel).to_string(),
        );
        let env = state::detect_env(node_dir.as_deref());
        if let Some(current) = env.dsh_version {
            if let Some(latest) =
                crate::commands::latest_dsh_version(node_dir, &registry, &channel)
            {
                // DSH 是 pre-release 版本号（如 0.1.0-rc.7），纯 semver 三元组
                // 比较会把它们判为相等，这里沿用设置分区「字符串不等」语义。
                if latest.trim() != current.trim() {
                    out.push(UpdateNotice {
                        target: "dsh".into(),
                        current: current.trim().to_string(),
                        latest: latest.trim().to_string(),
                        changelog: dsh_changelog(latest.trim()),
                        url: Some(DSH_HOMEPAGE.to_string()),
                    });
                }
            }
        }
    }

    out
}

/// DSH 更新日志（best-effort）：deepseek-harness 仓库 GitHub release 的 body。
/// npm 包 readme 为空、仓库也只在个别版本发布说明——匹配不到返回 None（前端显示占位文案）。
fn dsh_changelog(latest: &str) -> Option<String> {
    const RELEASES_URL: &str =
        "https://api.github.com/repos/deepseek-ai/deepseek-harness/releases?per_page=10";
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let body = agent
        .get(RELEASES_URL)
        .set("User-Agent", "whalito-update-check")
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let arr: serde_json::Value = serde_json::from_str(&body).ok()?;
    let arr = arr.as_array()?;
    for rel in arr {
        let tag = rel.get("tag_name")?.as_str()?;
        // 优先 tag 剥 v 后精确匹配（dsh-v0.1.0-rc.7 → 0.1.0-rc.7），再退化为包含匹配。
        let matched = strip_v(tag) == latest || tag.contains(latest);
        if !matched {
            continue;
        }
        let notes = rel.get("body")?.as_str()?.trim();
        if !notes.is_empty() {
            return Some(notes.to_string());
        }
    }
    None
}

/// 目标静默 24 小时（弹窗「暂不更新」）：期间后台检查不再提示该目标。
/// target: "dsh" / "whalito"。
#[tauri::command]
pub fn snooze_update(app: AppHandle, target: String) -> Result<(), String> {
    let mut snooze = state::load_update_snooze(&app);
    let until = now_millis() + SNOOZE_MILLIS;
    match target.as_str() {
        "dsh" => snooze.dsh = Some(until),
        "whalito" => snooze.whalito = Some(until),
        _ => return Err(format!("未知更新目标：{target}")),
    }
    state::save_update_snooze(&app, &snooze)?;
    Ok(())
}

/// 一键更新：检查 → 选资产 → 下载 → 静默安装 → 退出并由安装链重启应用。
/// `skip_confirm`：弹窗「立即更新」已构成确认，传入 true 跳过原生确认对话框；
/// 设置分区等既有调用不传，仍走原生确认。
#[tauri::command]
pub async fn whalito_apply_update(app: AppHandle, skip_confirm: Option<bool>) -> Result<(), String> {
    if !skip_confirm.unwrap_or(false) && !confirm_update(&app).await? {
        return Ok(());
    }
    emit(&app, "正在获取最新版本…");
    let info = tauri::async_runtime::spawn_blocking(fetch_release_info)
        .await
        .map_err(|e| e.to_string())??;
    let current = current_version();
    if !is_update_available(&current, &info.version) {
        return Err(format!("当前已是最新版本（{current}）"));
    }
    let asset = pick_asset_for(&info.assets, platform_str(), TEST_BUILD)
        .ok_or_else(|| format!("该版本没有适用于{}的安装包", if TEST_BUILD { "测试版" } else { "当前版本" }))?;
    emit(&app, "正在下载更新…");
    let dest = std::env::temp_dir().join(format!("whalito-update-{}", asset.name));
    let url = asset.url;
    let dest_for_dl = dest.clone();
    tauri::async_runtime::spawn_blocking(move || download_to(&url, &dest_for_dl))
        .await
        .map_err(|e| e.to_string())??;
    emit(&app, "已开始安装，应用即将重启…");
    // 先写更新标记（目标版本），重启后据此校验是否更新成功；写失败不阻断更新。
    write_update_marker(&app, &current, &info.version);
    spawn_update_chain(&dest)?;
    app.exit(0);
    Ok(())
}

/// 更新标记文件路径（应用配置目录，与 config.json 同目录）。
fn update_marker_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("update_marker.json"))
}

/// 写更新标记：应用退出前记录「从 from 更新到 to」。
/// 失败只记日志，不阻断更新流程（无标记则重启后不校验、不提示）。
fn write_update_marker(app: &AppHandle, from: &str, to: &str) {
    let marker = UpdateMarker {
        from: from.to_string(),
        to: to.to_string(),
    };
    let path = match update_marker_path(app) {
        Ok(p) => p,
        Err(e) => {
            push_log(&app.state::<AppState>().logs, &format!("[系统] 更新标记路径失败：{e}"));
            return;
        }
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            push_log(&app.state::<AppState>().logs, &format!("[系统] 更新标记目录创建失败：{e}"));
            return;
        }
    }
    match serde_json::to_string(&marker) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                push_log(&app.state::<AppState>().logs, &format!("[系统] 更新标记写入失败：{e}"));
            }
        }
        Err(e) => push_log(&app.state::<AppState>().logs, &format!("[系统] 更新标记序列化失败：{e}")),
    }
}

/// 更新是否成功：当前运行版本与标记目标版本一致（容忍首尾空白）。
pub fn update_succeeded(current: &str, to: &str) -> bool {
    to.trim() == current
}

/// 读取并消费更新标记（应用重启后调用一次）：
/// 对比当前运行版本与标记目标版本，得出更新是否成功；无论结果如何都删除标记，
/// 保证只提示一次。无标记返回 None（正常启动）。
#[tauri::command]
pub fn whalito_update_result(app: AppHandle) -> Option<UpdateMarkerResult> {
    let path = update_marker_path(&app).ok()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let marker: UpdateMarker = serde_json::from_str(&text).ok()?;
    let _ = std::fs::remove_file(&path);
    let success = update_succeeded(&current_version(), &marker.to);
    Some(UpdateMarkerResult {
        success,
        from: marker.from,
        to: marker.to,
    })
}

/// 更新确认对话框。window.confirm 在 WebView2 中不可用（默认脚本对话框只支持
/// alert，confirm 静默返回 false），确认改走 tauri-plugin-dialog 的原生对话框；
/// 用户取消返回 Ok(false)（无错误，静默结束）。
async fn confirm_update(app: &AppHandle) -> Result<bool, String> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        handle
            .dialog()
            .message("将下载并安装鲸仔新版本，应用会自动重启。继续？")
            .title("鲸仔更新")
            .buttons(MessageDialogButtons::OkCancelCustom(
                "立即更新".to_string(),
                "取消".to_string(),
            ))
            .blocking_show()
    })
    .await
    .map_err(|e| format!("显示更新确认对话框失败：{e}"))
}

/// 鲸仔更新确认对话框的独立命令：前端在**打开全屏 loading 之前**先调用它，
/// 用户确认后才进入更新流程（避免「确认框还没点，loading 已经盖上」）。
/// 确认后前端应以 `whalito_apply_update(skipConfirm=true)` 执行，跳过二次确认。
#[tauri::command]
pub async fn confirm_whalito_update(app: AppHandle) -> Result<bool, String> {
    confirm_update(&app).await
}

/// DSH 升级前的备份确认对话框（设置分区「立即更新」入口调用）。
/// 鲸仔会自动备份 DSH 配置与插件（backup_dsh_config），这里再让用户确认一次
/// 重要数据已自行备份（双保险），确认后才开始升级；取消返回 Ok(false)。
#[tauri::command]
pub async fn confirm_dsh_update(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        handle
            .dialog()
            .message(
                "升级 DeepSeek Harness 前，请先备份插件信息。\n\n鲸仔会自动备份 DSH 配置与插件（备份路径见运行日志），建议您也确认重要数据已自行备份。\n\n确认已备份并继续升级？",
            )
            .title("升级前备份确认")
            .buttons(MessageDialogButtons::OkCancelCustom(
                "确认已备份，开始升级".to_string(),
                "取消".to_string(),
            ))
            .blocking_show()
    })
    .await
    .map_err(|e| format!("显示备份确认对话框失败：{e}"))
}

/// 选择当前平台 + 变体适用的安装包资产：
/// Windows 匹配 `_x64-setup.exe`，macOS 匹配 `.dmg`；
/// 测试构建只接受名称含 "-Test_" 的资产，生产构建只接受不含 "-Test_" 的资产。
fn pick_asset_for(assets: &[AssetInfo], platform: &str, test_build: bool) -> Option<AssetInfo> {
    let matches: Vec<&AssetInfo> = assets
        .iter()
        .filter(|a| match platform {
            "windows" => a.name.ends_with("_x64-setup.exe"),
            "macos" => a.name.ends_with(".dmg"),
            _ => false,
        })
        .collect();
    if matches.is_empty() {
        return None;
    }
    let matched = matches
        .iter()
        .find(|a| a.name.contains("-Test_") == test_build)
        .copied();
    match matched {
        Some(a) => Some(a.clone()),
        None => {
            if test_build {
                None
            } else {
                Some(matches[0].clone())
            }
        }
    }
}

/// 下载到目标文件并做基本校验（Windows：MZ 魔数；macOS：非空且扩展名为 .dmg）。
fn download_to(url: &str, dest: &Path) -> Result<(), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(300))
        .build();
    let resp = agent
        .get(url)
        .set("User-Agent", "whalito-update-check")
        .call()
        .map_err(|e| format!("下载更新失败：{e}"))?;
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| format!("创建临时文件失败：{e}"))?;
    std::io::copy(&mut reader, &mut file).map_err(|e| format!("写入临时文件失败：{e}"))?;
    drop(file);
    #[cfg(windows)]
    {
        let mut f = std::fs::File::open(dest).map_err(|e| format!("校验下载文件失败：{e}"))?;
        let mut magic = [0u8; 2];
        f.read_exact(&mut magic).map_err(|e| format!("读取下载文件失败：{e}"))?;
        if &magic != b"MZ" {
            let _ = std::fs::remove_file(dest);
            return Err("下载的文件不是有效的 Windows 安装程序".into());
        }
    }
    #[cfg(target_os = "macos")]
    {
        let meta = std::fs::metadata(dest).map_err(|e| format!("校验下载文件失败：{e}"))?;
        let is_dmg = dest
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("dmg"));
        if meta.len() == 0 || !is_dmg {
            let _ = std::fs::remove_file(dest);
            return Err("下载的文件不是有效的 macOS 安装包".into());
        }
    }
    Ok(())
}

/// 启动更新链：等待旧进程退出 → 安装新版本到当前目录 → 重启应用。
/// Windows 链由独立 wscript（GUI 子系统，无控制台窗口）承载；macOS 链由独立 /bin/sh 脚本承载。
fn spawn_update_chain(dest: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("获取当前程序路径失败：{e}"))?;
    let dir = exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(windows)]
    {
        let script = build_update_vbs(dest, &dir, &exe);
        let script_path = std::env::temp_dir().join(format!(
            "whalito-update-{}.vbs",
            std::process::id()
        ));
        std::fs::write(&script_path, &script).map_err(|e| format!("写入更新脚本失败：{e}"))?;
        use std::os::windows::process::CommandExt;
        // wscript.exe 是 GUI 子系统进程：即使不带 CREATE_NO_WINDOW 也不会弹控制台，
        // 保留该 flag 只是双保险。
        std::process::Command::new("wscript.exe")
            .arg(&script_path)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn()
            .map_err(|e| format!("启动更新进程失败：{e}"))?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        // macOS：current_exe() 指向 <...>/Whalito.app/Contents/MacOS/Whalito，
        // 必须向上找到 .app 包根目录，安装目录 = 包根目录的父目录（如 /Applications）。
        // 修复：旧实现误用 exe.parent()（Contents/MacOS）作为安装目录，导致
        // 「rm -rf 清不掉旧包、ditto 把新包拷进旧包内部的 Contents/MacOS/Whalito.app」，
        // 最终 open 重启的还是旧二进制——版本永远不变，还留下嵌套垃圾包。
        let bundle_root = std::iter::successors(exe.parent(), |p| p.parent())
            .find(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("app"))
            })
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                format!(
                    "无法定位 .app 应用包目录（当前程序路径：{}）",
                    exe.display()
                )
            })?;
        // .app 包名 = 包根目录文件名（productName 与卷名同源，dmg 卷内同名 .app）。
        let app_name = bundle_root
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "Whalito".to_string());
        let app_dir = bundle_root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dir.clone());
        let script = build_update_script();
        let script_path = std::env::temp_dir().join("whalito-update.sh");
        std::fs::write(&script_path, &script).map_err(|e| format!("写入更新脚本失败：{e}"))?;
        std::process::Command::new("/bin/sh")
            .arg(&script_path)
            .arg(dest)
            .arg(&app_dir)
            .arg(&app_name)
            .spawn()
            .map_err(|e| format!("启动更新进程失败：{e}"))?;
        Ok(())
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        Err("当前平台暂不支持自动更新".into())
    }
}

/// macOS 更新脚本模板（参数经位置变量传入，避免路径引号注入问题）：
/// $1=dmg 路径，$2=.app 所在目录（如 /Applications），$3=.app 包名。
/// 流程：等旧进程退出 → 去掉隔离属性 → 挂载 dmg（按制表符解析卷挂载点，
/// 兼容卷名含空格）→ 新包先拷到同卷暂存目录 → 成功则原子替换并重启新版本；
/// 任一环节失败则清理暂存、保留旧包并重启旧版本（不丢应用）。
/// 执行日志写 /tmp/whalito-update.log 便于排查。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn build_update_script() -> String {
    [
        "#!/bin/sh",
        "set -u",
        "LOG=/tmp/whalito-update.log",
        "log() { echo \"[$(date '+%Y-%m-%d %H:%M:%S')] $*\" >> \"$LOG\"; }",
        "DMG=\"$1\"; APPDIR=\"$2\"; APPNAME=\"$3\"",
        // 等旧进程退出
        "sleep 4",
        // 移除下载隔离属性（未公证版本，避免 Gatekeeper 二次拦截）
        "xattr -dr com.apple.quarantine \"$DMG\" 2>/dev/null",
        // 挂载并解析卷挂载点：hdiutil 输出为制表符分隔，取最后一列，兼容卷名含空格
        "MOUNT=$(hdiutil attach \"$DMG\" -nobrowse 2>/dev/null | awk -F '\\t' '/\\/Volumes\\// {print $NF}' | head -1)",
        "if [ -z \"$MOUNT\" ]; then log \"挂载 dmg 失败\"; exit 1; fi",
        // 新版本先拷到同卷暂存目录，成功后原子替换，避免中途失败丢掉旧应用
        "STAGE=\"$APPDIR/$APPNAME.new.app\"",
        "rm -rf \"$STAGE\"",
        "if ditto \"$MOUNT/$APPNAME.app\" \"$STAGE\"; then",
        "  hdiutil detach \"$MOUNT\" -quiet 2>/dev/null",
        // ditto 会保留源 xattr，这里再清一次暂存包的隔离属性，防止 Gatekeeper 拦截重启
        "  xattr -dr com.apple.quarantine \"$STAGE\" 2>/dev/null",
        "  rm -rf \"$APPDIR/$APPNAME.app\"",
        "  if mv \"$STAGE\" \"$APPDIR/$APPNAME.app\"; then",
        "    log \"安装完成，重启新版本\"",
        "    open \"$APPDIR/$APPNAME.app\"",
        "  else",
        "    log \"替换应用包失败：$APPDIR/$APPNAME.app\"",
        "    exit 1",
        "  fi",
        "else",
        "  hdiutil detach \"$MOUNT\" -quiet 2>/dev/null",
        "  rm -rf \"$STAGE\"",
        "  log \"复制新版本失败，保留旧版本并重启\"",
        "  open \"$APPDIR/$APPNAME.app\" 2>/dev/null || true",
        "  exit 1",
        "fi",
        "",
    ]
    .join("\n")
}

/// 组装更新脚本（WScript，独立纯函数，便于单测）：
/// 等待 5 秒等应用退出 → 隐藏并等待静默安装（/D= 必须位于末尾、不额外引号）→ 重启应用。
/// wscript.exe 是 GUI 子系统进程，永不弹出控制台窗口（旧 cmd 链里的 ping 会闪出终端）。
#[cfg_attr(not(windows), allow(dead_code))]
pub fn build_update_vbs(installer: &Path, install_dir: &Path, app_exe: &Path) -> String {
    // VBScript 字符串里双引号写成 ""；反斜杠是字面量，无需转义。
    format!(
        "WScript.Sleep 5000\n\
         Set sh = CreateObject(\"WScript.Shell\")\n\
         sh.Run \"\"\"{}\"\" /S /D={}\", 0, True\n\
         sh.Run \"\"\"{}\"\"\", 1, False\n",
        installer.display(),
        install_dir.display(),
        app_exe.display()
    )
}

/// 拉取最新发布信息：releases/latest，404 时回退 tags 列表第一个 tag（无资产）。
fn fetch_release_info() -> Result<ReleaseInfo, String> {
    let releases = format!("{REPO_API}/releases/latest");
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    match agent.get(&releases).set("User-Agent", "whalito-update-check").call() {
        Ok(resp) => {
            let body = resp.into_string().map_err(|e| format!("读取响应失败：{e}"))?;
            parse_release_json(&body)
        }
        Err(ureq::Error::Status(404, _)) => {
            // 仓库还没有 release：回退 tags 列表第一个 tag（无资产可下载）。
            let body = http_get(&format!("{REPO_API}/tags"))?;
            let tags: Vec<serde_json::Value> =
                serde_json::from_str(&body).map_err(|e| format!("解析 tags 失败：{e}"))?;
            let first = tags
                .first()
                .and_then(|t| t.get("name"))
                .and_then(|n| n.as_str())
                .map(strip_v)
                .unwrap_or_else(|| "0.0.0".to_string());
            Ok(ReleaseInfo {
                version: first,
                assets: Vec::new(),
                url: None,
                body: None,
            })
        }
        Err(e) => Err(format!("检查更新失败：{e}")),
    }
}

fn http_get(url: &str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let resp = agent
        .get(url)
        .set("User-Agent", "whalito-update-check")
        .call()
        .map_err(|e| format!("请求 {url} 失败：{e}"))?;
    resp.into_string().map_err(|e| format!("读取响应失败：{e}"))
}

fn parse_release_json(body: &str) -> Result<ReleaseInfo, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("解析响应失败：{e}"))?;
    let version = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .map(strip_v)
        .unwrap_or_else(|| "0.0.0".to_string());
    let url = v.get("html_url").and_then(|u| u.as_str()).map(String::from);
    // GitHub release body 即发布说明（更新日志），空串视为无。
    let body = v
        .get("body")
        .and_then(|b| b.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let assets = v
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    Some(AssetInfo {
                        name: a.get("name")?.as_str()?.to_string(),
                        url: a.get("browser_download_url")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ReleaseInfo {
        version,
        assets,
        url,
        body,
    })
}

fn strip_v(tag: &str) -> String {
    tag.trim().trim_start_matches('v').to_string()
}

/// 更新进度：写日志 + 发事件给主窗口。
fn emit(app: &AppHandle, stage: &str) {
    push_log(
        &app.state::<AppState>().logs,
        &format!("[系统] 鲸仔更新：{stage}"),
    );
    let _ = app.emit(UPDATE_EVENT, stage.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_check_enabled_gates_per_target() {
        // 默认两个目标都自动检查；分别关闭互不影响；未知目标视为开启。
        let mut s = state::Settings::default();
        assert!(auto_check_enabled(&s, "dsh"));
        assert!(auto_check_enabled(&s, "whalito"));
        s.dsh_auto_check_update = false;
        assert!(!auto_check_enabled(&s, "dsh"));
        assert!(auto_check_enabled(&s, "whalito"));
        // 关闭鲸仔自动检查不影响 DSH（先恢复 DSH 为开启，验证互不干扰）。
        s.dsh_auto_check_update = true;
        s.whalito_auto_check_update = false;
        assert!(auto_check_enabled(&s, "dsh"));
        assert!(!auto_check_enabled(&s, "whalito"));
        assert!(auto_check_enabled(&s, "unknown"));
    }

    #[test]
    fn update_available_compares_semver() {
        assert!(is_update_available("0.2.0", "0.3.0"));
        assert!(is_update_available("0.2.0", "1.0.0"));
        assert!(is_update_available("0.2.0", "0.2.1"));
        assert!(!is_update_available("0.2.0", "0.2.0"));
        assert!(!is_update_available("0.3.0", "0.2.9"));
    }

    #[test]
    fn update_available_tolerates_bad_input() {
        assert!(!is_update_available("0.2.0", ""));
        assert!(!is_update_available("abc", "0.3.0"));
        // parse_semver 把 0.3.0-pre 解析为 (0,3,0)，视为有更新。
        assert!(is_update_available("0.2.0", "v0.3.0-pre"));
        assert!(is_update_available("v0.2.0", "v0.3.0"));
    }

    #[test]
    fn strips_v_prefix() {
        assert_eq!(strip_v("v0.2.0"), "0.2.0");
        assert_eq!(strip_v("0.2.0"), "0.2.0");
    }

    #[test]
    fn update_succeeded_compares_versions() {
        assert!(update_succeeded("0.4.4", "0.4.4"));
        assert!(update_succeeded("0.4.4", " 0.4.4 "));
        assert!(!update_succeeded("0.4.4", "0.4.5"));
        assert!(!update_succeeded("0.4.4", ""));
    }

    #[test]
    fn parse_release_json_captures_body() {
        // body 以 markdown 标题 `## ` 开头，raw string 定界符需多于两个 #。
        let json = r###"{
            "tag_name": "v0.4.4",
            "html_url": "https://github.com/entireyu/dsh-whalito-desk/releases/tag/v0.4.4",
            "body": "## [0.4.4] - 2026-08-18\n### 修复\n- 一些修复",
            "assets": [
                {"name": "Whalito_0.4.4_x64-setup.exe", "browser_download_url": "u1"},
                {"name": "Whalito-Test_0.4.4_x64-setup.exe", "browser_download_url": "u2"}
            ]
        }"###;
        let info = parse_release_json(json).unwrap();
        assert_eq!(info.version, "0.4.4");
        assert_eq!(info.assets.len(), 2);
        assert!(info.body.as_deref().unwrap().contains("一些修复"));
    }

    #[test]
    fn parse_release_json_tolerates_missing_or_empty_body() {
        let no_body = r#"{"tag_name": "v0.4.4", "assets": []}"#;
        assert!(parse_release_json(no_body).unwrap().body.is_none());
        let empty_body = r#"{"tag_name": "v0.4.4", "body": "   ", "assets": []}"#;
        assert!(parse_release_json(empty_body).unwrap().body.is_none());
    }

    #[test]
    fn update_script_is_windowless_and_contains_install_relaunch() {
        let script = build_update_vbs(
            Path::new(r"C:\Temp\setup.exe"),
            Path::new(r"C:\App\whalito"),
            Path::new(r"C:\App\whalito\Whalito.exe"),
        );
        // 旧实现用 ping 等待会闪出控制台窗口，必须移除。
        assert!(!script.contains("ping"));
        assert!(script.contains("WScript.Sleep 5000"));
        assert!(script.contains("CreateObject(\"WScript.Shell\")"));
        // 静默安装参数：安装包路径 + /S + 末位不引号的 /D= 安装目录。
        assert!(script.contains("/S /D=C:\\App\\whalito"));
        assert!(script.contains("C:\\Temp\\setup.exe"));
        assert!(script.contains("C:\\App\\whalito\\Whalito.exe"));
    }

    #[test]
    fn picks_prod_asset_in_prod_build() {
        let assets = vec![
            AssetInfo {
                name: "Whalito_0.3.0_x64-setup.exe".into(),
                url: "u1".into(),
            },
            AssetInfo {
                name: "Whalito-Test_0.3.0_x64-setup.exe".into(),
                url: "u2".into(),
            },
            AssetInfo {
                name: "Whalito_0.3.0_universal.dmg".into(),
                url: "u3".into(),
            },
            AssetInfo {
                name: "notes.txt".into(),
                url: "u4".into(),
            },
        ];
        // 测试构建会选 Test 资产；生产构建选非 Test。
        let picked = pick_asset_for(&assets, "windows", TEST_BUILD).expect("should pick an asset");
        assert!(!picked.name.contains("notes.txt"));
        assert!(!picked.name.ends_with(".dmg"));
        let setup_only = assets
            .iter()
            .filter(|a| a.name.ends_with("_x64-setup.exe"))
            .collect::<Vec<_>>();
        assert_eq!(setup_only.len(), 2);
        if TEST_BUILD {
            assert!(picked.name.contains("-Test_"));
        } else {
            assert!(!picked.name.contains("-Test_"));
        }
    }

    #[test]
    fn no_asset_for_test_build_without_test_asset() {
        let assets = vec![AssetInfo {
            name: "Whalito_0.3.0_x64-setup.exe".into(),
            url: "u1".into(),
        }];
        let picked = pick_asset_for(&assets, "windows", TEST_BUILD);
        if TEST_BUILD {
            assert!(picked.is_none());
        } else {
            assert!(picked.is_some());
        }
    }

    #[test]
    fn picks_assets_by_platform() {
        let assets = vec![
            AssetInfo {
                name: "Whalito_0.3.0_x64-setup.exe".into(),
                url: "exe".into(),
            },
            AssetInfo {
                name: "Whalito_0.3.0_universal.dmg".into(),
                url: "dmg".into(),
            },
        ];
        assert_eq!(
            pick_asset_for(&assets, "windows", false).map(|a| a.name),
            Some("Whalito_0.3.0_x64-setup.exe".to_string())
        );
        assert_eq!(
            pick_asset_for(&assets, "macos", false).map(|a| a.name),
            Some("Whalito_0.3.0_universal.dmg".to_string())
        );
        assert!(pick_asset_for(&assets, "linux", false).is_none());
    }

    #[test]
    fn macos_update_script_is_complete() {
        let s = build_update_script();
        assert!(s.starts_with("#!/bin/sh"));
        assert!(s.contains("xattr -dr com.apple.quarantine"));
        assert!(s.contains("hdiutil attach \"$DMG\" -nobrowse"));
        // 挂载点按制表符取最后一列（兼容卷名含空格）
        assert!(s.contains("awk -F '\\t' '/\\/Volumes\\// {print $NF}'"));
        // 新包先拷到同卷暂存目录，成功后原子替换
        assert!(s.contains("ditto \"$MOUNT/$APPNAME.app\" \"$STAGE\""));
        assert!(s.contains("mv \"$STAGE\" \"$APPDIR/$APPNAME.app\""));
        assert!(s.contains("hdiutil detach \"$MOUNT\" -quiet"));
        // 重启走 .app 包（而非旧实现的裸可执行文件路径）
        assert!(s.contains("open \"$APPDIR/$APPNAME.app\""));
        // 失败兜底：保留旧包并重启旧版本
        assert!(s.contains("保留旧版本并重启"));
    }
}
