//! dsh 命令注册到系统 PATH（可选功能，用户级 / 系统级）。
//!
//! 鲸仔安装 DSH 的隔离前缀（Windows `%LOCALAPPDATA%\dsh-launcher\npm`、
//! macOS `~/Library/Application Support/com.deepseek.dsh-launcher/npm`）默认
//! **不在** PATH 里——终端无法直接敲 `dsh`。本模块提供一键注册/注销：
//! - **用户级**（默认，免管理员）：
//!   - Windows：`HKCU\Environment\Path`（保持 REG_EXPAND_SZ）+ 广播 WM_SETTINGCHANGE
//!   - macOS：`~/.zshrc` 标记块内的 `export PATH`
//! - **系统级**（影响所有用户，需要管理员/root）：
//!   - Windows：`HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment\Path`
//!   - macOS：`/etc/paths.d/whalito`（每行一个路径，PATH 合成标准位置）
//!
//! 说明：注册/注销只影响**新开的终端**（已运行的进程继承启动时的环境）；
//! 鲸仔自身从不依赖 PATH 定位 dsh（直接用 node + lib/bin.js），
//! 所以注册与否完全不影响鲸仔与 DSH 的运行。

use std::fs;
use std::path::PathBuf;

use serde::Serialize;
use tauri::State;

use crate::state::{push_log, AppState};

/// PATH 状态视图（前端展示）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DshPathView {
    /// 当前是否已注册（终端可直接敲 dsh）。
    pub registered: bool,
    /// 将被加入 PATH 的目录。
    pub prefix: String,
    /// 平台标识（"windows" / "macos"）。
    pub platform: String,
    /// 级别："user"（用户级）/ "system"（系统级）。
    pub level: String,
    /// 读取失败原因（如系统级权限不足）；None = 正常。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 要加入 PATH 的目录（Windows：prefix 根，shim 在那里；macOS：prefix/bin）。
pub fn path_entry() -> PathBuf {
    let prefix = crate::state::app_prefix_dir();
    #[cfg(target_os = "macos")]
    {
        prefix.join("bin")
    }
    #[cfg(not(target_os = "macos"))]
    {
        prefix
    }
}

fn path_sep() -> char {
    if std::env::consts::OS == "windows" {
        ';'
    } else {
        ':'
    }
}

/// 在 PATH 字符串中加入/移除一个目录（纯函数，便于测试）。
/// present=true 移除（注销）；false 追加（注册，幂等：已存在则不重复）。
pub fn merge_path(current: &str, entry: &str, present: bool) -> String {
    let sep = path_sep();
    #[cfg(windows)]
    fn normalize(s: &str) -> String {
        s.trim_end_matches('\\').trim().trim_end_matches('/').to_lowercase()
    }
    #[cfg(not(windows))]
    fn normalize(s: &str) -> String {
        s.trim_end_matches('/').trim().to_lowercase()
    }
    let norm_entry = normalize(entry);
    let mut parts: Vec<&str> = current
        .split(sep)
        .filter(|s| !s.trim().is_empty())
        .collect();
    if present {
        parts.retain(|p| normalize(p) != norm_entry);
        parts.join(&sep.to_string())
    } else {
        if !parts.iter().any(|p| normalize(p) == norm_entry) {
            parts.push(entry);
        }
        parts.join(&sep.to_string())
    }
}

/// 是否合法的级别参数。
fn parse_level(level: &str) -> Result<&'static str, String> {
    match level {
        "user" => Ok("user"),
        "system" => Ok("system"),
        other => Err(format!("无效的 PATH 级别：{other}（仅支持 user / system）")),
    }
}

fn level_label(level: &str) -> &'static str {
    if level == "system" {
        "系统级"
    } else {
        "用户级"
    }
}

/// —— Windows：注册表 Path ——
/// 用户级：HKCU\Environment；系统级：HKLM\...\Session Manager\Environment。
#[cfg(windows)]
fn path_registry_key(level: &str) -> &'static str {
    if level == "system" {
        "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment"
    } else {
        "Environment"
    }
}

#[cfg(windows)]
fn read_path_raw(level: &str) -> Result<(Vec<u8>, winreg::enums::RegType), String> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ};
    use winreg::RegKey;
    let (hive, sub) = if level == "system" {
        (HKEY_LOCAL_MACHINE, path_registry_key(level))
    } else {
        (HKEY_CURRENT_USER, path_registry_key(level))
    };
    let root = RegKey::predef(hive);
    let env = root
        .open_subkey_with_flags(sub, KEY_READ)
        .map_err(|e| format!("读取{}环境变量失败：{e}", level_label(level)))?;
    env.get_raw_value("Path")
        .map(|v| (v.bytes, v.vtype))
        .map_err(|e| format!("读取{} PATH 失败：{e}", level_label(level)))
}

#[cfg(windows)]
fn write_path_raw(level: &str, bytes: Vec<u8>, vtype: winreg::enums::RegType) -> Result<(), String> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_WRITE};
    use winreg::{RegKey, RegValue};
    let (hive, sub) = if level == "system" {
        (HKEY_LOCAL_MACHINE, path_registry_key(level))
    } else {
        (HKEY_CURRENT_USER, path_registry_key(level))
    };
    let root = RegKey::predef(hive);
    let env = root
        .open_subkey_with_flags(sub, KEY_WRITE)
        .map_err(|e| {
            format!(
                "打开{}环境变量失败：{e}（{} PATH 需要管理员权限，请以管理员身份运行鲸仔后再试）",
                level_label(level),
                level_label(level)
            )
        })?;
    env.set_raw_value("Path", &RegValue { bytes, vtype })
        .map_err(|e| format!("写入{} PATH 失败：{e}", level_label(level)))?;
    // 广播环境变更：让资源管理器/已运行的终端刷新，新进程才能继承新 PATH。
    broadcast_env_change();
    Ok(())
}

#[cfg(windows)]
fn broadcast_env_change() {
    #[link(name = "user32")]
    extern "system" {
        fn SendMessageTimeoutW(
            h_wnd: isize,
            msg: u32,
            w_param: usize,
            l_param: *const u16,
            fu_flags: u32,
            u_timeout: u32,
            lpdw_result: *mut usize,
        ) -> isize;
    }
    const HWND_BROADCAST: isize = 0xFFFF;
    const WM_SETTINGCHANGE: u32 = 0x001A;
    const SMTO_ABORTIFHUNG: u32 = 0x0002;
    let wide: Vec<u16> = "Environment\0".encode_utf16().collect();
    let mut result: usize = 0;
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            wide.as_ptr(),
            SMTO_ABORTIFHUNG,
            5000,
            &mut result,
        );
    }
}

#[cfg(windows)]
fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units).trim_end_matches('\0').to_string()
}

#[cfg(windows)]
fn encode_utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .flat_map(|u| u.to_le_bytes().to_vec())
        .collect()
}

/// —— macOS：用户级 ~/.zshrc 标记块 / 系统级 /etc/paths.d/whalito ——
#[cfg(target_os = "macos")]
const MAC_MARK_BEGIN: &str = "# whalito-dsh-path begin";
#[cfg(target_os = "macos")]
const MAC_MARK_END: &str = "# whalito-dsh-path end";

#[cfg(target_os = "macos")]
fn mac_user_file() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".zshrc")
}

#[cfg(target_os = "macos")]
fn mac_system_file() -> PathBuf {
    PathBuf::from("/etc/paths.d/whalito")
}

/// 在 shell 配置里 upsert/移除标记块（纯函数，便于测试）。
#[cfg(target_os = "macos")]
fn patch_zshrc(content: &str, block: &str, present: bool) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut out: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in lines {
        let t = line.trim();
        if t == MAC_MARK_BEGIN {
            skipping = true;
            continue;
        }
        if t == MAC_MARK_END {
            skipping = false;
            continue;
        }
        if !skipping {
            out.push(line);
        }
    }
    let mut result = out.join("\n");
    if !present {
        return result.trim_end().to_string() + if result.is_empty() { "" } else { "\n" };
    }
    if !result.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }
    result.push_str(MAC_MARK_BEGIN);
    result.push('\n');
    result.push_str(block);
    result.push('\n');
    result.push_str(MAC_MARK_END);
    result.push('\n');
    result
}

/// 当前注册状态（指定级别）。
#[tauri::command]
pub fn dsh_path_status(level: String) -> Result<DshPathView, String> {
    let level = parse_level(&level)?;
    status_view(level)
}

fn status_view(level: &str) -> Result<DshPathView, String> {
    let prefix = path_entry().to_string_lossy().into_owned();
    // 读取失败不整体报错：降级为「未注册」+ 原因，前端展示而非一直查询中。
    let (registered, error) = match path_contains_entry(level) {
        Ok(r) => (r, None),
        Err(e) => (false, Some(e)),
    };
    Ok(DshPathView {
        registered,
        prefix,
        platform: std::env::consts::OS.to_string(),
        level: level.to_string(),
        error,
    })
}

/// 指定级别的 PATH 是否已包含 dsh 目录。
fn path_contains_entry(level: &str) -> Result<bool, String> {
    let entry = path_entry().to_string_lossy().to_lowercase();
    #[cfg(windows)]
    {
        let (bytes, _) = read_path_raw(level)?;
        let current = decode_utf16le(&bytes);
        let normalize = |s: &str| s.trim_end_matches('\\').trim().to_lowercase();
        Ok(current
            .split(';')
            .any(|p| normalize(p) == entry.trim_end_matches('\\').to_lowercase()))
    }
    #[cfg(target_os = "macos")]
    {
        let file = if level == "system" {
            mac_system_file()
        } else {
            mac_user_file()
        };
        if level == "system" {
            let content = fs::read_to_string(&file).unwrap_or_default();
            Ok(content.contains(&entry))
        } else {
            let content = fs::read_to_string(&file).unwrap_or_default();
            Ok(content.contains(MAC_MARK_BEGIN))
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (entry, level);
        Ok(false)
    }
}

/// 注册（enable=true）或注销（enable=false）dsh 到指定级别的 PATH。
#[tauri::command]
pub fn dsh_path_toggle(
    app: tauri::AppHandle,
    st: State<'_, AppState>,
    enable: bool,
    level: String,
) -> Result<DshPathView, String> {
    let level = parse_level(&level)?;
    let entry_s = path_entry().to_string_lossy().into_owned();
    let action = if enable { "注册" } else { "注销" };

    #[cfg(windows)]
    {
        let (bytes, vtype) = read_path_raw(level)?;
        let current = decode_utf16le(&bytes);
        let merged = merge_path(&current, &entry_s, !enable);
        if merged != current {
            write_path_raw(level, encode_utf16le(&merged), vtype)?;
        }
    }
    #[cfg(target_os = "macos")]
    {
        if level == "system" {
            // 系统级：/etc/paths.d/whalito（每行一个路径，PATH 合成读取）。
            let file = mac_system_file();
            if enable {
                fs::write(&file, format!("{entry_s}\n"))
                    .map_err(|e| format!("写入 {} 失败：{e}（系统级需要管理员权限，请用 sudo 运行鲸仔后再试）", file.display()))?;
            } else {
                let _ = fs::remove_file(&file);
            }
        } else {
            let path = mac_user_file();
            let content = fs::read_to_string(&path).unwrap_or_default();
            let block = format!("export PATH=\"{entry_s}:$PATH\"");
            let patched = patch_zshrc(&content, &block, !enable);
            if patched != content {
                if let Some(dir) = path.parent() {
                    let _ = fs::create_dir_all(dir);
                }
                fs::write(&path, patched)
                    .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
            }
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (&entry_s, &action, level);
        return Err("当前平台暂不支持注册 dsh 到系统 PATH".to_string());
    }

    push_log(
        &st.logs,
        &format!(
            "[系统] 已{action} dsh 到{} PATH（{}），新开的终端可直接使用 dsh 命令",
            level_label(level),
            entry_s
        ),
    );
    status_view(level)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_path_adds_and_removes_entry() {
        let sep = path_sep();
        let current = format!("C:\\Windows{sep}C:\\Users\\me\\bin");
        let entry = "C:\\Users\\me\\AppData\\Local\\dsh-launcher\\npm";
        let added = merge_path(&current, entry, false);
        assert!(added.contains(entry));
        assert_eq!(added.matches(entry).count(), 1);
        assert_eq!(merge_path(&added, entry, false), added);
        let removed = merge_path(&added, entry, true);
        assert!(!removed.contains("dsh-launcher"));
        assert_eq!(removed, current);
        assert_eq!(merge_path("", entry, false), entry.to_string());
        assert_eq!(merge_path(&current, entry, true), current);
    }

    #[test]
    fn merge_path_dedupes_trailing_separator_and_case() {
        let sep = path_sep();
        let entry = "C:\\MY\\PATH";
        let current = format!("C:\\my\\path\\{sep}C:\\other");
        let added = merge_path(&current, entry, false);
        assert_eq!(added.matches("my\\path").count(), 1);
        let removed = merge_path(&current, entry, true);
        assert!(!removed.to_lowercase().contains("my\\path"));
        assert!(removed.contains("C:\\other"));
    }

    #[test]
    fn parse_level_accepts_user_system() {
        assert_eq!(parse_level("user").unwrap(), "user");
        assert_eq!(parse_level("system").unwrap(), "system");
        assert!(parse_level("global").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn patch_zshrc_upserts_and_removes_block() {
        let block = "export PATH=\"/tmp/x/bin:$PATH\"";
        let base = "# existing\nPATH=/usr/bin\n";
        let added = patch_zshrc(base, block, false);
        assert!(added.contains(MAC_MARK_BEGIN));
        assert!(added.contains(block));
        assert!(added.starts_with("# existing"));
        assert_eq!(patch_zshrc(&added, block, false), added);
        let removed = patch_zshrc(&added, block, true);
        assert!(!removed.contains(MAC_MARK_BEGIN));
        assert!(!removed.contains("export PATH=\"/tmp/x/bin"));
        assert!(removed.contains("# existing"));
    }
}
