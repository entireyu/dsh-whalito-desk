//! 鲸仔设置分区插件同步：把内置的 DSH 客户端插件包等内容写入 web profile
//! （node_modules + cordis.patch.yml 标记块）。所有维护动作只发生在标记块内，
//! 标记块外的用户内容绝不动。不改动 deepseek-harness 源码。

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::state::{push_log, AppState};

/// 一个内置插件包：cordis 条目 id、包名（同时是 node_modules 下的目录名）、
/// 编译期嵌入的文件清单。
pub struct PkgSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub files: &'static [(&'static str, &'static str)],
}

/// 内置插件包清单：鲸仔设置分区 + WebUI+ 增强。数组顺序即 patch.yml 中
/// insert 行的顺序；修改文件内容无需改这里（include_str! 编译期自动跟随）。
pub const PKGS: &[PkgSpec] = &[
    PkgSpec {
        id: "whalito-settings",
        name: "@entireyu/whalito-dsh-settings",
        files: &[
            ("package.json", include_str!("../whalito-dsh-settings/package.json")),
            ("index.js", include_str!("../whalito-dsh-settings/index.js")),
            ("client.js", include_str!("../whalito-dsh-settings/client.js")),
        ],
    },
    PkgSpec {
        id: "dsh-webui-plus",
        name: "@entireyu/dsh-webui-plus",
        files: &[
            ("package.json", include_str!("../dsh-webui-plus/package.json")),
            ("index.js", include_str!("../dsh-webui-plus/index.js")),
            ("client.js", include_str!("../dsh-webui-plus/client.js")),
        ],
    },
];

/// cordis.patch.yml 中的托管标记：同步只替换/追加这两个标记之间的块。
const MARK_BEGIN: &str = "# ⟪ whalito-managed begin ⟫";
const MARK_END: &str = "# ⟪ whalito-managed end ⟫";
/// 旧版（≤0.4.6 测试构建）写入时的损坏标记：源码 `⟪`（U+27EA）在旧写入
/// 链路里变成 `隡`（U+96A1）。新版同步时识别并统一替换为标准标记，避免
/// 出现两个标记块（重复 insert 同一插件）。
const LEGACY_MARK_BEGIN: &str = "# 隡 whalito-managed begin 隡";
const LEGACY_MARK_END: &str = "# 隡 whalito-managed end 隡";

/// 同步结果（installed=已就绪无变化 / updated=有写入 / skipped=跳过）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PluginSyncReport {
    pub status: String,
    pub detail: String,
    pub profile_dir: String,
}

/// DSH 家目录。测试构建固定使用隔离的 ~/.dsh-test（忽略外部 DSH_HOME，
/// 避免误指生产数据）；生产构建 DSH_HOME 环境变量优先，否则 ~/.dsh。
pub fn dsh_home() -> PathBuf {
    let base = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    if crate::state::TEST_BUILD {
        return PathBuf::from(base).join(".dsh-test");
    }
    if let Ok(h) = std::env::var("DSH_HOME") {
        if !h.trim().is_empty() {
            return PathBuf::from(h);
        }
    }
    PathBuf::from(base).join(".dsh")
}

/// web profile 目录（鲸仔只启动 dsh web 这一个 profile）。
pub fn profile_dir() -> PathBuf {
    dsh_home().join("profiles").join("web")
}

/// 生成托管的 cordis.patch.yml 标记块：每个插件包一行 insert。
fn managed_block() -> String {
    let mut lines = String::from(MARK_BEGIN);
    for pkg in PKGS {
        lines.push_str(&format!(
            "\n- insert:\n    - id: {}\n      name: '{}'",
            pkg.id, pkg.name
        ));
    }
    lines.push('\n');
    lines.push_str(MARK_END);
    lines.push('\n');
    lines
}

/// 判断现有补丁层是否为"仅空列表"（新 profile 默认内容：注释 + `[]`）。
/// 这种文件不能直接在 `[]` 后追加 block 条目（YAML 会把 flow 序列
/// 之后的内容判为语法错误），必须把 `[]` 就地替换为块。
fn bare_empty_list(existing: &str) -> bool {
    let mut items: Vec<&str> = Vec::new();
    for raw in existing.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        items.push(line);
    }
    items.len() == 1 && items[0] == "[]"
}

/// 行级结构：`- insert:` 块 = 顶格 `- insert:` + 缩进的 `- id:` + `name:`。
/// 判断某 `- insert:` 行是否构成与托管插件完全一致的单一条目。
/// 要求该块恰好三行结束（下一行不存在/空行/顶格），避免误删带延续行的块。
fn is_managed_dup_insert(lines: &[&str], at: usize) -> bool {
    let Some(first) = lines.get(at) else { return false };
    if first.trim() != "- insert:" {
        return false;
    }
    // 下一行必须是 `    - id: <pkg.id>`，再下一行 `      name: '<pkg.name>'`。
    let (Some(id_line), Some(name_line)) = (lines.get(at + 1), lines.get(at + 2)) else {
        return false;
    };
    let id_trim = id_line.trim();
    let name_trim = name_line.trim();
    if !id_trim.starts_with("- id:") || !name_trim.starts_with("name:") {
        return false;
    }
    let id_val = id_trim["- id:".len()..].trim();
    let name_val = name_trim["name:".len()..].trim().trim_matches('\'');
    let matched = PKGS
        .iter()
        .any(|p| p.id == id_val && p.name == name_val);
    if !matched {
        return false;
    }
    // 块必须恰好三行：at+3 不存在、空行、或以非空白开头（下一个顶格条目）。
    match lines.get(at + 3) {
        None => true,
        Some(next) => {
            let t = next.trim();
            t.is_empty() || !t.starts_with('-') && !t.starts_with("  ") && !t.starts_with('\t')
        }
    }
}

/// 清理标记块外与托管插件完全一致的重复 insert 条目。
/// 升级场景：旧版本把插件条目写在标记块外（用户手动安装 / 旧版同步残留），
/// 新版本又通过标记块托管同一插件 → 同一插件被 insert 两次，DSH 加载时报错
/// 或后加载覆盖前加载（表现为"插件配置被覆盖/消失"）。这里把标记块外与
/// 托管插件 id+name 完全相同的条目删除（标记块内的托管版本是权威）。
/// 只精确匹配托管插件自身，绝不触碰用户安装的其他第三方插件。
fn dedupe_managed_dups(existing: &str) -> String {
    let lines: Vec<&str> = existing.lines().collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut i = 0;
    // 标记块外 = 不在 MARK_BEGIN..=MARK_END 之间的行。
    let begin = lines.iter().position(|l| l.trim() == MARK_BEGIN);
    let end = lines.iter().position(|l| l.trim() == MARK_END);
    while i < lines.len() {
        let in_block = match (begin, end) {
            (Some(b), Some(e)) if b <= i && i <= e => true,
            _ => false,
        };
        if !in_block && is_managed_dup_insert(&lines, i) {
            i += 3;
            continue;
        }
        out.push(lines[i]);
        i += 1;
    }
    out.join("\n")
}

/// 在标记块内 upsert 插件 insert 行：两个标记都存在则整块替换，
/// 否则追加——若文件是"仅空列表"则把 `[]` 就地替换为块（避免
/// flow 序列后接 block 条目的 YAML 语法错误）。标记块外内容原样保留。
///
/// 兼容旧版损坏标记：旧测试构建把 `⟪` 写成了 `隡`，先把它们规范化为
/// 标准标记再处理，保证新旧版本间幂等、不产生重复标记块。
/// 最后清理标记块外与托管插件重复的 insert 条目（升级残留去重）。
pub fn upsert_marker_block(existing: &str) -> String {
    let normalized = existing.replace(LEGACY_MARK_BEGIN, MARK_BEGIN).replace(LEGACY_MARK_END, MARK_END);
    let existing = &normalized;
    let merged = match (existing.find(MARK_BEGIN), existing.find(MARK_END)) {
        (Some(begin), Some(end)) if end > begin => {
            let tail_start = match existing[end..].find('\n') {
                Some(i) => end + i + 1,
                None => existing.len(),
            };
            let mut out = String::with_capacity(existing.len() + managed_block().len());
            out.push_str(&existing[..begin]);
            out.push_str(&managed_block());
            out.push_str(&existing[tail_start..]);
            out
        }
        _ => {
            let mut out = existing.to_string();
            if !out.ends_with('\n') {
                out.push('\n');
            }
            if bare_empty_list(existing) {
                if let Some(pos) = out.find("[]") {
                    out.replace_range(pos..pos + 2, &managed_block());
                    return out;
                }
            }
            out.push_str(&managed_block());
            out
        }
    };
    // 去重后再补一个结尾换行（lines/join 会吃掉末尾 \n）。
    let deduped = dedupe_managed_dups(&merged);
    if deduped.ends_with('\n') {
        deduped
    } else {
        format!("{deduped}\n")
    }
}

/// 从 package.json 内容解析版本号（供"磁盘版本 ≥ 内置版本则跳过"比较）。
fn pkg_version(package_json: &str) -> Option<(u64, u64, u64)> {
    let v: serde_json::Value = serde_json::from_str(package_json).ok()?;
    let ver = v.get("version")?.as_str()?;
    crate::state::parse_semver(ver)
}

/// 把内置插件文件写入 profile 的 node_modules；按内容比对，返回是否有写入。
/// 版本感知：磁盘上已存在版本 ≥ 内置版本的包 → 整体跳过（尊重用户手动安装/
/// 升级的插件，升级鲸仔不降级覆盖用户内容）；仅当磁盘版本更低或未安装时才
/// 写入内置版本（保证鲸仔托管的新版本仍能推送）。
pub fn sync_package_files(profile: &Path) -> Result<bool, String> {
    let mut changed = false;
    for pkg in PKGS {
        let pkg_dir = profile.join("node_modules").join(pkg.name);
        // 内置 package.json 即 files 中的第一项；磁盘版本与之比较。
        let builtin_json = pkg
            .files
            .iter()
            .find(|(n, _)| *n == "package.json")
            .map(|(_, c)| *c);
        let disk_pkg_path = pkg_dir.join("package.json");
        let disk_json = fs::read_to_string(&disk_pkg_path).ok();
        let builtin_ver = builtin_json.and_then(pkg_version);
        let disk_ver = disk_json.as_deref().and_then(pkg_version);
        if let (Some(b), Some(d)) = (builtin_ver, disk_ver) {
            if d >= b {
                // 用户安装的版本不低于内置：不覆盖，避免"已安装的插件被还原"。
                continue;
            }
        }
        for (name, content) in pkg.files {
            let path = pkg_dir.join(name);
            let same = fs::read_to_string(&path)
                .map(|c| c == *content)
                .unwrap_or(false);
            if same {
                continue;
            }
            fs::create_dir_all(&pkg_dir)
                .map_err(|e| format!("创建插件目录 {} 失败：{e}", pkg_dir.display()))?;
            fs::write(&path, *content)
                .map_err(|e| format!("写入插件文件 {} 失败：{e}", path.display()))?;
            changed = true;
        }
    }
    Ok(changed)
}

/// 维护 cordis.patch.yml 标记块；返回是否有写入。
/// 显式以 UTF-8 编码写入，杜绝旧版把 `⟪` 写成 `隡` 的编码损坏。
pub fn sync_patch_layer(profile: &Path) -> Result<bool, String> {
    let path = profile.join("cordis.patch.yml");
    let current = if path.exists() {
        fs::read_to_string(&path).map_err(|e| format!("读取 cordis.patch.yml 失败：{e}"))?
    } else {
        // 文件不存在：直接写标记块（不要先写 `[]` 再追加，避免中间态语法错误）。
        fs::write(&path, managed_block()).map_err(|e| format!("写入 cordis.patch.yml 失败：{e}"))?;
        return Ok(true);
    };
    let updated = upsert_marker_block(&current);
    if updated == current {
        return Ok(false);
    }
    fs::write(&path, updated).map_err(|e| format!("写入 cordis.patch.yml 失败：{e}"))?;
    Ok(true)
}

/// 幂等同步：写入插件文件 + 维护标记块，并写入应用日志。
/// 错误会先记日志再返回（调用方可决定是否继续阻断）。
pub fn ensure_settings_plugin(app: &AppHandle) -> Result<PluginSyncReport, String> {
    let profile = profile_dir();
    let profile_str = profile.to_string_lossy().into_owned();
    if !profile.exists() {
        // dsh 首次启动会补全 package.json / cordis.yml；这里先创建目录并预置
        // 插件包与补丁层，让首次启动即装载"鲸仔"分区（无需二次重启）。
        fs::create_dir_all(&profile)
            .map_err(|e| format!("创建 profile 目录 {} 失败：{e}", profile.display()))?;
    }
    match (|| -> Result<PluginSyncReport, String> {
        let files_changed = sync_package_files(&profile)?;
        let patch_changed = sync_patch_layer(&profile)?;
        let status = if files_changed || patch_changed { "updated" } else { "installed" };
        let names: Vec<String> = PKGS.iter().map(|p| p.name.to_string()).collect();
        Ok(PluginSyncReport {
            status: status.into(),
            detail: format!("插件包已就绪（{}）", names.join("、")),
            profile_dir: profile_str.clone(),
        })
    })() {
        Ok(report) => {
            push_log(
                &app.state::<AppState>().logs,
                &format!("[系统] 鲸仔设置分区插件：{}（{profile_str}）", report.detail),
            );
            Ok(report)
        }
        Err(e) => {
            push_log(
                &app.state::<AppState>().logs,
                &format!("[系统] 鲸仔设置分区插件同步失败：{e}"),
            );
            Err(e)
        }
    }
}

/// 供面板手动触发重同步/排障的命令。
#[tauri::command]
pub fn sync_settings_plugin(app: AppHandle) -> Result<PluginSyncReport, String> {
    ensure_settings_plugin(&app)
}

/// 诊断辅助：把鲸仔桥事件追加写入 %TEMP%\whalito-bridge.log，
/// 供排障时直接查看（内嵌页 postMessage 链路两侧的行为都落到这里）。
#[tauri::command]
pub fn bridge_diag(line: String) {
    use std::io::Write;
    let path = std::env::temp_dir().join("whalito-bridge.log");
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{secs}] {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_has_all_pkgs(block: &str) -> bool {
        for pkg in PKGS {
            if !block.contains(&format!("name: '{}'", pkg.name)) {
                return false;
            }
        }
        true
    }

    #[test]
    fn appends_block_when_markers_absent() {
        let input = "# 用户自己的注释\n- id: some-other\n  name: 'x'\n";
        let out = upsert_marker_block(input);
        assert!(out.starts_with(input));
        assert!(out.contains(MARK_BEGIN));
        assert!(out.contains(MARK_END));
        assert!(block_has_all_pkgs(&out));
        // 每个包都有对应 id 行（name 去掉 scope 前缀）。
        assert!(out.contains("id: whalito-settings"));
        assert!(out.contains("id: dsh-webui-plus"));
    }

    #[test]
    fn replaces_existing_managed_block_only() {
        let stale = format!(
            "{}\n- insert:\n    - id: whalito-settings\n      name: 'stale'\n{}\n",
            MARK_BEGIN, MARK_END
        );
        let input = format!("# 前文\n{stale}# 后文\n");
        let out = upsert_marker_block(&input);
        assert!(out.starts_with("# 前文\n"));
        assert!(out.ends_with("# 后文\n"));
        assert!(!out.contains("name: 'stale'"));
        assert!(block_has_all_pkgs(&out));
        assert_eq!(out.matches(MARK_BEGIN).count(), 1);
        assert_eq!(out.matches(MARK_END).count(), 1);
    }

    #[test]
    fn converts_bare_empty_list_to_block() {
        let input = "# Your patch layer for this dsh profile.\n[]\n";
        let out = upsert_marker_block(input);
        assert!(!out.contains("[]"));
        assert!(out.contains(MARK_BEGIN));
        assert!(out.contains("- insert:"));
        assert!(block_has_all_pkgs(&out));
        assert!(out.starts_with("# Your patch layer"));
    }

    #[test]
    fn appends_after_user_block_items() {
        let input = "- id: user-row\n  name: 'user-pkg'\n";
        let out = upsert_marker_block(input);
        assert!(out.starts_with(input));
        assert!(out.contains(MARK_BEGIN));
        assert!(block_has_all_pkgs(&out));
    }

    #[test]
    fn is_idempotent() {
        let input = "# 前\n- id: a\n  name: 'b'\n";
        let once = upsert_marker_block(input);
        let twice = upsert_marker_block(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn normalizes_legacy_corrupted_marker() {
        // 旧版测试构建把 `⟪` 写成 `隡`：同步时应整体替换为标准标记块，
        // 且不残留旧标记（避免两个标记块重复 insert）。
        let legacy = format!(
            "# 隡 whalito-managed begin 隡\n- insert:\n    - id: whalito-settings\n      name: 'old'\n# 隡 whalito-managed end 隡\n"
        );
        let out = upsert_marker_block(&legacy);
        assert!(!out.contains('隡'), "旧损坏标记应被清除：{out}");
        assert_eq!(out.matches(MARK_BEGIN).count(), 1);
        assert_eq!(out.matches(MARK_END).count(), 1);
        assert!(block_has_all_pkgs(&out));
        assert!(!out.contains("name: 'old'"));
        // 幂等：二次处理结果不变。
        assert_eq!(upsert_marker_block(&out), out);
    }

    #[test]
    fn sync_package_files_roundtrip() {
        let base = std::env::temp_dir().join(format!("whalito-sync-test-{}", std::process::id()));
        let profile = base.join("profiles").join("web");
        let _ = fs::remove_dir_all(&base);
        assert!(sync_package_files(&profile).unwrap());
        assert!(!sync_package_files(&profile).unwrap());
        for pkg in PKGS {
            for (name, content) in pkg.files {
                let path = profile.join("node_modules").join(pkg.name).join(name);
                assert_eq!(fs::read_to_string(&path).unwrap(), *content);
            }
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn sync_package_files_respects_newer_installed_version() {
        // 磁盘上已存在更高版本（用户手动安装/升级过）→ 同步不降级覆盖。
        let base = std::env::temp_dir().join(format!("whalito-ver-test-{}", std::process::id()));
        let profile = base.join("profiles").join("web");
        let _ = fs::remove_dir_all(&base);
        let pkg_dir = profile.join("node_modules").join(PKGS[1].name);
        fs::create_dir_all(&pkg_dir).unwrap();
        // 写入一个版本比内置更高的 package.json（如 0.2.0）与"用户自定义"的 client.js。
        let newer = r#"{"name":"@entireyu/dsh-webui-plus","version":"0.2.0"}"#;
        fs::write(pkg_dir.join("package.json"), newer).unwrap();
        let custom = "// 用户自定义内容\n";
        fs::write(pkg_dir.join("client.js"), custom).unwrap();
        // 首次同步：whalito-settings 未安装会写入（整体 changed=true），
        // 但 dsh-webui-plus 版本更高必须跳过覆盖。
        sync_package_files(&profile).unwrap();
        assert_eq!(
            fs::read_to_string(pkg_dir.join("package.json")).unwrap(),
            newer,
            "版本更高的 package.json 不应被降级覆盖"
        );
        assert_eq!(
            fs::read_to_string(pkg_dir.join("client.js")).unwrap(),
            custom,
            "用户自定义 client.js 不应被覆盖"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn sync_package_files_upgrades_older_version() {
        // 磁盘版本低于内置（旧鲸仔装的）→ 正常覆盖为内置版本。
        let base = std::env::temp_dir().join(format!("whalito-upg-test-{}", std::process::id()));
        let profile = base.join("profiles").join("web");
        let _ = fs::remove_dir_all(&base);
        let pkg_dir = profile.join("node_modules").join(PKGS[1].name);
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("package.json"), r#"{"name":"@entireyu/dsh-webui-plus","version":"0.0.9"}"#).unwrap();
        assert!(sync_package_files(&profile).unwrap());
        let written = fs::read_to_string(pkg_dir.join("package.json")).unwrap();
        assert!(written.contains("\"0.1.2\""), "应升级为内置版本：{written}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn dedupes_duplicate_managed_insert_outside_block() {
        // 升级残留：标记块外还有一条与托管插件完全一致的 insert（用户旧版手动
        // 安装或旧同步残留）→ 应被删除，避免同一插件被 insert 两次。
        let dup = format!(
            "{MARK_BEGIN}\n\
             - insert:\n    - id: whalito-settings\n      name: '@entireyu/whalito-dsh-settings'\n\
             - insert:\n    - id: dsh-webui-plus\n      name: '@entireyu/dsh-webui-plus'\n\
             {MARK_END}\n\
             - insert:\n    - id: dsh-webui-plus\n      name: '@entireyu/dsh-webui-plus'\n"
        );
        let out = upsert_marker_block(&dup);
        // 标记块外的重复条目被清掉；标记块内保留一份。
        assert_eq!(out.matches("name: '@entireyu/dsh-webui-plus'").count(), 1);
        assert_eq!(out.matches("name: '@entireyu/whalito-dsh-settings'").count(), 1);
        assert!(block_has_all_pkgs(&out));
    }

    #[test]
    fn keeps_user_third_party_inserts_outside_block() {
        // 标记块外用户安装的其他第三方插件条目（与托管无关）必须保留。
        let input = format!(
            "{MARK_BEGIN}\n- insert:\n    - id: whalito-settings\n      name: '@entireyu/whalito-dsh-settings'\n{MARK_END}\n\
             - insert:\n    - id: my-user-plugin\n      name: 'some-user-plugin'\n"
        );
        let out = upsert_marker_block(&input);
        assert!(out.contains("id: my-user-plugin"));
        assert!(out.contains("name: 'some-user-plugin'"));
        assert!(block_has_all_pkgs(&out));
    }

    #[test]
    fn dedupe_is_idempotent_with_dups() {
        let dup = format!(
            "{MARK_BEGIN}\n- insert:\n    - id: dsh-webui-plus\n      name: '@entireyu/dsh-webui-plus'\n{MARK_END}\n\
             - insert:\n    - id: dsh-webui-plus\n      name: '@entireyu/dsh-webui-plus'\n"
        );
        let once = upsert_marker_block(&dup);
        let twice = upsert_marker_block(&once);
        assert_eq!(once, twice);
        assert_eq!(once.matches(MARK_BEGIN).count(), 1);
        assert_eq!(once.matches(MARK_END).count(), 1);
    }
}
