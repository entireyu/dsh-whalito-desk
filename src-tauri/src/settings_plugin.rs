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
    /// 鲸仔独占托管的包（private、用户无法通过任何途径手动安装/升级）：
    /// 同步按**内容比对**覆盖，不依赖版本号——避免「改了内嵌内容但忘了升
    /// package.json 版本」时，版本感知（磁盘 ≥ 内置则跳过）导致改动永远
    /// 推不出去的坑。对外可安装的包（npm 发布）必须保持版本感知。
    pub force_sync: bool,
}

/// 内置插件包清单：鲸仔设置分区 + WebUI+ 增强。数组顺序即 patch.yml 中
/// insert 行的顺序；修改文件内容无需改这里（include_str! 编译期自动跟随）。
/// 注意：**改了 whalito-settings 的内容后仍应同步提升其 package.json 版本**
/// （版本是用户可见/可诊断的推送信号）；force_sync 只是兜底，不是偷懒借口。
pub const PKGS: &[PkgSpec] = &[
    PkgSpec {
        id: "whalito-settings",
        name: "@entireyu/whalito-dsh-settings",
        files: &[
            ("package.json", include_str!("../whalito-dsh-settings/package.json")),
            ("index.js", include_str!("../whalito-dsh-settings/index.js")),
            ("client.js", include_str!("../whalito-dsh-settings/client.js")),
        ],
        force_sync: true,
    },
    PkgSpec {
        id: "dsh-webui-plus",
        name: "@entireyu/dsh-webui-plus",
        files: &[
            ("package.json", include_str!("../dsh-webui-plus/package.json")),
            ("index.js", include_str!("../dsh-webui-plus/index.js")),
            ("client.js", include_str!("../dsh-webui-plus/client.js")),
        ],
        force_sync: false,
    },
];

/// 可禁用的内置插件 id 清单（patch 层 `- id: xxx` + `disabled: true` 覆盖行）。
/// - dsh-webui-plus：鲸仔托管（patch 层 insert），禁用 = 标记块生成 disabled 覆盖行
/// - dsh-market：插件市场（bundle 层，dsh plugin 安装），禁用 = 同样覆盖行
/// whalito-settings 不在其中：禁用它即失去「鲸仔设置」入口，永不提供禁用。
pub const DISABLEABLE_IDS: &[&str] = &["dsh-webui-plus", "dsh-market"];

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

/// —— DSH 配置备份（安装/更新 DSH 前调用）——
/// 备份 DSH 家目录下的配置（settings.yaml、凭证、profiles 等，排除可重建的
/// node_modules 与体积大的会话/附件数据）以及应用专用 npm 前缀下用户手动安装
/// 的第三方插件（排除 DSH 本体）。备份存到鲸仔应用配置目录 dsh-backup 下，
/// 保留最近 MAX_BACKUPS 份，旧的自动清理。best-effort：失败不阻断安装。
const MAX_BACKUPS: usize = 5;

/// 递归复制目录，跳过指定名称的子项（不区分文件/目录）。
fn copy_dir_recursive(src: &Path, dst: &Path, skip_names: &[&str]) -> Result<usize, String> {
    let mut count = 0;
    for entry in fs::read_dir(src).map_err(|e| format!("读取 {} 失败：{e}", src.display()))? {
        let entry = entry.map_err(|e| format!("读取目录项失败：{e}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if skip_names.contains(&name.as_str()) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry
            .file_type()
            .map_err(|e| format!("读取类型失败：{e}"))?
            .is_dir()
        {
            fs::create_dir_all(&to)
                .map_err(|e| format!("创建 {} 失败：{e}", to.display()))?;
            count += copy_dir_recursive(&from, &to, skip_names)?;
        } else {
            fs::copy(&from, &to).map_err(|e| format!("复制 {} 失败：{e}", from.display()))?;
            count += 1;
        }
    }
    Ok(count)
}

/// 清理旧备份：保留最近 MAX_BACKUPS 份（按目录名排序，时间戳前缀可字典序）。
fn prune_backups(backup_root: &Path) -> Result<(), String> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(backup_root)
        .map_err(|e| format!("读取备份目录 {} 失败：{e}", backup_root.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("dsh-backup-"))
        })
        .collect();
    dirs.sort();
    while dirs.len() > MAX_BACKUPS {
        if let Some(oldest) = dirs.first() {
            fs::remove_dir_all(oldest)
                .map_err(|e| format!("清理旧备份 {} 失败：{e}", oldest.display()))?;
        }
        dirs.remove(0);
    }
    Ok(())
}

/// 备份 DSH 配置与应用前缀下的用户插件；返回备份目录路径（无可备份时返回 None）。
/// 备份内容：
///   - DSH 家目录顶层配置文件（settings.yaml / .credentials.yaml / 匿名 id 等）
///   - profiles 目录（排除 node_modules——依赖可重建，且体积大头在会话/附件）
///   - .agent-presets（如存在）
///   - 应用专用 npm 前缀下用户手动安装的第三方插件（排除 DSH 本体 @deepseek-ai/dsh）
pub fn backup_dsh_config(app: &AppHandle) -> Result<Option<PathBuf>, String> {
    let home = dsh_home();
    let backup_root = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("获取应用配置目录失败：{e}"))?
        .join("dsh-backup");
    let prefix = crate::state::app_prefix_dir();
    backup_dsh_config_at(&home, &prefix, &backup_root)
}

/// 备份实现（独立纯函数，便于单元测试）：把 home 下的配置与 prefix 下的用户
/// 插件复制到 backup_root/dsh-backup-<unix秒>，保留最近 MAX_BACKUPS 份。
fn backup_dsh_config_at(
    home: &Path,
    prefix: &Path,
    backup_root: &Path,
) -> Result<Option<PathBuf>, String> {
    fs::create_dir_all(backup_root)
        .map_err(|e| format!("创建备份目录 {} 失败：{e}", backup_root.display()))?;

    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // 毫秒级命名避免同一秒内同名；极端场景（同毫秒连续两次备份，CI 快速循环
    // 可能触发）追加序号保证目录唯一，否则同名目录会被合并、剪枝计数出错。
    let base_name = format!("dsh-backup-{millis}");
    let mut dir = backup_root.join(&base_name);
    let mut seq = 2u32;
    while dir.exists() {
        dir = backup_root.join(format!("{base_name}-{seq}"));
        seq += 1;
    }
    fs::create_dir_all(&dir).map_err(|e| format!("创建备份目录 {} 失败：{e}", dir.display()))?;
    let mut copied = 0usize;

    // 1. DSH 家目录顶层配置文件。
    for name in [
        "settings.yaml",
        ".credentials.yaml",
        ".anonymous-user-id",
        "pet-style.json",
    ] {
        let src = home.join(name);
        if src.is_file() {
            fs::copy(&src, dir.join(name))
                .map_err(|e| format!("复制 {} 失败：{e}", src.display()))?;
            copied += 1;
        }
    }

    // 2. profiles（排除 node_modules）。
    let profiles_src = home.join("profiles");
    if profiles_src.is_dir() {
        let dst = dir.join("profiles");
        fs::create_dir_all(&dst).map_err(|e| format!("创建 {} 失败：{e}", dst.display()))?;
        copied += copy_dir_recursive(&profiles_src, &dst, &["node_modules"])?;
    }

    // 3. agent presets。
    let presets_src = home.join(".agent-presets");
    if presets_src.is_dir() {
        copied += copy_dir_recursive(&presets_src, &dir.join(".agent-presets"), &[])?;
    }

    // 4. 应用专用 npm 前缀下的用户插件（排除 DSH 本体与 @deepseek-ai 依赖）。
    let user_nm = prefix.join("node_modules");
    if user_nm.is_dir() {
        let dst = dir.join("user-plugins");
        fs::create_dir_all(&dst).map_err(|e| format!("创建 {} 失败：{e}", dst.display()))?;
        for entry in fs::read_dir(&user_nm)
            .map_err(|e| format!("读取 {} 失败：{e}", user_nm.display()))?
        {
            let entry = entry.map_err(|e| format!("读取目录项失败：{e}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "@deepseek-ai" {
                // 只跳过 DSH 本体，@deepseek-ai 下其他用户插件仍备份。
                let dsh_sub = entry.path().join("dsh");
                if dsh_sub.is_dir() {
                    continue;
                }
            }
            let to = dst.join(&name);
            if entry
                .file_type()
                .map_err(|e| format!("读取类型失败：{e}"))?
                .is_dir()
            {
                fs::create_dir_all(&to)
                    .map_err(|e| format!("创建 {} 失败：{e}", to.display()))?;
                copied += copy_dir_recursive(&entry.path(), &to, &[])?;
            } else {
                fs::copy(entry.path(), &to)
                    .map_err(|e| format!("复制 {} 失败：{e}", entry.path().display()))?;
                copied += 1;
            }
        }
    }

    // 清理旧备份：失败不阻断本次备份（best-effort，下次安装再试）。
    let _ = prune_backups(backup_root);
    if copied == 0 {
        // 没有可备份内容：删除刚建的空目录。
        let _ = fs::remove_dir_all(&dir);
        return Ok(None);
    }
    Ok(Some(dir))
}

/// 读取 profile 的 `dsh.profile.bundles`（dsh plugin 维护的层列表，权威的
/// 「bundle 层已装插件」来源）。文件缺失/解析失败返回空列表。
pub fn installed_bundles(profile: &Path) -> Vec<String> {
    let path = profile.join("package.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    v.get("dsh")
        .and_then(|d| d.get("profile"))
        .and_then(|p| p.get("bundles"))
        .and_then(|b| b.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// 生成托管的 cordis.patch.yml 标记块：
/// - 未禁用的托管插件：一行 insert；
/// - 已禁用的托管插件：**insert 行保留 + 追加 `- id: xxx` + `disabled: true` 覆盖行**
///   （cordis patch 顶层条目是「按 id 覆盖已有行」，没有 insert 创建的行就匹配
///   不到 → 禁用无效且 loader 警告；insert 创建行后覆盖行才能禁用）；
/// - 不在 PKGS 中的可禁用插件（dsh-market：bundle 层行已存在）→ 只需覆盖行；
/// - R1：PKGS 同名包已在 bundle 层**真正激活**（bundles 列出且包声明 dsh.bundle，
///   由调用方算出 active_bundles）→ 跳过 insert，避免同一插件两层各加载一次。
fn managed_block(disabled_ids: &[&str], active_bundles: &[String]) -> String {
    // 常量形态：extras = DISABLEABLE_IDS 中不属于 PKGS 的（dsh-market 等）。
    let const_extras: Vec<String> = DISABLEABLE_IDS
        .iter()
        .filter(|id| !PKGS.iter().any(|p| p.id == **id))
        .map(|s| s.to_string())
        .collect();
    managed_block_with(disabled_ids, active_bundles, &const_extras)
}

/// managed_block 的完整形态：extra_ids = 可禁用的「非 PKGS」loader 条目 id
/// 池（常量 + bundle 层动态条目），被禁用时只输出覆盖行（行由包自身的
/// 补丁/insert 创建，无需鲸仔 insert）。
fn managed_block_with(
    disabled_ids: &[&str],
    active_bundles: &[String],
    extra_ids: &[String],
) -> String {
    let mut lines = String::from(MARK_BEGIN);
    for pkg in PKGS {
        if active_bundles.iter().any(|b| b == pkg.name) {
            // bundle 层已激活（用户 dsh plugin 安装的同名 bundle 包），标记块不重复 insert。
            continue;
        }
        lines.push_str(&format!(
            "\n- insert:\n    - id: {}\n      name: '{}'",
            pkg.id, pkg.name
        ));
        if disabled_ids.contains(&pkg.id) {
            lines.push_str(&format!("\n- id: {}\n  disabled: true", pkg.id));
        }
    }
    // 不在 PKGS 中的可禁用插件（dsh-market、用户经插件市场装的 bundle 层
    // 插件）：loader 行由包自身 insert，标记块只输出覆盖行。
    for extra in extra_ids {
        if disabled_ids.contains(&extra.as_str()) {
            lines.push_str(&format!("\n- id: {extra}\n  disabled: true"));
        }
    }
    lines.push('\n');
    lines.push_str(MARK_END);
    lines.push('\n');
    lines
}

/// bundle 层是否真的激活了该包：bundles 列出**且**包声明 `dsh.bundle`。
/// 普通依赖（只有 dsh.client 或没有 dsh 声明）不会在 bundle 层 insert 任何行，
/// 与标记块托管不冲突，不能据此跳过鲸仔的 insert（否则 WebUI+ 会凭空消失）。
fn bundle_layer_active(profile: &Path, name: &str) -> bool {
    let pkg_json = profile.join("node_modules").join(name).join("package.json");
    let Ok(text) = fs::read_to_string(&pkg_json) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    v.get("dsh").and_then(|d| d.get("bundle")).is_some()
}

/// 计算「bundle 层真正激活」的包名集合（供 managed_block 的 R1 判定）。
fn active_bundles(profile: &Path) -> Vec<String> {
    installed_bundles(profile)
        .into_iter()
        .filter(|b| bundle_layer_active(profile, b))
        .collect()
}

/// 内核 bundle（DSH 本体组成，绝不提供禁用/卸载入口）：
/// `@deepseek-ai/*` 官方 scope 的 bundle 都是运行时/基础 UI（dsh-base、
/// dsh-web-app 等）；插件市场装的第三方插件一般无 scope 或属作者 scope。
fn is_core_bundle_name(name: &str) -> bool {
    name.starts_with("@deepseek-ai/")
}

/// 从 bundle 包的补丁文件提取 loader 条目 id（禁用覆盖行按 id 定向）。
/// 兼容两种行形态：顶层 `- id: x`（列表条目）与 `- insert:` 的子行
/// （缩进 2/4 空格）。更深层的配置字段（如 entry 属性里嵌套的 `id:`）
/// 不会以列表项缩进出现，忽略；带引号的 id 一并剥壳。
fn patch_loader_ids(patch_text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in patch_text.lines() {
        let t = raw.trim();
        if t.starts_with("- id:") && raw.len() - raw.trim_start().len() <= 4 {
            let v = t["- id:".len()..]
                .trim()
                .trim_matches('\'')
                .trim_matches('"')
                .to_string();
            if !v.is_empty() && !out.contains(&v) {
                out.push(v);
            }
        }
    }
    out
}

/// 一个可禁用的 bundle 层 loader 条目（id → 所属包）。
struct BundleDisableEntry {
    id: String,
    pkg: String,
    description: String,
}

/// 枚举 bundle 层可禁用条目：profile `dsh.profile.bundles` 中已激活、非内核
/// （@deepseek-ai/*）、非鲸仔托管 PKGS 的包，从其补丁文件（package.json
/// `dsh.bundle.patch`，默认 ./cordis.patch.yml）提取 loader 条目 id。
/// 这类包正是「插件市场安装、可能在 loader 层打断 DSH 启动」的插件
/// （如 dshmarket 1.29.2 事件）；禁用 = 标记块写 disabled 覆盖行，重启生效。
fn bundle_disable_entries(profile: &Path) -> Vec<BundleDisableEntry> {
    let mut out = Vec::new();
    for bundle in active_bundles(profile) {
        if is_core_bundle_name(&bundle) || PKGS.iter().any(|p| p.name == bundle) {
            continue;
        }
        let pkg_dir = profile.join("node_modules").join(&bundle);
        let Ok(text) = fs::read_to_string(pkg_dir.join("package.json")) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let description = v
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        let patch_rel = v
            .get("dsh")
            .and_then(|d| d.get("bundle"))
            .and_then(|b| b.get("patch"))
            .and_then(|p| p.as_str())
            .unwrap_or("cordis.patch.yml")
            .trim_start_matches("./");
        let Ok(patch) = fs::read_to_string(pkg_dir.join(patch_rel)) else {
            continue;
        };
        for id in patch_loader_ids(&patch) {
            out.push(BundleDisableEntry {
                id,
                pkg: bundle.clone(),
                description: description.clone(),
            });
        }
    }
    out
}

/// 可禁用的全部 id 池：鲸仔托管 PKGS（whalito-settings 除外，见 toggle 门禁）
/// + 常量可禁用（dsh-market 未装时的兼容保留）+ bundle 层可禁用条目。
/// 供禁用解析/权限判定使用；内容随 profile 现状动态变化。
fn disableable_id_pool(profile: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in PKGS {
        if !out.contains(&p.id.to_string()) {
            out.push(p.id.to_string());
        }
    }
    for extra in DISABLEABLE_IDS {
        if !out.iter().any(|x| x == extra) {
            out.push(extra.to_string());
        }
    }
    for entry in bundle_disable_entries(profile) {
        if !out.contains(&entry.id) {
            out.push(entry.id);
        }
    }
    out
}

/// 解析现有 patch 中的禁用条目（顶格 `- id: <可禁用 id>` + 下一行 `disabled: true`）。
/// 只认鲸仔可管理的 id；insert 块里的缩进 `- id:` 行不会误判。
/// 常量形态：allowed = PKGS 全部 id + DISABLEABLE_IDS（兼容旧调用与测试）。
fn parse_disabled_ids(existing: &str) -> Vec<String> {
    let mut allowed: Vec<String> = PKGS.iter().map(|p| p.id.to_string()).collect();
    for extra in DISABLEABLE_IDS {
        if !allowed.iter().any(|x| x == extra) {
            allowed.push(extra.to_string());
        }
    }
    parse_disabled_ids_with(existing, &allowed)
}

/// 完整形态：allowed = 动态可禁用 id 池（含 bundle 层条目，见 disableable_id_pool）。
fn parse_disabled_ids_with(existing: &str, allowed: &[String]) -> Vec<String> {
    let lines: Vec<&str> = existing.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i + 1 < lines.len() {
        let t = lines[i].trim();
        if t.starts_with("- id:") {
            // 顶格条目（insert 块的子行带缩进，不匹配）。
            let raw = t["- id:".len()..].trim();
            let id_val = raw.trim_matches('\'').trim_matches('"').to_string();
            let manageable = allowed.iter().any(|x| x == &id_val);
            if manageable && lines[i + 1].trim().starts_with("disabled: true") {
                if !out.contains(&id_val) {
                    out.push(id_val);
                }
            }
        }
        i += 1;
    }
    out
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

/// 行级结构：`- insert:` 块 = 顶格 `- insert:` + 缩进的 `- id:`（可带引号），
/// 通常还有 `name:` 行。判断某 `- insert:` 行是否构成指向托管插件 id 的条目。
/// 只按 `- id:` 判定（id 是插件唯一标识，重复 insert 同一 id 即冲突），
/// name 写法（单/双引号、有无）不影响判定；块必须恰好 2~3 行结束
/// （下一行不存在/空行/顶格），避免误删带多个条目的 insert 块。
fn is_managed_dup_insert(lines: &[&str], at: usize) -> bool {
    let Some(first) = lines.get(at) else { return false };
    if first.trim() != "- insert:" {
        return false;
    }
    // 下一行必须是 `    - id: <pkg.id>`（id 值可能带单/双引号）。
    let Some(id_line) = lines.get(at + 1) else { return false };
    let id_trim = id_line.trim();
    if !id_trim.starts_with("- id:") {
        return false;
    }
    let raw = id_trim["- id:".len()..].trim();
    let id_val = raw.trim_matches('\'').trim_matches('"');
    let matched = PKGS.iter().any(|p| p.id == id_val);
    if !matched {
        return false;
    }
    // at+2 可能是 `name:` 行（允许存在）；at+3 若存在且是缩进行 → 块内还有
    // 更多条目（多条目 insert 块），不整体删除，保守保留。
    if let Some(tail) = lines.get(at + 2) {
        let t = tail.trim();
        if !t.is_empty() && !t.starts_with("name:") {
            return false;
        }
    }
    match lines.get(at + 3) {
        None => true,
        Some(next) => {
            let t = next.trim();
            t.is_empty() || !t.starts_with('-') && !t.starts_with("  ") && !t.starts_with('\t')
        }
    }
}

/// 清理标记块外与托管插件 id 相同的重复 insert 条目。
/// 升级场景：旧版本（或用户手动安装）把插件条目写在标记块外，新版本又通过
/// 标记块托管同一插件 → 同一 id 被 insert 两次，DSH 加载时报「重复插件」
/// 直接起不来。这里把标记块外 id 与托管插件相同的条目删除（标记块内的托管
/// 版本是权威）。只按 id 匹配托管插件自身，绝不触碰用户安装的其他第三方插件。
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

/// 在标记块内 upsert 插件条目（insert / disabled 覆盖行）：两个标记都存在则
/// 整块替换，否则追加——若文件是"仅空列表"则把 `[]` 就地替换为块（避免
/// flow 序列后接 block 条目的 YAML 语法错误）。标记块外内容原样保留。
///
/// 禁用状态从现有文件解析（标记块内的 `- id:` + `disabled: true` 行），
/// 因此禁用/启用切换后再次同步保持幂等。
///
/// 兼容旧版损坏标记：旧测试构建把 `⟪` 写成了 `隡`，先把它们规范化为
/// 标准标记再处理，保证新旧版本间幂等、不产生重复标记块。
/// 最后清理标记块外与托管插件重复的 insert 条目（升级残留去重）。
pub fn upsert_marker_block(existing: &str) -> String {
    upsert_marker_block_with(existing, &[], &[])
}

/// upsert 的完整形态：bundles = profile 的 bundle 层列表（R1：同名包已在
/// bundle 层 → 标记块跳过 insert）；forced_disabled = 调用方显式指定的禁用
/// 列表（toggle 命令用；同步流程传空、从现有文件解析）。
pub fn upsert_marker_block_with(existing: &str, bundles: &[String], forced_disabled: &[String]) -> String {
    let mut allowed: Vec<String> = PKGS.iter().map(|p| p.id.to_string()).collect();
    for extra in DISABLEABLE_IDS {
        if !allowed.iter().any(|x| x == extra) {
            allowed.push(extra.to_string());
        }
    }
    upsert_marker_block_with_allowed(existing, bundles, forced_disabled, &allowed)
}

/// upsert 的完整形态（动态 id 池）：allowed = 禁用解析允许的 id 池（PKGS
/// 全部 id + 常量可禁用 + bundle 层动态条目，见 disableable_id_pool）。
/// 重建时 PKGS 走 insert/覆盖分支，非 PKGS id 只输出 disabled 覆盖行。
pub fn upsert_marker_block_with_allowed(
    existing: &str,
    bundles: &[String],
    forced_disabled: &[String],
    allowed: &[String],
) -> String {
    let normalized = existing.replace(LEGACY_MARK_BEGIN, MARK_BEGIN).replace(LEGACY_MARK_END, MARK_END);
    let existing = &normalized;
    let mut disabled_ids = parse_disabled_ids_with(existing, allowed);
    for id in forced_disabled {
        if !disabled_ids.contains(id) {
            disabled_ids.push(id.clone());
        }
    }
    let extra_ids: Vec<String> = allowed
        .iter()
        .filter(|id| !PKGS.iter().any(|p| p.id == id.as_str()))
        .cloned()
        .collect();
    let block = managed_block_with(
        &disabled_ids.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        bundles,
        &extra_ids,
    );
    replace_marker_block(existing, &block)
}

/// 用给定标记块内容替换/追加标记区间（不做禁用解析）；标记块外内容原样保留。
/// 供 toggle 命令用「权威禁用列表」重建标记块（避免 upsert 重新解析旧行）。
pub fn replace_marker_block(existing: &str, block: &str) -> String {
    let merged = match (existing.find(MARK_BEGIN), existing.find(MARK_END)) {
        (Some(begin), Some(end)) if end > begin => {
            let tail_start = match existing[end..].find('\n') {
                Some(i) => end + i + 1,
                None => existing.len(),
            };
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..begin]);
            out.push_str(block);
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
                    // 一并吞掉 `[]` 后的换行，与「替换区间」分支的结尾一致（单 \n）。
                    let mut end = pos + 2;
                    if out.as_bytes().get(end) == Some(&b'\n') {
                        end += 1;
                    }
                    out.replace_range(pos..end, block);
                    return out;
                }
            }
            out.push_str(block);
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
/// 同步策略按包分两种：
/// - `force_sync`（鲸仔独占托管、private 不可手动安装的包，如 whalito-settings）：
///   **内容比对覆盖**，不依赖版本号——改了内嵌内容即使忘了升版本也会推送，
///   彻底绕开「版本没升就推不出去」的坑；
/// - 其余包（对外发布、用户可能手动安装/升级的 npm 包，如 dsh-webui-plus）：
///   **版本感知**——磁盘版本 ≥ 内置版本则整体跳过（尊重用户手动安装/升级，
///   升级鲸仔不降级覆盖用户内容）；仅当磁盘版本更低或未安装时才写入内置版本。
///
/// R5：目标目录是**符号链接**（pnpm 依赖树的标准形态，`dsh plugin add` 装的
/// 包都指向 .pnpm store）→ 整体跳过。绝不顺着符号链接写进 pnpm store 篡改
/// 依赖树；pnpm 管理的包由 dsh/pnpm 负责，鲸仔不碰。
pub fn sync_package_files(profile: &Path) -> Result<bool, String> {
    let mut changed = false;
    for pkg in PKGS {
        let pkg_dir = profile.join("node_modules").join(pkg.name);
        // pnpm 依赖树符号链接：用户通过 dsh plugin 安装的包，跳过（不写不覆盖）。
        if let Ok(meta) = fs::symlink_metadata(&pkg_dir) {
            if meta.file_type().is_symlink() {
                continue;
            }
        }
        if !pkg.force_sync {
            // 版本感知（仅对可手动安装的公开包）：磁盘版本 ≥ 内置 → 跳过。
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
/// 禁用状态从现有文件解析后重建标记块（幂等）；R1：bundle 层已有 PKGS 同名
/// 包（用户 dsh plugin 安装）→ 标记块跳过该插件的 insert 行，避免双重加载。
pub fn sync_patch_layer(profile: &Path) -> Result<bool, String> {
    let path = profile.join("cordis.patch.yml");
    let bundles = active_bundles(profile);
    let current = if path.exists() {
        fs::read_to_string(&path).map_err(|e| format!("读取 cordis.patch.yml 失败：{e}"))?
    } else {
        // 文件不存在：直接写标记块（不要先写 `[]` 再追加，避免中间态语法错误）。
        let block = managed_block(&[], &bundles);
        fs::write(&path, block).map_err(|e| format!("写入 cordis.patch.yml 失败：{e}"))?;
        return Ok(true);
    };
    let updated = {
        let allowed = disableable_id_pool(profile);
        upsert_marker_block_with_allowed(&current, &bundles, &[], &allowed)
    };
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

/// 内置插件状态行（设置分区「内置插件」tab 的数据源）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PluginEntryView {
    /// loader 条目 id（patch 覆盖行按它定向）。
    pub id: String,
    /// npm 包名。
    pub name: String,
    /// 一句话描述。
    pub description: String,
    /// 鲸仔自身组件（whalito-settings）：不提供禁用。
    pub builtin: bool,
    /// 未安装时可由鲸仔安装（dshmarket 被用户卸载后的恢复入口）。
    pub installable: bool,
    /// 是否已安装（node_modules 有包 = 已就绪；dshmarket 看 bundle 层）。
    pub installed: bool,
    /// 当前是否被禁用（patch 标记块内的 disabled 覆盖行）。
    pub disabled: bool,
}

/// 内置/可管理插件列表 + 当前状态（供「鲸仔设置」分区与面板展示）。
/// 覆盖：鲸仔托管 PKGS（whalito-settings 内置不可禁、dsh-webui-plus）+ 
/// dshmarket（已装走 bundle 层条目，未装给安装入口）+ 插件市场安装的
/// 其他 bundle 层插件（描述取各自 package.json）。
#[tauri::command]
pub fn plugins_status(_app: AppHandle) -> Vec<PluginEntryView> {
    let profile = profile_dir();
    let patch = fs::read_to_string(profile.join("cordis.patch.yml")).unwrap_or_default();
    let allowed = disableable_id_pool(&profile);
    let disabled = parse_disabled_ids_with(&patch, &allowed);
    let bundles = installed_bundles(&profile);
    let is_disabled = |id: &str| disabled.iter().any(|d| d == id);
    let mut out: Vec<PluginEntryView> = Vec::new();
    for pkg in PKGS {
        let builtin = pkg.id == "whalito-settings";
        out.push(PluginEntryView {
            id: pkg.id.to_string(),
            name: pkg.name.to_string(),
            description: match pkg.id {
                "whalito-settings" => "鲸仔设置分区：内嵌页里鲸仔配置、服务器控制与版本信息的入口。".to_string(),
                _ => "WebUI+ 增强：DSH 网页界面的实用增强（版本徽标、推荐卡片等）。".to_string(),
            },
            builtin,
            installable: false,
            installed: true,
            disabled: !builtin && is_disabled(pkg.id),
        });
    }
    // dsh-market 未安装时保留安装入口行（已装时走 bundle 层条目，见下）。
    let market_installed = bundles.iter().any(|b| b == crate::market::MARKET_PKG);
    if !market_installed {
        out.push(PluginEntryView {
            id: "dsh-market".to_string(),
            name: crate::market::MARKET_PKG.to_string(),
            description: "插件市场：在 DSH 设置页内浏览、搜索并安装社区插件（含热挂载与自重启）。".to_string(),
            builtin: false,
            installable: true,
            installed: false,
            disabled: false,
        });
    }
    // bundle 层可禁用条目（含已装的 dsh-market 与市场装的其他插件）。
    for entry in bundle_disable_entries(&profile) {
        let entry_id = entry.id.clone();
        let description = if entry.description.is_empty() {
            format!("插件市场安装的 bundle 插件（{}）。", entry.pkg)
        } else if entry.id == "dsh-market" {
            // dsh-market 有既定中文描述，覆盖英文包描述。
            "插件市场：在 DSH 设置页内浏览、搜索并安装社区插件（含热挂载与自重启）。".to_string()
        } else {
            entry.description
        };
        out.push(PluginEntryView {
            id: entry.id,
            name: entry.pkg,
            description,
            builtin: false,
            installable: false,
            installed: true,
            disabled: is_disabled(&entry_id),
        });
    }
    out
}

/// 切换插件禁用状态（写 cordis.patch.yml 标记块内的 disabled 覆盖行）。
/// 允许集 = PKGS（whalito-settings 内置除外）+ 常量 + bundle 层动态条目；
/// **禁用 ≠ 卸载**——不删除任何插件文件，重新启用即恢复加载
/// （bundle 层插件保持已安装）。改动需重启 DSH 生效。返回新的状态列表。
#[tauri::command]
pub fn toggle_plugin(app: AppHandle, id: String, enabled: bool) -> Result<Vec<PluginEntryView>, String> {
    let profile = profile_dir();
    let allowed = disableable_id_pool(&profile);
    if id == "whalito-settings" || !allowed.iter().any(|x| x == &id) {
        return Err(format!("插件 {id} 不允许禁用（内置组件、DSH 内核或未知插件）"));
    }
    let path = profile.join("cordis.patch.yml");
    let current = if path.exists() {
        fs::read_to_string(&path).map_err(|e| format!("读取 cordis.patch.yml 失败：{e}"))?
    } else {
        String::new()
    };
    let bundles = active_bundles(&profile);
    let mut disabled = parse_disabled_ids_with(&current, &allowed);
    if enabled {
        disabled.retain(|d| d != &id);
    } else if !disabled.contains(&id) {
        disabled.push(id.clone());
    }
    // 用权威禁用列表重建标记块（不走 upsert 的重新解析，保证启用后旧行被移除）。
    let block = managed_block_with(
        &disabled.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        &bundles,
        &allowed
            .iter()
            .filter(|x| !PKGS.iter().any(|p| p.id == x.as_str()))
            .cloned()
            .collect::<Vec<_>>(),
    );
    let updated = replace_marker_block(&current, &block);
    if updated != current {
        fs::write(&path, updated).map_err(|e| format!("写入 cordis.patch.yml 失败：{e}"))?;
    }
    let st = app.state::<AppState>();
    crate::state::push_log(
        &st.logs,
        &format!(
            "[系统] 已{}插件 {id}（重启服务器后生效）",
            if enabled { "启用" } else { "禁用" }
        ),
    );
    Ok(plugins_status(app))
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
    fn managed_block_emits_disabled_overrides() {
        // 禁用 webui-plus（PKGS）：insert 行保留 + disabled 覆盖行——
        // patch 顶层条目只能覆盖「已存在」的行，没有 insert 覆盖会落空。
        let block = managed_block(&["dsh-webui-plus"], &[]);
        assert!(block.contains("- id: dsh-webui-plus\n  disabled: true"));
        assert!(block.contains("name: '@entireyu/dsh-webui-plus'"), "禁用时必须保留 insert 行：{block}");
        // whalito-settings 不受影响（不可禁用，仍 insert）。
        assert!(block.contains("id: whalito-settings"));
        assert!(block.contains("name: '@entireyu/whalito-dsh-settings'"));
        // dshmarket（不在 PKGS，bundle 层行由包自身 insert）→ 只需覆盖行。
        let block2 = managed_block(&["dsh-market"], &[]);
        assert!(block2.contains("- id: dsh-market\n  disabled: true"));
        assert!(block2.contains("name: '@entireyu/dsh-webui-plus'"));
    }

    #[test]
    fn managed_block_skips_bundles_managed_pkgs() {
        // R1：bundle 层「真正激活」的同名包（active_bundles 由调用方算出）→ 跳过 insert。
        let active = vec!["@entireyu/dsh-webui-plus".to_string()];
        let block = managed_block(&[], &active);
        assert!(!block.contains("name: '@entireyu/dsh-webui-plus'"));
        assert!(block.contains("name: '@entireyu/whalito-dsh-settings'"));
        // 未激活 → 照常 insert（普通依赖不算，否则 WebUI+ 会凭空消失）。
        let block2 = managed_block(&[], &[]);
        assert!(block2.contains("name: '@entireyu/dsh-webui-plus'"));
    }

    #[test]
    fn bundle_layer_active_requires_dsh_bundle_manifest() {
        // bundle_layer_active：bundles 列出 + 包声明 dsh.bundle 才算激活。
        let base = std::env::temp_dir().join(format!("whalito-bla-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let profile = base.join("profiles").join("web");
        // 无 dsh 声明 → 普通依赖，不激活。
        let plain = profile.join("node_modules").join("plain-pkg");
        fs::create_dir_all(&plain).unwrap();
        fs::write(plain.join("package.json"), r#"{"name":"plain-pkg","version":"1.0.0"}"#).unwrap();
        assert!(!bundle_layer_active(&profile, "plain-pkg"));
        // 只有 dsh.client → 不激活（dsh.client 单独不可安装）。
        let client_only = profile.join("node_modules").join("client-pkg");
        fs::create_dir_all(&client_only).unwrap();
        fs::write(
            client_only.join("package.json"),
            r#"{"name":"client-pkg","dsh":{"client":{"platform":"web"}}}"#,
        )
        .unwrap();
        assert!(!bundle_layer_active(&profile, "client-pkg"));
        // 声明 dsh.bundle → 激活。
        let bundle = profile.join("node_modules").join("bundle-pkg");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(
            bundle.join("package.json"),
            r#"{"name":"bundle-pkg","dsh":{"bundle":{"patch":"./cordis.patch.yml"}}}"#,
        )
        .unwrap();
        assert!(bundle_layer_active(&profile, "bundle-pkg"));
        // 文件缺失 → false。
        assert!(!bundle_layer_active(&profile, "missing-pkg"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn parse_disabled_ids_finds_top_level_entries_only() {
        // 顶格 `- id:` + disabled: true 才是禁用条目；insert 块的缩进 id 不算。
        let patch = format!(
            "{MARK_BEGIN}\n\
             - insert:\n    - id: dsh-webui-plus\n      name: 'x'\n\
             - id: dsh-market\n  disabled: true\n\
             {MARK_END}\n\
             - insert:\n    - id: dsh-market\n      name: 'y'\n"
        );
        let ids = parse_disabled_ids(&patch);
        assert_eq!(ids, vec!["dsh-market".to_string()]);
        // 无禁用内容 → 空。
        assert!(parse_disabled_ids("# 注释\n[]\n").is_empty());
    }

    #[test]
    fn disabled_state_survives_sync_and_toggle_roundtrip() {
        // 禁用 dshmarket → 标记块含覆盖行；再次 upsert（模拟下次同步）幂等。
        let base = "# 用户注释\n[]\n";
        let once = upsert_marker_block_with(base, &[], &["dsh-market".to_string()]);
        assert!(once.contains("- id: dsh-market\n  disabled: true"));
        let twice = upsert_marker_block_with(&once, &[], &[]);
        assert_eq!(once, twice);
        // 启用：权威列表重建（replace 路径）→ 覆盖行被移除、insert 恢复。
        let disabled: Vec<String> = Vec::new();
        let block = managed_block(&[], &[]);
        let reenabled = replace_marker_block(&once, &block);
        assert!(!reenabled.contains("- id: dsh-market"));
        assert!(reenabled.contains("name: '@entireyu/dsh-webui-plus'"));
        // 用户注释保留。
        assert!(reenabled.starts_with("# 用户注释\n"));
    }

    #[test]
    fn sync_package_files_skips_symlinked_dirs() {
        // R5：pnpm 依赖树是符号链接，鲸仔绝不写穿它（此测试仅在可建符号链接的平台）。
        #[cfg(not(windows))]
        {
            let base =
                std::env::temp_dir().join(format!("whalito-symlink-test-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let profile = base.join("profiles").join("web");
            let pkg_dir = profile.join("node_modules").join(PKGS[1].name);
            fs::create_dir_all(pkg_dir.parent().unwrap()).unwrap();
            let target = base.join("store").join("real-pkg");
            fs::create_dir_all(&target).unwrap();
            fs::write(target.join("package.json"), r#"{"name":"x","version":"9.9.9"}"#).unwrap();
            std::os::unix::fs::symlink(&target, &pkg_dir).unwrap();
            // 第一次：PKGS[0]（whalito-settings，非符号链接）尚未安装会正常写入
            //（changed=true）；PKGS[1]（webui-plus 的符号链接目录）必须跳过。
            assert!(sync_package_files(&profile).unwrap());
            // 第二次：内容已一致 → 无写入（符号链接包依旧跳过），返回 changed=false。
            assert!(!sync_package_files(&profile).unwrap());
            assert_eq!(
                fs::read_to_string(target.join("package.json")).unwrap(),
                r#"{"name":"x","version":"9.9.9"}"#,
                "不得顺着符号链接写入 store"
            );
            let _ = fs::remove_dir_all(&base);
        }
        // Windows：不建符号链接（权限），仅验证普通目录路径仍正常。
        #[cfg(windows)]
        {
            let base = std::env::temp_dir()
                .join(format!("whalito-symlink-test-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let profile = base.join("profiles").join("web");
            assert!(sync_package_files(&profile).unwrap());
            let _ = fs::remove_dir_all(&base);
        }
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
    fn force_sync_pkg_overrides_same_version_content() {
        // 防再犯（0.5.0 踩过的坑）：whalito-settings 内容变更但版本号没升
        // （磁盘版本 == 内置版本）→ force_sync 必须仍按内容覆盖推送。
        let base = std::env::temp_dir().join(format!("whalito-fsync-test-{}", std::process::id()));
        let profile = base.join("profiles").join("web");
        let _ = fs::remove_dir_all(&base);
        let pkg_dir = profile.join("node_modules").join(PKGS[0].name);
        fs::create_dir_all(&pkg_dir).unwrap();
        // 磁盘 package.json 与内置同版本（0.2.2），但 client.js 是旧内容。
        let same_ver = r#"{"name":"@entireyu/whalito-dsh-settings","version":"0.2.2"}"#;
        fs::write(pkg_dir.join("package.json"), same_ver).unwrap();
        fs::write(pkg_dir.join("client.js"), "// 旧内容\n").unwrap();
        assert!(sync_package_files(&profile).unwrap());
        let written = fs::read_to_string(pkg_dir.join("client.js")).unwrap();
        assert!(
            !written.contains("// 旧内容"),
            "force_sync 包内容不同必须覆盖：{written}"
        );
        // 幂等：内容相同后不再写。
        assert!(!sync_package_files(&profile).unwrap());
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

    #[test]
    fn dedupes_user_installed_dup_before_block() {
        // 用户曾手动安装 dsh-webui-plus：条目写在标记块**之前**（旧版鲸仔
        // 只托管 whalito-settings 时用户手动加的），升级后标记块托管同 id →
        // 必须只保留标记块内一份，否则 DSH 报重复插件起不来。
        let input = format!(
            "- insert:\n    - id: dsh-webui-plus\n      name: '@entireyu/dsh-webui-plus'\n\
             {MARK_BEGIN}\n- insert:\n    - id: whalito-settings\n      name: '@entireyu/whalito-dsh-settings'\n{MARK_END}\n"
        );
        let out = upsert_marker_block(&input);
        assert_eq!(out.matches("id: dsh-webui-plus").count(), 1, "块前重复条目应被清理：{out}");
        assert!(block_has_all_pkgs(&out));
        // 幂等。
        assert_eq!(upsert_marker_block(&out), out);
    }

    #[test]
    fn dedupes_dup_without_name_line() {
        // 用户手写的条目可能只有 `- id:` 没有 `name:`（或名字写法不同）——
        // id 是唯一标识，按 id 去重。
        let input = format!(
            "{MARK_BEGIN}\n- insert:\n    - id: dsh-webui-plus\n      name: '@entireyu/dsh-webui-plus'\n{MARK_END}\n\
             - insert:\n    - id: dsh-webui-plus\n"
        );
        let out = upsert_marker_block(&input);
        assert_eq!(out.matches("id: dsh-webui-plus").count(), 1, "无 name 的重复条目也应清理：{out}");
        assert!(block_has_all_pkgs(&out));
    }

    #[test]
    fn dedupes_dup_with_quoted_id() {
        // 用户手写 id 可能带引号（YAML 合法）——按 id 值去重。
        let input = format!(
            "{MARK_BEGIN}\n- insert:\n    - id: dsh-webui-plus\n      name: '@entireyu/dsh-webui-plus'\n{MARK_END}\n\
             - insert:\n    - id: 'dsh-webui-plus'\n      name: '@entireyu/dsh-webui-plus'\n"
        );
        let out = upsert_marker_block(&input);
        assert_eq!(out.matches("id: dsh-webui-plus").count(), 1, "带引号 id 的重复条目也应清理：{out}");
        assert_eq!(out.matches("id: 'dsh-webui-plus'").count(), 0);
        assert!(block_has_all_pkgs(&out));
    }

    #[test]
    fn keeps_multi_insert_block_with_user_plugin() {
        // 块外 `- insert:` 同时含用户插件 + 托管插件（多条目块）→ 不能整体删，
        // 保守保留（至少不删用户插件；托管的由标记块内权威条目保证加载）。
        let input = format!(
            "{MARK_BEGIN}\n- insert:\n    - id: dsh-webui-plus\n      name: '@entireyu/dsh-webui-plus'\n{MARK_END}\n\
             - insert:\n    - id: dsh-webui-plus\n      name: '@entireyu/dsh-webui-plus'\n    - id: my-ui\n      name: 'my-ui'\n"
        );
        let out = upsert_marker_block(&input);
        assert!(out.contains("id: my-ui"), "用户插件条目必须保留：{out}");
    }

    #[test]
    fn backup_copies_config_and_skips_node_modules() {
        let base = std::env::temp_dir().join(format!("whalito-bak-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let home = base.join("home");
        let prefix = base.join("prefix");
        let backup_root = base.join("backup");
        // 构造 DSH 家目录：settings.yaml + profiles/web（含 node_modules 与 patch）。
        fs::create_dir_all(home.join("profiles").join("web").join("node_modules")).unwrap();
        fs::write(home.join("settings.yaml"), "k: v\n").unwrap();
        fs::write(home.join("profiles").join("web").join("cordis.patch.yml"), "# p\n").unwrap();
        fs::write(home.join("profiles").join("web").join("node_modules").join("big.bin"), "x".repeat(10)).unwrap();
        // 构造应用前缀：DSH 本体 + 用户第三方插件。
        fs::create_dir_all(prefix.join("node_modules").join("@deepseek-ai").join("dsh")).unwrap();
        fs::write(prefix.join("node_modules").join("@deepseek-ai").join("dsh").join("index.js"), "dsh").unwrap();
        fs::create_dir_all(prefix.join("node_modules").join("user-plugin")).unwrap();
        fs::write(prefix.join("node_modules").join("user-plugin").join("client.js"), "user").unwrap();

        let dir = backup_dsh_config_at(&home, &prefix, &backup_root)
            .unwrap()
            .expect("应产生备份目录");
        assert!(dir.join("settings.yaml").exists());
        assert!(dir.join("profiles").join("web").join("cordis.patch.yml").exists());
        assert!(
            !dir.join("profiles").join("web").join("node_modules").exists(),
            "node_modules 不应被备份（依赖可重建）"
        );
        assert!(dir.join("user-plugins").join("user-plugin").join("client.js").exists());
        assert!(
            !dir.join("user-plugins").join("@deepseek-ai").join("dsh").exists(),
            "DSH 本体不应被备份"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn backup_prunes_to_max_kept() {
        let base = std::env::temp_dir().join(format!("whalito-prune-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let home = base.join("home");
        let prefix = base.join("prefix");
        let backup_root = base.join("backup");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&prefix).unwrap();
        fs::write(home.join("settings.yaml"), "k: v\n").unwrap();
        // 连续备份 MAX_BACKUPS + 3 次（毫秒级命名不会同名冲突）。
        for _ in 0..(MAX_BACKUPS + 3) {
            backup_dsh_config_at(&home, &prefix, &backup_root).unwrap();
        }
        let kept = fs::read_dir(&backup_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("dsh-backup-"))
            .count();
        assert_eq!(kept, MAX_BACKUPS, "旧备份应被清理，只保留最近 {MAX_BACKUPS} 份");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn backup_returns_none_when_nothing_to_backup() {
        let base = std::env::temp_dir().join(format!("whalito-none-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let home = base.join("home");
        let prefix = base.join("prefix");
        let backup_root = base.join("backup");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&prefix).unwrap();
        assert!(backup_dsh_config_at(&home, &prefix, &backup_root).unwrap().is_none());
        let _ = fs::remove_dir_all(&base);
    }

    // —— bundle 层插件禁用（0.5.2 插件管理）——

    /// 构造含 bundle 层插件的 profile 临时目录：
    /// bundles = [@deepseek-ai/dsh-base（内核）, dshmarket, my-plugin]，
    /// 后两个包带 `dsh.bundle.patch` 与补丁文件（多形态 id）。
    fn fixture_profile_with_bundles() -> PathBuf {
        let base = std::env::temp_dir().join(format!("whalito-bundle-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let profile = base.join("profiles").join("web");
        fs::create_dir_all(&profile).unwrap();
        fs::write(
            profile.join("package.json"),
            r#"{"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base","dshmarket","my-plugin"]}}}"#,
        )
        .unwrap();
        let pkgs: &[(&str, &str, &str)] = &[
            ("dshmarket", "插件市场", "- insert:\n    - id: dsh-market\n      name: 'dshmarket'\n"),
            (
                "my-plugin",
                "我的增强插件",
                "- id: my-fancy\n  name: 'my-plugin'\n- insert:\n    - id: 'my-extra'\n      name: 'my-plugin'\n",
            ),
        ];
        for (name, desc, patch) in pkgs {
            let dir = profile.join("node_modules").join(name);
            fs::create_dir_all(&dir).unwrap();
            let manifest = format!(
                r#"{{"name":"{name}","description":"{desc}","dsh":{{"bundle":{{"patch":"./cordis.patch.yml"}}}}}}"#
            );
            fs::write(dir.join("package.json"), manifest).unwrap();
            fs::write(dir.join("cordis.patch.yml"), patch).unwrap();
        }
        // 内核 bundle（@deepseek-ai scope）：即使列在 bundles 也不可禁用。
        let core_dir = profile.join("node_modules").join("@deepseek-ai").join("dsh-base");
        fs::create_dir_all(&core_dir).unwrap();
        fs::write(
            core_dir.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh-base","dsh":{"bundle":{"patch":"./cordis.patch.yml"}}}"#,
        )
        .unwrap();
        fs::write(
            core_dir.join("cordis.patch.yml"),
            "- insert:\n    - id: dsh-base\n      name: '@deepseek-ai/dsh-base'\n",
        )
        .unwrap();
        profile
    }

    #[test]
    fn patch_loader_ids_extracts_top_level_and_insert_children() {
        let text = "\
- insert:
    - id: top-insert
      name: x
      config:
        - id: deep-nested
- id: 'quoted-top'
  disabled: true
- insert:
    - id: second
    - id: third
";
        let ids = patch_loader_ids(text);
        assert!(ids.contains(&"top-insert".to_string()));
        assert!(ids.contains(&"quoted-top".to_string()));
        assert!(ids.contains(&"second".to_string()));
        assert!(ids.contains(&"third".to_string()));
        assert!(
            !ids.contains(&"deep-nested".to_string()),
            "深层配置字段（缩进 8）不是 loader 条目，不应纳入：{ids:?}"
        );
    }

    #[test]
    fn bundle_disable_entries_skips_core_and_matches_descriptions() {
        let profile = fixture_profile_with_bundles();
        let entries = bundle_disable_entries(&profile);
        let _ = fs::remove_dir_all(profile.parent().unwrap().parent().unwrap());
        // dsh-base（内核）不在其中；dshmarket 与 my-plugin 的 loader id 都在。
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert!(!ids.contains(&"dsh-base"));
        assert!(ids.contains(&"dsh-market"));
        assert!(ids.contains(&"my-fancy"));
        assert!(ids.contains(&"my-extra"));
        let market = entries.iter().find(|e| e.id == "dsh-market").unwrap();
        assert_eq!(market.pkg, "dshmarket");
        assert_eq!(market.description, "插件市场");
        let fancy = entries.iter().find(|e| e.id == "my-fancy").unwrap();
        assert_eq!(fancy.description, "我的增强插件");
    }

    #[test]
    fn disableable_pool_covers_pkgs_constants_and_bundles() {
        let profile = fixture_profile_with_bundles();
        let pool = disableable_id_pool(&profile);
        let _ = fs::remove_dir_all(profile.parent().unwrap().parent().unwrap());
        for expect in ["whalito-settings", "dsh-webui-plus", "dsh-market", "my-fancy", "my-extra"] {
            assert!(pool.iter().any(|x| x == expect), "池应包含 {expect}：{pool:?}");
        }
        assert!(!pool.iter().any(|x| x == "dsh-base"), "内核 bundle 不得入池");
    }

    #[test]
    fn managed_block_with_only_emits_disabled_extra_overrides() {
        let extras = vec!["my-fancy".to_string(), "my-extra".to_string()];
        let block = managed_block_with(&["my-fancy", "dsh-webui-plus"], &[], &extras);
        assert!(block.contains("- id: my-fancy\n  disabled: true"), "{block}");
        assert!(
            !block.contains("id: my-extra"),
            "未禁用的 extra 不输出覆盖行：{block}"
        );
        assert!(
            block.contains("- id: dsh-webui-plus\n  disabled: true"),
            "PKGS 分支仍需输出覆盖行：{block}"
        );
    }

    #[test]
    fn sync_keeps_bundle_disabled_override_with_dynamic_pool() {
        let profile = fixture_profile_with_bundles();
        let base = profile.parent().unwrap().parent().unwrap();
        let existing = format!(
            "{MARK_BEGIN}\n- insert:\n    - id: whalito-settings\n      name: '@entireyu/whalito-dsh-settings'\n\
             - id: my-fancy\n  disabled: true\n{MARK_END}\n"
        );
        let allowed = disableable_id_pool(&profile);
        let bundles = active_bundles(&profile);
        let out = upsert_marker_block_with_allowed(&existing, &bundles, &[], &allowed);
        assert!(
            out.contains("- id: my-fancy\n  disabled: true"),
            "同步重建后 bundle 插件禁用覆盖行必须保留：{out}"
        );
        // 幂等：再跑一次结果不变。
        let again = upsert_marker_block_with_allowed(&out, &bundles, &[], &allowed);
        assert_eq!(out, again);
        // 未知 id 的 disabled 行（不属于池）应被当作外部内容丢弃（既有语义）。
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn upsert_with_forced_disabled_roundtrip_for_bundle_id() {
        let existing = format!(
            "{MARK_BEGIN}\n- insert:\n    - id: whalito-settings\n      name: '@entireyu/whalito-dsh-settings'\n{MARK_END}\n"
        );
        let allowed = vec![
            "whalito-settings".to_string(),
            "dsh-webui-plus".to_string(),
            "my-extra".to_string(),
        ];
        // 禁用 → 覆盖行出现。
        let disabled_once =
            upsert_marker_block_with_allowed(&existing, &[], &["my-extra".to_string()], &allowed);
        assert!(disabled_once.contains("- id: my-extra\n  disabled: true"), "{disabled_once}");
        // 模拟同步（无 forced）：从现有文件解析并保留，幂等。
        let kept = upsert_marker_block_with_allowed(&disabled_once, &[], &[], &allowed);
        assert!(kept.contains("- id: my-extra\n  disabled: true"), "{kept}");
        assert_eq!(kept, disabled_once);
        // 启用 = 用权威禁用列表（去掉该 id）重建标记块（toggle 的路径）。
        let extra_ids: Vec<String> = allowed
            .iter()
            .filter(|id| !PKGS.iter().any(|p| p.id == id.as_str()))
            .cloned()
            .collect();
        let block = managed_block_with(&["whalito-settings"], &[], &extra_ids);
        let re_enabled = replace_marker_block(&disabled_once, &block);
        assert!(!re_enabled.contains("my-extra"), "启用后覆盖行应移除：{re_enabled}");
    }
}
