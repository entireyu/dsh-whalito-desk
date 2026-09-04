<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { emit, listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { dshOrigin, isWhalitoMessage, postToDsh, toPlain } from "./whalitoBridge";
import LoadingScreen from "./LoadingScreen.vue";
import type {
  DshPathView,
  DshPathsSnapshot,
  VersionsSnapshot,
  WhalitoMessage,
  WhalitoPluginEntry,
  WhalitoSettings,
  WhalitoVersionInfo,
} from "./whalitoBridge";

interface EnvInfo {
  found: boolean;
  version: string | null;
  nodePath: string | null;
  npmPrefix: string | null;
  installPrefix: string | null;
  dshInstalled: boolean;
  dshVersion: string | null;
  nodeVersionOk: boolean;
  nodeTooOld: boolean;
  nvmFound: boolean;
  nvmPath: string | null;
  /** 当前 Node 不可用时，nvm 已安装、可直接切换到的最高合格版本。 */
  nvmSwitchVersion: string | null;
}

interface ServerStatus {
  phase: string;
  url: string | null;
  pid: number | null;
}

/** 后台更新提示（来自 Rust 每小时检查的 update-available 事件）。 */
interface UpdateNotice {
  target: "dsh" | "whalito";
  current: string;
  latest: string;
  changelog: string | null;
  url: string | null;
}

/** 鲸仔自更新结果（重启后一次性校验：当前版本 vs 目标版本）。 */
interface UpdateMarkerResult {
  success: boolean;
  from: string;
  to: string;
}

interface Settings {
  port: number;
  registry: string;
  /** DSH 版本偏好："latest"（稳定版）/ "next"（预发布版）。 */
  dshChannel: string;
  autostart: boolean;
  autoRestart: boolean;
  workspaceDir: string | null;
  nodeDir: string | null;
  downloadDir: string | null;
  petEnabled: boolean;
  /** 服务跟随鲸仔程序停止（默认关）：退出鲸仔时同时停止由鲸仔启动的 DSH 服务。 */
  dshStopWithWhalito: boolean;
  /** 自动检查 DSH 更新（默认开）：关闭后后台不再自动检查，手动检查不受影响。 */
  dshAutoCheckUpdate: boolean;
  /** 自动检查鲸仔更新（默认开）：关闭后后台不再自动检查，手动检查不受影响。 */
  whalitoAutoCheckUpdate: boolean;
}

const env = ref<EnvInfo | null>(null);
const server = ref<ServerStatus>({ phase: "stopped", url: null, pid: null });
// 服务器启停/重启进行中的文案：内嵌页在切换期间显示 loading，
// 而不是因 url 暂时为空直接跳到「服务器未运行」。
const serverBusy = ref("");
const settings = ref<Settings | null>(null);
// 当前平台（"windows" / "macos" / "linux"），由后端 get_platform 命令返回。
const platform = ref<string>("windows");

// —— 运行日志（结构化行：类型/时间/内容；展开、分页、错误复制） ——
interface LogRow {
  id: number;
  ts: number;
  kind: "system" | "error" | "info";
  text: string;
}
/** 启发式分类：[系统] 前缀为系统消息；错误关键词命中为 error；其余 info。 */
function classifyLog(text: string): LogRow["kind"] {
  if (text.startsWith("[系统]")) return "system";
  if (
    /error|错误|失败|exception|panic|refused|EPERM|ECONN|EACCES|EADDRINUSE|fatal|status code [45]\d\d/i.test(
      text,
    )
  ) {
    return "error";
  }
  return "info";
}
let nextLogId = 0;
function makeLogRow(text: string): LogRow {
  return { id: ++nextLogId, ts: Date.now(), kind: classifyLog(text), text };
}
const logs = ref<LogRow[]>([]);
// 日志窗口：默认渲染尾部 LOG_WINDOW 条；「加载更早」每次回退 LOG_PAGE 条。
const LOG_WINDOW = 300;
const LOG_PAGE = 300;
const logHidden = ref(0);
const logAutoScroll = ref(true);
const expandedLogIds = ref<Set<number>>(new Set());
const logBox = ref<HTMLElement | null>(null);

/** 追加一条日志（live 事件 / 本地消息共用）；自动滚动只在贴底且开关打开时触发。 */
function pushLogRow(text: string) {
  logs.value.push(makeLogRow(text));
  if (logs.value.length > 2500) {
    const drop = logs.value.length - 2500;
    logs.value.splice(0, drop);
    logHidden.value = Math.max(0, logHidden.value - drop);
  }
  // 新行到来时若已展示到尾部，保持尾部窗口（不自动把旧行挤出视图）。
  logHidden.value = Math.min(logHidden.value, Math.max(0, logs.value.length - LOG_WINDOW));
  if (logAutoScroll.value) requestAnimationFrame(() => autoScrollIfPinned(true));
}

/** 日志视口是否贴底（留 48px 容差）；贴底才允许新行自动滚动。 */
function isLogPinned(): boolean {
  const el = logBox.value;
  if (!el) return true;
  return el.scrollHeight - el.scrollTop - el.clientHeight < 48;
}
function autoScrollIfPinned(force = false) {
  if (!force && !isLogPinned()) return;
  const el = logBox.value;
  if (el) el.scrollTop = el.scrollHeight;
}
function logVisible(): LogRow[] {
  return logs.value.slice(logHidden.value);
}
function logErrorRows(): LogRow[] {
  return logs.value.filter((r) => r.kind === "error");
}
function toggleLogExpanded(id: number) {
  const next = new Set(expandedLogIds.value);
  if (next.has(id)) next.delete(id);
  else next.add(id);
  expandedLogIds.value = next;
}
/** 复制文本到系统剪贴板（走 Rust，跨 iframe 场景可靠），返回是否成功。 */
async function copyText(text: string): Promise<boolean> {
  if (!text) return false;
  try {
    await invoke("clipboard_write", { text });
    showToast("已复制到剪贴板");
    return true;
  } catch {
    return false;
  }
}
/** 「复制到 AI」模板：版本信息 + 报错内容（按需求措辞）。 */
function toAiPrompt(rows: LogRow[]): string {
  const head =
    `现在在使用 DSH，版本号 ${env.value?.dshVersion ?? "未知"}（鲸仔 v${
      whalitoVer.value?.current ?? "未知"
    }${whalitoVer.value?.testBuild ? " 测试版" : ""}·${platform.value}）。` +
    "日志报错内容如下，请帮我排查并修复：\n\n";
  const body = rows
    .map((r) => `[${new Date(r.ts).toLocaleString()}] ${r.text}`)
    .join("\n");
  return head + body;
}
const haveErrors = computed(() => logErrorRows().length > 0);
const whalitoUpdateAvailable = computed(
  () => !!whalitoVer.value?.updateAvailable && !whalitoVer.value?.testBuild,
);

const busy = ref<string | null>(null);
const error = ref<string>("");
const notice = ref<string>("");
const showSettings = ref(false);
const confirmingStop = ref(false);
const autoRestartCount = ref(0);
const MAX_AUTO_RESTART = 3;
/** 连续自动重启失败（面板错误横幅可给出「更新鲸仔 / 插件管理」动作）。 */
const autoRestartFailed = ref(false);

// 插件市场（dsh-market）状态：bundles 判定 + installedOnce 标记。
const marketInstalled = ref(false);
const marketOnce = ref(false);
// 内置插件列表（hello 快照带给「鲸仔设置」分区的内置插件 tab）。
const plugins = ref<WhalitoPluginEntry[]>([]);
// dsh 命令注册到 PATH 的状态（用户级 / 系统级；设置分区「其他设置」展示）。
const dshPaths = ref<DshPathsSnapshot>({ user: null, system: null });

async function loadPlugins() {
  plugins.value = await invoke<WhalitoPluginEntry[]>("plugins_status").catch(() => []);
}

async function loadDshPaths() {
  const [user, system] = await Promise.all([
    invoke<DshPathView>("dsh_path_status", { level: "user" }).catch((e) => {
      pushLogRow(`[系统] 读取用户级 PATH 状态失败：${typeof e === "string" ? e : String(e)}`);
      return null;
    }),
    invoke<DshPathView>("dsh_path_status", { level: "system" }).catch((e) => {
      pushLogRow(`[系统] 读取系统级 PATH 状态失败：${typeof e === "string" ? e : String(e)}`);
      return null;
    }),
  ]);
  dshPaths.value = { user, system };
}

async function loadMarketStatus() {
  try {
    const s = await invoke<{ installed: boolean; installed_once: boolean }>("market_status");
    marketInstalled.value = s.installed;
    marketOnce.value = s.installed_once;
  } catch {
    /* 忽略瞬时错误 */
  }
}

/** 面板「插件市场」：检查/安装（force=false）或显式重新安装（force=true）。 */
async function syncMarket(force = false) {
  await wrap(`正在${force ? "重新安装" : "检查"}插件市场…`, async () => {
    const msg = await invoke<string>("sync_market_plugin", { force });
    notice.value = msg;
    await loadMarketStatus();
    return msg;
  });
}

// ============ 插件管理（应急保启动：禁用会打断 DSH 启动的插件） ============
const pluginsBusy = ref<string | null>(null);

/** 禁用/启用插件：写 cordis.patch.yml 覆盖行，重启服务器后生效。 */
async function togglePluginEntry(row: WhalitoPluginEntry) {
  pluginsBusy.value = row.id;
  try {
    const list = await wrap(
      `正在${row.disabled ? "启用" : "禁用"}插件 ${row.name}…`,
      () => invoke<WhalitoPluginEntry[]>("toggle_plugin", { id: row.id, enabled: row.disabled }),
    );
    if (list) {
      plugins.value = list;
      if (server.value.phase === "running" || server.value.phase === "external") {
        notice.value = `插件 ${row.name} 已${row.disabled ? "启用" : "禁用"}，重启服务器后生效。`;
      } else {
        notice.value = `插件 ${row.name} 已${row.disabled ? "启用" : "禁用"}，下次启动服务器时生效。`;
      }
    }
  } catch (e) {
    error.value = typeof e === "string" ? e : String(e);
  } finally {
    pluginsBusy.value = null;
  }
}

/** 滚动到「插件管理」卡片（启动失败引导入口用）。 */
function focusPluginsCard() {
  goPanel();
  requestAnimationFrame(() => {
    const el = document.getElementById("panel-plugins");
    el?.scrollIntoView({ behavior: "smooth", block: "start" });
  });
}

// 内嵌 DSH 页面右键自定义菜单（复制/剪切/粘贴 ─ 刷新页面 / 重启服务器 / 显示隐藏桌宠）的位置；null = 关闭。
const ctxMenu = ref<{ x: number; y: number } | null>(null);

// 下载完成提示（会话日志导出等）：保存路径 + 自动消失计时器。
const toast = ref<{ text: string; path: string } | null>(null);
let toastTimer: number | undefined;

const installingNode = ref(false);
// nvm 切换确认（两步点击）：第一步亮出「确认切换」，避免误触直接改默认版本。
const confirmSwitchNode = ref(false);
const installingDsh = ref(false);
const latestVersion = ref<string | null>(null);
// DSH 更新进度文案（面板进度条 / 内嵌页更新 loading 共用）。
const dshUpdateMessage = ref("");
// DSH 更新/安装 loading 的已用时计时器（全屏 loading 显示「已用时 mm:ss」）。
const dshUpdateElapsed = ref("");
let dshUpdateStartedAt = 0;
let dshUpdateTimer: number | undefined;
// 鲸仔自更新状态：全屏遮罩显示下载 / 安装进度，直到应用退出重启。
const whalitoUpdating = ref(false);
const whalitoUpdateMessage = ref("");
// 后台更新提示：队列 + 当前展示项（一次可能同时发现 DSH 与鲸仔两个新版本；
// 更新进行中 / 忙碌时先排队，空闲再弹）。
const updateNotices = ref<UpdateNotice[]>([]);
const currentUpdateNotice = ref<UpdateNotice | null>(null);

// 视图：flow = 引导流程 / panel = 鲸仔管理后台 / embed = 内嵌页面
const view = ref<"flow" | "panel" | "embed">("flow");
const stage = ref<"detecting" | "node" | "dsh" | "server" | "wizard-done">("detecting");
const flowError = ref<string>("");
/** true = 当前是「重新运行安装引导」的体检向导（完成不自动启动/嵌页）。 */
const flowManual = ref(false);
const installStage = ref<string>("");

const embedNonce = ref(0);
const embedFrame = ref<HTMLIFrameElement | null>(null);
const whalitoVer = ref<WhalitoVersionInfo | null>(null);
let whalitoPingLogged = false;

const stageText: Record<string, string> = {
  install: "正在安装…",
  use: "正在切换版本…",
  download: "正在下载…",
  extract: "正在解压…",
  verify: "正在校验…",
  error: "安装失败",
};

// 引导流程的全屏 loading 阶段（检测环境 / 安装 DSH / 启动服务器）。
const flowLoading = computed(() => {
  return (
    stage.value === "detecting" || stage.value === "dsh" || stage.value === "server"
  );
});

const flowLoadingStatus = computed(() => {
  switch (stage.value) {
    case "dsh":
      return "安装 DeepSeek Harness 中";
    case "server":
      return "启动服务器中";
    default:
      return "检测环境中";
  }
});

const flowLoadingDetail = computed(() => {
  if (stage.value === "dsh") return stageText[installStage.value] ?? "准备中…";
  if (stage.value === "server") return "首次启动可能需要一点时间，请稍候…";
  return "";
});

const unlisteners: UnlistenFn[] = [];
let pollTimer: number | undefined;
let versionTimer: number | undefined;
let lastTrayRunning: boolean | null = null;

const phaseText: Record<string, string> = {
  stopped: "已停止",
  starting: "启动中",
  running: "运行中",
  external: "运行中（外部）",
  error: "异常",
};

// ============ 管理后台状态总览条 ============
interface OverviewItem {
  label: string;
  text: string;
  state: string; // ok | warn | bad | muted（对应 .dot 色类）
}

/** 管理后台顶部概览：Node / Harness / 服务器 三项状态一目了然。 */
const overviewItems = computed<OverviewItem[]>(() => {
  const e = env.value;
  const nodeState = !e?.found ? "bad" : e.nodeTooOld ? "warn" : "ok";
  const nodeText = !e?.found
    ? "未检测到"
    : e.nodeTooOld
      ? `版本过低 ${e.version ?? ""}`
      : `已安装 ${e.version ?? ""}`;
  const dshState = e?.dshInstalled ? "ok" : "bad";
  const dshText = e?.dshInstalled ? `已安装 ${e.dshVersion ?? ""}` : "未安装";
  const phase = server.value.phase;
  const serverState =
    phase === "running" || phase === "external"
      ? "ok"
      : phase === "starting"
        ? "warn"
        : phase === "error"
          ? "bad"
          : "muted";
  const serverText = phaseText[phase] ?? phase;
  return [
    { label: "Node.js", text: nodeText, state: nodeState },
    { label: "DeepSeek Harness", text: dshText, state: dshState },
    { label: "服务器", text: serverText, state: serverState },
  ];
});

/** 复制服务器地址到剪贴板（复用 Rust clipboard_write）。 */
async function copyServerUrl() {
  if (!server.value.url) return;
  const ok = await invoke<void>("clipboard_write", { text: server.value.url })
    .then(() => true)
    .catch(() => false);
  if (ok) showToast("已复制服务器地址", "");
  else postWhalitoError("复制服务器地址失败");
}

/** 隐藏到托盘：close() 触发 CloseRequested，非退出状态下被拦截为 hide。 */
function hideToTray() {
  getCurrentWindow().close();
}

async function wrap<T>(task: string, fn: () => Promise<T>): Promise<T | undefined> {
  busy.value = task;
  error.value = "";
  notice.value = "";
  try {
    return await fn();
  } catch (e) {
    error.value = typeof e === "string" ? e : String(e);
    return undefined;
  } finally {
    busy.value = null;
  }
}

async function refreshEnv() {
  try {
    env.value = await invoke<EnvInfo>("detect_env");
  } catch {
    env.value = null;
  }
}

async function refreshStatus() {
  try {
    server.value = await invoke<ServerStatus>("server_status");
    if (server.value.phase !== "external") confirmingStop.value = false;
    if (server.value.phase === "running") autoRestartCount.value = 0;
    syncTray();
  } catch {
    /* 忽略瞬时错误 */
  }
}

function syncTray() {
  const running = server.value.phase !== "stopped";
  if (running !== lastTrayRunning) {
    lastTrayRunning = running;
    invoke("update_tray_state", { running }).catch(() => {});
  }
}

async function refreshAll() {
  await Promise.all([refreshEnv(), refreshStatus()]);
}

async function installNode() {
  confirmSwitchNode.value = false;
  installingNode.value = true;
  installStage.value = "install";
  try {
    const r = await wrap("正在安装 Node.js…", () => invoke<EnvInfo>("install_node"));
    if (r) {
      env.value = r;
      await resumeFlow();
    }
  } finally {
    installingNode.value = false;
  }
}

async function upgradeNode() {
  confirmSwitchNode.value = false;
  installingNode.value = true;
  installStage.value = "install";
  try {
    const r = await wrap("正在升级 Node.js…", () => invoke<EnvInfo>("upgrade_node"));
    if (r) {
      env.value = r;
      await resumeFlow();
    }
  } finally {
    installingNode.value = false;
  }
}

async function installNodeNvm() {
  confirmSwitchNode.value = false;
  installingNode.value = true;
  installStage.value = "install";
  try {
    const r = await wrap("正在通过 nvm 安装 Node.js…", () => invoke<EnvInfo>("install_node_nvm"));
    if (r) {
      env.value = r;
      await resumeFlow();
    }
  } finally {
    installingNode.value = false;
  }
}

/** 切换到 nvm 已安装的合格版本（两步确认：第一次点击亮出确认，第二次执行）。 */
async function switchNodeNvm() {
  const target = env.value?.nvmSwitchVersion;
  if (!target) return;
  if (!confirmSwitchNode.value) {
    confirmSwitchNode.value = true;
    return;
  }
  confirmSwitchNode.value = false;
  installingNode.value = true;
  try {
    const r = await wrap(`正在切换到 Node ${target}…`, () =>
      invoke<EnvInfo>("switch_node_nvm", { version: target }),
    );
    if (r) {
      env.value = r;
      await resumeFlow();
    }
  } finally {
    installingNode.value = false;
  }
}

// ============ 后台更新提示（每小时检查弹窗） ============
/** 弹出下一个更新提示；更新进行中 / 忙碌时等待（watch 空闲后补弹）。 */
function maybeShowUpdateNotice() {
  if (installingDsh.value || whalitoUpdating.value || busy.value) return;
  if (currentUpdateNotice.value) return;
  const next = updateNotices.value.shift();
  if (next) currentUpdateNotice.value = next;
}

/** 暂不更新：该目标静默 24 小时（后端持久化），并弹下一条（如有）。 */
async function snoozeUpdate() {
  const n = currentUpdateNotice.value;
  if (!n) return;
  try {
    await invoke("snooze_update", { target: n.target });
  } catch (e) {
    postWhalitoError(typeof e === "string" ? e : String(e));
  }
  currentUpdateNotice.value = null;
  maybeShowUpdateNotice();
}

/** 立即更新：按目标走对应更新流程（弹窗本身就是确认，鲸仔跳过原生二次确认）。 */
function updateFromPopup() {
  const n = currentUpdateNotice.value;
  if (!n) return;
  currentUpdateNotice.value = null;
  if (n.target === "dsh") {
    void runDshUpdateFromPopup();
    return;
  }
  whalitoUpdating.value = true;
  whalitoUpdateMessage.value = "正在准备更新…";
  invoke("whalito_apply_update", { skipConfirm: true }).then(
    () => {
      // 正常返回只有「更新取消/无更新」路径；真正成功时应用已退出重启。
      whalitoUpdating.value = false;
      whalitoUpdateMessage.value = "";
    },
    (e) => {
      whalitoUpdating.value = false;
      whalitoUpdateMessage.value = "";
      postWhalitoError(typeof e === "string" ? e : String(e));
    },
  );
}

/** 弹窗触发的 DSH 更新：与设置分区 update-dsh 动作一致（停止 → 更新 → 启动）。 */
async function runDshUpdateFromPopup() {
  installingDsh.value = true;
  dshUpdateMessage.value = "正在更新 DeepSeek Harness…";
  postToDsh(embedFrame.value, {
    channel: "whalito",
    type: "update-progress",
    message: "正在更新 DeepSeek Harness…",
  });
  try {
    await performDshUpdate();
  } catch (e) {
    postWhalitoError(typeof e === "string" ? e : String(e));
    if (server.value.phase !== "running" && server.value.phase !== "external") {
      await startServer().catch(() => {});
    }
  } finally {
    await finishDshUpdate();
  }
}

/** 弹窗里的「查看详情」链接：交给系统浏览器打开。 */
function openNoticeUrl() {
  const n = currentUpdateNotice.value;
  if (n?.url) void invoke("open_url", { url: n.url }).catch(() => {});
}

// ============ 鲸仔更新（管理后台「关于」/ 启动失败修复入口） ============
/** 检查鲸仔是否有可用更新（silent=true 时失败不打扰，仅更新状态）。 */
async function checkWhalitoUpdateUi(silent = false) {
  try {
    const r = await invoke<WhalitoVersionInfo | null>("whalito_check_update");
    if (r) whalitoVer.value = r;
  } catch (e) {
    if (!silent) {
      error.value = `检查鲸仔更新失败：${typeof e === "string" ? e : String(e)}`;
    }
  }
}

/**
 * 更新鲸仔：先弹 Rust 原生确认框（测试版会提示无更新资产），确认后进入
 * 全屏「鲸仔更新中」并执行（命令末尾退出应用、由安装链重启，不 await）。
 */
function updateWhalitoNow() {
  void invoke<boolean>("confirm_whalito_update")
    .then((ok) => {
      if (!ok) return;
      whalitoUpdating.value = true;
      whalitoUpdateMessage.value = "正在准备更新…";
      invoke("whalito_apply_update", { skipConfirm: true }).then(
        () => {
          whalitoUpdating.value = false;
          whalitoUpdateMessage.value = "";
        },
        (e) => {
          whalitoUpdating.value = false;
          whalitoUpdateMessage.value = "";
          error.value = typeof e === "string" ? e : String(e);
        },
      );
    })
    .catch((e) => {
      error.value = typeof e === "string" ? e : String(e);
    });
}

async function installNodePortable() {
  confirmSwitchNode.value = false;
  installingNode.value = true;
  try {
    const dir = await invoke<string | null>("pick_node_dir");
    if (!dir) return;
    installStage.value = "download";
    const r = await wrap("正在下载并安装便携版 Node.js…", () =>
      invoke<EnvInfo>("install_node_portable", { dir }),
    );
    if (r) {
      env.value = r;
      settings.value = await invoke<Settings>("get_settings");
      await resumeFlow();
    }
  } finally {
    installingNode.value = false;
  }
}

async function installDsh() {
  installingDsh.value = true;
  installStage.value = "install";
  try {
    const r = await wrap("正在安装 DeepSeek Harness…", () => invoke<EnvInfo>("install_dsh"));
    if (r) env.value = r;
  } finally {
    installingDsh.value = false;
  }
}

// 统一 DSH 更新编排：停止服务器 → 更新安装 → 重新启动。
// 必须在停止服务器之后再执行 update_dsh：Windows 下运行中的服务器会锁定
// node_modules 里的原生模块，先删目录再重装会因文件锁导致半装/长耗时
//（面板残留「旧版本 + 更新中」的根因）。失败抛错，由调用方 catch 兜底恢复。
async function performDshUpdate() {
  const wasRunning =
    server.value.phase === "running" || server.value.phase === "external";
  if (wasRunning) {
    dshUpdateMessage.value = "正在停止服务器…";
    await doStop();
  }
  dshUpdateMessage.value = "正在更新 DeepSeek Harness…";
  const r = await invoke<EnvInfo>("update_dsh");
  env.value = r;
  await checkLatest();
  if (wasRunning) {
    dshUpdateMessage.value = "更新完成，正在启动服务器…";
    await startServer();
    embedNonce.value += 1;
  }
}

// 更新完成后兜底：清状态并刷新环境/服务器/版本快照（失败恢复由调用方 catch 负责）。
async function finishDshUpdate() {
  installingDsh.value = false;
  dshUpdateMessage.value = "";
  await refreshEnv();
  await refreshStatus();
  pushSnapshot();
}

// ============ DSH 更新/安装 loading 计时器 ============
// installingDsh 期间每秒刷新「已用时 mm:ss」，长时间更新也有明确反馈。
function tickDshUpdateTimer() {
  const total = Math.max(0, Math.floor((Date.now() - dshUpdateStartedAt) / 1000));
  const mm = String(Math.floor(total / 60)).padStart(2, "0");
  const ss = String(total % 60).padStart(2, "0");
  dshUpdateElapsed.value = `已用时 ${mm}:${ss}`;
}

function startDshUpdateTimer() {
  dshUpdateStartedAt = Date.now();
  tickDshUpdateTimer();
  if (dshUpdateTimer !== undefined) window.clearInterval(dshUpdateTimer);
  dshUpdateTimer = window.setInterval(tickDshUpdateTimer, 1000);
}

function stopDshUpdateTimer() {
  if (dshUpdateTimer !== undefined) {
    window.clearInterval(dshUpdateTimer);
    dshUpdateTimer = undefined;
  }
  dshUpdateElapsed.value = "";
}

// installingDsh 置位即开始计时（覆盖弹窗 / 设置页 / 引导流程三个入口），复位即停止。
watch(installingDsh, (v) => {
  if (v) startDshUpdateTimer();
  else stopDshUpdateTimer();
});

async function startServer() {
  autoRestartCount.value = 0;
  serverBusy.value = "启动服务器中";
  try {
    const r = await wrap("正在启动…", () => invoke<ServerStatus>("start_server"));
    if (r) server.value = r;
    else await refreshStatus(); // 失败：按真实状态同步，避免残留旧界面
  } finally {
    serverBusy.value = "";
  }
}

async function doStop() {
  serverBusy.value = "停止服务器中";
  try {
    const r = await wrap("正在停止…", () => invoke<ServerStatus>("stop_server"));
    if (r) server.value = r;
    else await refreshStatus();
  } finally {
    serverBusy.value = "";
  }
}

async function restartServer() {
  confirmingStop.value = false;
  serverBusy.value = "重启服务器中";
  try {
    const r = await wrap("正在重启…", () => invoke<ServerStatus>("restart_server"));
    if (r) server.value = r;
    else await refreshStatus(); // 重启失败：按真实状态同步，避免残留旧地址
  } finally {
    serverBusy.value = "";
  }
}

// ============ 内嵌 DSH 页面右键菜单 ============
/** 刷新页面：重新挂载 iframe（key 变化触发重建，等同浏览器刷新 DSH 页面）。 */
function ctxReload() {
  ctxMenu.value = null;
  embedNonce.value += 1;
}

/** 重启服务器：与设置分区里的「重启服务器」动作一致。 */
async function ctxRestart() {
  ctxMenu.value = null;
  await restartServer();
  embedNonce.value += 1;
  pushSnapshot();
}

/** 显示/隐藏桌宠：与托盘菜单一致——由 Rust 按真实状态翻转，前端只同步结果。 */
async function ctxTogglePet() {
  ctxMenu.value = null;
  const r = await wrap("正在更新桌宠…", () => invoke<boolean>("pet_toggle"));
  if (r !== undefined) {
    if (settings.value) settings.value.petEnabled = r;
    pushSnapshot();
  }
}

// ============ 内嵌页面右键剪贴板操作 ============
// 选区/光标在内嵌 iframe（跨源），剪贴板由父窗口经 Rust 代为读写：
// 复制 → 通知 iframe 读选区文本上行 → 写入系统剪贴板；
// 粘贴 → 父窗口读剪贴板 → 下发文本 → iframe 在光标处插入（组件自己的粘贴管线）。
function ctxCopy() {
  ctxMenu.value = null;
  diag("menu copy clicked");
  postToDsh(embedFrame.value, { channel: "whalito", type: "action", action: "context-copy" });
}

async function ctxPaste() {
  ctxMenu.value = null;
  diag("menu paste clicked");
  try {
    const text = await invoke<string>("clipboard_read");
    diag(`clipboard_read ok, len=${text.length}`);
    postToDsh(embedFrame.value, {
      channel: "whalito",
      type: "action",
      action: "context-paste",
      text,
    });
  } catch (e) {
    const msg = typeof e === "string" ? e : String(e);
    diag(`clipboard_read failed: ${msg}`);
    postWhalitoError(`读取剪贴板失败：${msg}`);
  }
}

/** 鲸仔面板自身右键不弹任何菜单（自定义菜单只服务内嵌 DSH 页面）。 */
function onPanelContextMenu(e: MouseEvent) {
  e.preventDefault();
  ctxMenu.value = null;
}

// ============ 下载提示 ============
/** 弹提示，durationMs 后自动消失；重复提示重置计时。下载提示默认 8s，复制提示 2s。 */
function showToast(text: string, path = "", durationMs = 8000) {
  toast.value = { text, path };
  if (toastTimer !== undefined) window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => {
    toast.value = null;
  }, durationMs);
}

/** 「打开所在文件夹」：在系统文件管理器里定位下载文件。 */
async function revealDownload(path: string) {
  await invoke("reveal_in_folder", { path }).catch((e) => {
    notice.value = typeof e === "string" ? e : String(e);
  });
}

/** 面板设置里的目录选择（工作目录 / 下载目录）。 */
async function pickWorkspaceDir() {
  if (!settings.value) return;
  const dir = await invoke<string | null>("pick_directory");
  if (dir) settings.value.workspaceDir = dir;
}

async function pickDownloadDir() {
  if (!settings.value) return;
  const dir = await invoke<string | null>("pick_directory");
  if (dir) settings.value.downloadDir = dir;
}

async function stopServer() {
  if (server.value.phase === "external" && !confirmingStop.value) {
    confirmingStop.value = true;
    return;
  }
  confirmingStop.value = false;
  await doStop();
}

async function stopServerTray() {
  confirmingStop.value = false;
  await doStop();
}

async function openUrl() {
  if (server.value.url) {
    await invoke("open_url", { url: server.value.url });
  }
}

function openEmbedded() {
  view.value = "embed";
}

// ============ 与内嵌 DSH 页面"鲸仔"设置分区通信 ============
function onEmbedLoad() {
  pushSnapshot();
}

/** 组装版本快照：DSH 来自环境检测 + 最近一次检查结果；鲸仔来自 Rust 命令缓存。 */
function buildVersions(): VersionsSnapshot {
  const dshCurrent = env.value?.dshVersion ?? null;
  return {
    dsh: {
      current: dshCurrent,
      latest: latestVersion.value,
      updateAvailable:
        latestVersion.value !== null &&
        dshCurrent !== null &&
        latestVersion.value !== dshCurrent,
    },
    whalito: whalitoVer.value
      ? toPlain(whalitoVer.value)
      : {
          current: null,
          testBuild: false,
          latest: null,
          updateAvailable: false,
          autoUpdate: false,
          url: null,
        },
  };
}

function pushSnapshot() {
  // 注意：必须 toPlain 去响应式——Vue reactive Proxy 过不了 postMessage
  // 的 structured clone（会抛 DataCloneError）；settings 未加载时也回握手。
  const err = postToDsh(embedFrame.value, {
    channel: "whalito",
    type: "hello",
    settings: settings.value ? toPlain(settings.value) : null,
    status: toPlain(server.value),
    versions: toPlain(buildVersions()),
    plugins: toPlain(plugins.value),
    dshPaths: toPlain(dshPaths.value),
  });
  if (err !== null) {
    invoke("bridge_diag", { line: `推送快照失败：${err}` }).catch(() => {});
  }
}

function postWhalitoError(message: string) {
  postToDsh(embedFrame.value, { channel: "whalito", type: "error", message });
}

/** 剪贴板链路诊断：写入 %TEMP%\whalito-bridge.log。 */
function diag(line: string) {
  invoke("bridge_diag", { line: `[clip] ${line}` }).catch(() => {});
}

function isPortValid(p: unknown): p is number {
  return typeof p === "number" && Number.isInteger(p) && p >= 1 && p <= 65535;
}

// 内嵌页消息串行队列：保存设置 / 检查更新 / 更新安装共享 latestVersion 等状态，
// 并发交错会让旧结果覆盖新结果（如：切回稳定版后仍显示预发布版的新版本）。
let whalitoQueue: Promise<void> = Promise.resolve();
function enqueueWhalito<T>(fn: () => Promise<T>): Promise<T> {
  const run = whalitoQueue.then(fn, fn);
  whalitoQueue = run.then(
    () => undefined,
    () => undefined,
  );
  return run;
}

async function handleWhalitoMessage(event: MessageEvent) {
  const origin = dshOrigin(server.value.url);
  if (origin !== null && event.origin !== origin) return;
  if (!isWhalitoMessage(event.data)) return;
  const msg = event.data;
  await enqueueWhalito(() => processWhalitoMessage(msg, event.origin));
}

async function processWhalitoMessage(msg: WhalitoMessage, eventOrigin: string) {
  if (msg.type === "ping") {
    if (!whalitoPingLogged) {
      whalitoPingLogged = true;
      pushLogRow("[系统] 鲸仔设置分区已连接（收到内嵌页握手请求）");
      invoke("bridge_diag", { line: `收到 ping，origin=${eventOrigin}` }).catch(() => {});
    }
    await loadPlugins();
    pushSnapshot();
    return;
  }
  if (msg.type !== "action") return;
  const action = msg.action;
  try {
    if (action === "save-settings") {
      const value = msg.value as WhalitoSettings | null;
      if (!value || !isPortValid(value.port)) {
        postWhalitoError("无效的端口（需要 1–65535 的整数）");
        return;
      }
      const prevPort = settings.value?.port;
      const r = await wrap("正在保存设置…", () =>
        invoke<Settings>("save_settings", { value }),
      );
      if (!r) {
        postWhalitoError(error.value || "保存设置失败");
        return;
      }
      settings.value = r;
      await wrap("正在更新开机自启…", () =>
        invoke<boolean>("set_autostart", { enabled: r.autostart }),
      );
      await wrap("正在更新桌宠…", () =>
        invoke<boolean>("pet_set_enabled", { enabled: r.petEnabled }),
      );
      settings.value = await invoke<Settings>("get_settings");
      // 保存后无条件重查版本：版本偏好/镜像源都会影响检查结果，
      // 无条件重查 + 串行队列保证最终显示与当前设置一致（不会残留旧通道结果）。
      latestVersion.value = null;
      await checkLatest();
      if (
        prevPort !== undefined &&
        r.port !== prevPort &&
        (server.value.phase === "running" || server.value.phase === "external")
      ) {
        notice.value = "端口已变更，正在重启服务器…";
        await restartServer();
        embedNonce.value += 1;
      }
      pushSnapshot();
      return;
    }
    if (action === "start") {
      await startServer();
      if (server.value.url) {
        view.value = "embed";
        embedNonce.value += 1;
      }
      pushSnapshot();
      return;
    }
    if (action === "stop") {
      await doStop();
      view.value = "panel";
      return;
    }
    if (action === "restart") {
      await restartServer();
      embedNonce.value += 1;
      pushSnapshot();
      return;
    }
    if (action === "focus-panel") {
      goPanel();
      return;
    }
    if (action === "toggle-plugin") {
      // 内置插件开关：改 patch 标记块（禁用 ≠ 卸载），重启服务器后生效。
      const v = msg.value as { id?: string; enabled?: boolean } | null;
      if (!v || typeof v.id !== "string" || typeof v.enabled !== "boolean") {
        postWhalitoError("无效的插件开关请求");
        return;
      }
      const list = await invoke<WhalitoPluginEntry[]>("toggle_plugin", {
        id: v.id,
        enabled: v.enabled,
      }).catch((e) => {
        postWhalitoError(typeof e === "string" ? e : String(e));
        return null;
      });
      if (list) {
        plugins.value = list;
        postToDsh(embedFrame.value, {
          channel: "whalito",
          type: "plugins",
          plugins: toPlain(list),
        });
      }
      return;
    }
    if (action === "install-market") {
      // dshmarket 被卸载后从「内置插件」tab 恢复安装：force=true 是用户显式
      // 意图（清除 installedOnce），否则已卸载过的用户会被「不自动装回」拒绝。
      const r = await invoke<string>("sync_market_plugin", { force: true }).catch((e) => {
        postWhalitoError(typeof e === "string" ? e : String(e));
        return null;
      });
      if (r) {
        postToDsh(embedFrame.value, {
          channel: "whalito",
          type: "notice",
          message: r,
        });
      }
      // 场景「禁用 dshmarket → 卸载 → 再安装」：patch 里残留 disabled 覆盖行，
      // 不清理的话重启后市场仍处于禁用态。安装成功即视为用户想用它，顺带启用。
      const market = plugins.value.find((p) => p.id === "dsh-market");
      if (market && market.disabled) {
        await invoke<WhalitoPluginEntry[]>("toggle_plugin", {
          id: "dsh-market",
          enabled: true,
        }).catch(() => {});
      }
      // 刷新本地插件状态并推送快照（分区「内置插件」tab 要看到安装后的变化）。
      await loadPlugins();
      pushSnapshot();
      return;
    }
    if (action === "restart-server") {
      await restartServer();
      embedNonce.value += 1;
      pushSnapshot();
      return;
    }
    if (action === "dsh-path-toggle") {
      // 一键注册/注销 dsh 到 PATH（user / system 两级）；只影响新开的终端。
      const v = msg.value as { enable?: boolean; level?: string } | null;
      const level = v?.level === "system" ? "system" : "user";
      if (!v || typeof v.enable !== "boolean") {
        postWhalitoError("无效的 PATH 注册请求");
        return;
      }
      const r = await invoke<DshPathView>("dsh_path_toggle", {
        enable: v.enable,
        level,
      }).catch((e) => {
        postWhalitoError(typeof e === "string" ? e : String(e));
        return null;
      });
      if (r) {
        dshPaths.value = { ...dshPaths.value, [level]: r };
        postToDsh(embedFrame.value, {
          channel: "whalito",
          type: "dsh-path",
          dshPath: toPlain(r),
        });
      }
      return;
    }
    if (action === "check-update") {
      const target = msg.target;
      if (target === "dsh") {
        await checkLatest();
        if (latestVersion.value === null) {
          postWhalitoError("无法获取 DSH 最新版本（检查失败或已是最新）");
        }
      } else if (target === "whalito") {
        const r = await invoke<WhalitoVersionInfo>("whalito_check_update").catch((e) => {
          postWhalitoError(typeof e === "string" ? e : String(e));
          return null;
        });
        whalitoVer.value = r ?? whalitoVer.value;
      }
      pushSnapshot();
      return;
    }
    if (action === "update-dsh") {
      // 升级前先让用户确认「已备份插件信息」（鲸仔会自动备份配置与插件，
      // 双保险再确认一次）；取消则不开始升级。
      const confirmed = await invoke<boolean>("confirm_dsh_update").catch(() => false);
      if (!confirmed) {
        postToDsh(embedFrame.value, {
          channel: "whalito",
          type: "notice",
          message: "已取消升级（未执行备份确认）",
        });
        return;
      }
      // 设置页「立即更新」：停止 → 更新 → 启动；进度经 install-stage → update-progress 回传。
      installingDsh.value = true;
      dshUpdateMessage.value = "正在更新 DeepSeek Harness…";
      postToDsh(embedFrame.value, {
        channel: "whalito",
        type: "update-progress",
        message: "正在更新 DeepSeek Harness…",
      });
      try {
        await performDshUpdate();
      } catch (e) {
        postWhalitoError(typeof e === "string" ? e : String(e));
        if (server.value.phase !== "running" && server.value.phase !== "external") {
          await startServer().catch(() => {});
        }
      } finally {
        await finishDshUpdate();
      }
      return;
    }
    if (action === "open-url") {
      const url = msg.url;
      if (typeof url === "string" && url.startsWith("https://")) {
        await invoke("open_url", { url }).catch((e) => postWhalitoError(String(e)));
      } else {
        postWhalitoError("无效的下载地址");
      }
      return;
    }
    if (action === "apply-update") {
      // 先弹原生确认框（此时全屏 loading 尚未打开）；用户确认后才进入
      // 「正在更新」状态并执行（skipConfirm=true 跳过 Rust 内部二次确认）。
      const confirmed = await invoke<boolean>("confirm_whalito_update").catch(() => false);
      if (!confirmed) {
        postToDsh(embedFrame.value, {
          channel: "whalito",
          type: "notice",
          message: "已取消更新",
        });
        return;
      }
      // 命令末尾会退出应用（安装链接管重启），不 await；失败时回传错误。
      // 确认后才进入全屏「正在更新」状态，让下载/安装进度可见。
      whalitoUpdating.value = true;
      whalitoUpdateMessage.value = "正在准备更新…";
      invoke("whalito_apply_update", { skipConfirm: true }).then(
        () => {
          // 命令正常返回只有一种情况：更新链异常提前返回（成功时应用已退出）。
          whalitoUpdating.value = false;
          whalitoUpdateMessage.value = "";
        },
        (e) => {
          whalitoUpdating.value = false;
          whalitoUpdateMessage.value = "";
          postWhalitoError(typeof e === "string" ? e : String(e));
        },
      );
      return;
    }
    if (action === "context-menu") {
      // 仅在内嵌 DSH 页面视图弹出（面板视图不显示右键菜单），并做边缘收拢。
      if (view.value === "embed" && typeof msg.x === "number" && typeof msg.y === "number") {
        const menuWidth = 170;
        const menuHeight = 250;
        const margin = 8;
        ctxMenu.value = {
          x: Math.max(margin, Math.min(msg.x, window.innerWidth - menuWidth - margin)),
          y: Math.max(margin, Math.min(msg.y, window.innerHeight - menuHeight - margin)),
        };
      }
      return;
    }
    if (action === "context-menu-close") {
      ctxMenu.value = null;
      return;
    }
    if (action === "clipboard-set") {
      // 内嵌页面复制：选区文本上行到这里 → 写入系统剪贴板。
      const text = msg.text ?? "";
      if (!text) return;
      diag(`clipboard-set received, len=${text.length}, dbg=${JSON.stringify(msg.dbg ?? null)}`);
      const ok = await invoke<void>("clipboard_write", { text })
        .then(() => true)
        .catch((e) => {
          diag(`clipboard_write failed: ${typeof e === "string" ? e : String(e)}`);
          return false;
        });
      if (ok) {
        diag("clipboard_write ok");
        showToast("已复制到剪贴板", "", 2000);
      } else {
        postWhalitoError("复制到剪贴板失败");
      }
      return;
    }
    if (action === "clipboard-noop") {
      diag(`clipboard-noop: ${typeof msg.why === "string" ? msg.why : "?"}`);
      return;
    }
    if (action === "pick-directory") {
      // DSH 设置分区请求原生目录选择：选完经 picked-dir 消息回填草稿。
      const dir = await invoke<string | null>("pick_directory").catch(() => null);
      if (dir && (msg.field === "workspaceDir" || msg.field === "downloadDir")) {
        postToDsh(embedFrame.value, {
          channel: "whalito",
          type: "picked-dir",
          field: msg.field,
          path: dir,
        });
      }
      return;
    }
    if (action === "whalito-download") {
      // 会话日志导出下载：由鲸仔下载到配置目录，完成弹提示。
      if (typeof msg.url === "string" && typeof msg.filename === "string") {
        const path = await invoke<string>("whalito_download", {
          url: msg.url,
          filename: msg.filename,
        }).catch((e) => {
          postWhalitoError(typeof e === "string" ? e : String(e));
          return null;
        });
        if (path !== null) showToast("会话日志已保存", path);
      }
      return;
    }
    postWhalitoError(`未知动作：${action ?? ""}`);
  } catch (e) {
    postWhalitoError(typeof e === "string" ? e : String(e));
  }
}

async function refreshLogs() {
  const raw = await invoke<string[]>("get_logs").catch(() => [] as string[]);
  logs.value = raw.map((text) => makeLogRow(text));
  logHidden.value = Math.max(0, logs.value.length - LOG_WINDOW);
  requestAnimationFrame(() => autoScrollIfPinned(true));
}

function clearLogs() {
  logs.value = [];
  logHidden.value = 0;
  expandedLogIds.value = new Set();
}

/** 加载更早：回退一页。 */
function loadEarlierLogs() {
  logHidden.value = Math.max(0, logHidden.value - LOG_PAGE);
}

/** 复制所有错误行（时间 + 内容）。 */
async function copyErrors() {
  const rows = logErrorRows();
  if (!rows.length) return;
  await copyText(rows.map((r) => `[${fmtTime(r.ts)}] ${r.text}`).join("\n"));
}

/** 复制错误日志到 AI（带版本上下文提示词）。 */
async function copyErrorsToAi() {
  const rows = logErrorRows();
  if (!rows.length) return;
  await copyText(toAiPrompt(rows));
}

function fmtTime(ts: number): string {
  const d = new Date(ts);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** 单行复制 / 复制到 AI（供日志行按钮使用）。 */
async function copyLogRow(row: LogRow, toAi: boolean) {
  await copyText(toAi ? toAiPrompt([row]) : row.text);
}

async function loadSettings() {
  settings.value = await invoke<Settings>("get_settings");
  whalitoVer.value = await invoke<WhalitoVersionInfo>("whalito_version_info").catch(
    () => null,
  );
}

async function saveSettings() {
  if (!settings.value) return;
  const r = await wrap("正在保存设置…", () =>
    invoke<Settings>("save_settings", { value: settings.value }),
  );
  if (r) {
    settings.value = r;
    notice.value = "设置已保存";
    showSettings.value = false;
    pushSnapshot();
  }
}

async function toggleAutostart() {
  if (!settings.value) return;
  const enabled = settings.value.autostart;
  const r = await wrap("正在更新开机自启…", () =>
    invoke<boolean>("set_autostart", { enabled }),
  );
  if (r !== undefined && settings.value) {
    settings.value.autostart = r;
    pushSnapshot();
  }
}

async function togglePet() {
  if (!settings.value) return;
  const enabled = settings.value.petEnabled;
  const r = await wrap("正在更新桌宠…", () =>
    invoke<boolean>("pet_set_enabled", { enabled }),
  );
  if (r !== undefined && settings.value) {
    settings.value.petEnabled = r;
    pushSnapshot();
  }
}

async function checkLatest() {
  try {
    latestVersion.value = await invoke<string | null>("check_latest_version");
  } catch {
    /* 忽略 */
  }
}

/** 主流程编排：检测 → 装 Node / 装 dsh / 启动服务器 → 内嵌打开。 */
async function runFlow() {
  flowManual.value = false;
  flowError.value = "";
  await refreshAll();

  const e = env.value;
  if (!e) {
    stage.value = "detecting";
    flowError.value = "环境检测失败，请重试。";
    return;
  }

  // 1. Node 缺失或版本过低 → 提示安装（等待用户选择）
  if (!e.found || e.nodeTooOld) {
    stage.value = "node";
    return;
  }

  // 2. Node 正常、未装 dsh → 自动安装
  if (!e.dshInstalled) {
    stage.value = "dsh";
    await installDsh();
    if (!env.value?.dshInstalled) {
      flowError.value = "DeepSeek Harness 安装未完成，请重试或查看日志。";
      return;
    }
  }

  // 3. 确保服务器运行
  stage.value = "server";
  await refreshStatus();
  if (server.value.phase !== "running" && server.value.phase !== "external") {
    await startServer();
    if (server.value.phase !== "running") {
      flowError.value = "服务器启动失败，请重试；仍失败可「尝试更新鲸仔」或在管理后台禁用插件。";
      // 静默探测鲸仔更新，有新版本则错误卡显示「尝试更新鲸仔」。
      void checkWhalitoUpdateUi(true);
      return;
    }
  }

  // 4. 内嵌打开（同一窗口）
  if (server.value.url) {
    view.value = "embed";
  } else {
    flowError.value = "未能获取服务器地址，请重试。";
  }
}

/**
 * 「重新运行安装引导」（关于）：环境体检向导，只检测/安装，**不自动启动
 * 服务器、不内嵌跳转**；结束后停在结果卡，由用户选择启动或返回。
 */
async function runWizard() {
  flowManual.value = true;
  flowError.value = "";
  autoRestartFailed.value = false;
  view.value = "flow";
  await refreshAll();

  const e = env.value;
  if (!e) {
    stage.value = "detecting";
    flowError.value = "环境检测失败，请重试。";
    return;
  }
  if (!e.found || e.nodeTooOld) {
    // Node 缺失/过低：进入安装卡片；完成后 resumeFlow 续跑本向导。
    stage.value = "node";
    return;
  }
  if (!e.dshInstalled) {
    stage.value = "dsh";
    await installDsh();
    await refreshEnv();
    if (!env.value?.dshInstalled) {
      stage.value = "detecting";
      flowError.value = "DeepSeek Harness 安装未完成，请重试或查看日志。";
      return;
    }
  }
  await refreshStatus();
  stage.value = "wizard-done";
}

/** Node 安装/切换等子流程完成后，按来源续跑：向导 → runWizard，引导 → runFlow。 */
function resumeFlow() {
  if (flowManual.value) void runWizard();
  else void runFlow();
}

/** 向导结果卡的「启动服务器」：启动后留在结果卡（不内嵌跳转）。 */
async function startFromWizard() {
  await startServer();
  await refreshStatus();
}

/** 向导里的「重新安装 / 修复 Harness」：走既有更新链路（备份确认 → 停服 →
 * 下载安装 → 重启），完成后回到结果卡；失败把错误留在结果卡上。 */
async function repairDshFromWizard() {
  const confirmed = await invoke<boolean>("confirm_dsh_update").catch(() => false);
  if (!confirmed) return;
  installingDsh.value = true;
  dshUpdateMessage.value = "正在重新安装 DeepSeek Harness…";
  try {
    await performDshUpdate();
  } catch (e) {
    flowError.value = `重新安装失败：${typeof e === "string" ? e : String(e)}`;
  } finally {
    installingDsh.value = false;
    await refreshAll();
    await refreshStatus();
    if (env.value?.dshInstalled) stage.value = "wizard-done";
    else stage.value = "detecting";
  }
}

function goPanel() {
  view.value = "panel";
  flowManual.value = false;
  autoRestartFailed.value = false;
}

onMounted(async () => {
  unlisteners.push(
    await listen<string>("log", (e) => {
      pushLogRow(e.payload);
    }),
  );
  unlisteners.push(
    await listen<string>("server-url", (e) => {
      server.value.url = e.payload;
    }),
  );
  // DSH 会话 cookie 已捕获并注入 WebView2（Windows）：若内嵌 iframe 已因
  // 未认证 401 白屏（跨站 iframe 拿不到 SameSite=Strict 握手 cookie），
  // 重建一次即可恢复；注入先于本事件完成，正常时序下首屏直接成功。
  unlisteners.push(
    await listen<null>("dsh-auth-ready", () => {
      if (view.value === "embed" && server.value.url) {
        embedNonce.value += 1;
      }
    }),
  );
  unlisteners.push(
    await listen<string>("install-stage", (e) => {
      installStage.value = e.payload;
      // 同步到内嵌页更新 loading 文案。
      dshUpdateMessage.value = stageText[e.payload] ?? e.payload;
      // 安装阶段回传设置分区（update-dsh / 面板安装都可见进度）。
      postToDsh(embedFrame.value, {
        channel: "whalito",
        type: "update-progress",
        message: stageText[e.payload] ?? e.payload,
      });
    }),
  );
  unlisteners.push(
    await listen<number>("server-exited", async () => {
      server.value = await invoke<ServerStatus>("server_status");
      if (settings.value?.autoRestart && autoRestartCount.value < MAX_AUTO_RESTART) {
        autoRestartCount.value += 1;
        pushLogRow(
          `[系统] 服务器异常退出，自动重启（第 ${autoRestartCount.value}/${MAX_AUTO_RESTART} 次）`,
        );
        const r = await invoke<ServerStatus>("start_server").catch(() => null);
        if (r) server.value = r;
      } else if (autoRestartCount.value >= MAX_AUTO_RESTART) {
        error.value = `服务器连续 ${MAX_AUTO_RESTART} 次启动失败，已停止自动重试。可在下方「尝试更新鲸仔」或「插件管理」中修复。`;
        autoRestartFailed.value = true;
        // 静默探测鲸仔更新，有新版本则在面板错误横幅旁给出更新按钮。
        void checkWhalitoUpdateUi(true);
        // 桌宠同步提醒（即使主窗口在托盘/后台，桌宠气泡也可见）。
        void emit("pet-alert", {
          kind: "system",
          key: "server-restart-failed",
          text: `服务器连续 ${MAX_AUTO_RESTART} 次启动失败，已停止自动重试。可尝试更新鲸仔或禁用插件后再启动。`,
        });
      }
    }),
  );
  // DSH 被插件（dsh-market 等）自重启后，鲸仔按端口接管新进程并发本事件。
  unlisteners.push(
    await listen<number>("server-restarted", async (e) => {
      server.value = await invoke<ServerStatus>("server_status").catch(() => server.value);
      notice.value = `DSH 已自行重启（插件市场等触发），鲸仔已重新接管（新进程 ${e.payload}）`;
      // 内嵌页重载以恢复连接（iframe 因 :key 变化重建）。
      embedNonce.value += 1;
      window.setTimeout(() => {
        if (notice.value.startsWith("DSH 已自行重启")) notice.value = "";
      }, 8000);
    }),
  );
  unlisteners.push(
    await listen<string>("tray-action", (e) => {
      if (e.payload === "start") startServer();
      else if (e.payload === "stop") stopServerTray();
      else if (e.payload === "open") {
        if (server.value.url) view.value = "embed";
        else view.value = "panel";
      }
    }),
  );
  // 桌宠请求唤起主界面：只切换视图并聚焦，不重载 iframe
  // （审批 / 提问经 SSE 实时到达，前端连接保持存活）。
  unlisteners.push(
    await listen<string | null>("pet-open-session", () => {
      if (server.value.url) {
        view.value = "embed";
      } else {
        view.value = "panel";
      }
    }),
  );
  unlisteners.push(
    await listen<string>("whalito-update", (e) => {
      whalitoUpdating.value = true;
      whalitoUpdateMessage.value = e.payload;
      postToDsh(embedFrame.value, {
        channel: "whalito",
        type: "update-progress",
        message: e.payload,
      });
    }),
  );
  // 后台每小时检查发现的更新：入队并尝试弹出（更新进行中则等空闲）。
  unlisteners.push(
    await listen<UpdateNotice>("update-available", (e) => {
      const n = e.payload;
      if (!n || (n.target !== "dsh" && n.target !== "whalito")) return;
      // 同一目标已在队列/展示中则忽略（事件重复到达防抖）。
      if (
        currentUpdateNotice.value?.target === n.target ||
        updateNotices.value.some((x) => x.target === n.target)
      ) {
        return;
      }
      updateNotices.value.push(n);
      maybeShowUpdateNotice();
    }),
  );
  window.addEventListener("message", handleWhalitoMessage);
  // 鲸仔面板右键不弹菜单；iframe 内的右键事件不会冒泡到这里，
  // 所以只影响面板自身。
  window.addEventListener("contextmenu", onPanelContextMenu);

  await Promise.all([loadSettings(), refreshLogs()]);
  // 状态快照（插件 / PATH）加载完成后补推一次，避免分区首屏 hello 拿到空数据
  // 而一直显示「查询中… / 空列表」（此前 load 完成后不推送，连接后 ping 停止）。
  await Promise.all([loadMarketStatus(), loadPlugins(), loadDshPaths()]);
  pushSnapshot();
  platform.value = await invoke<string>("get_platform").catch(() => "windows");
  // 鲸仔自更新重启后：校验更新是否成功（标记一次性，读后即删）。
  invoke<UpdateMarkerResult | null>("whalito_update_result")
    .then((r) => {
      if (!r) return;
      if (r.success) showToast(`已成功更新到 v${r.to}`, "");
      else
        error.value = `鲸仔更新未完成：当前仍为 v${r.from}，目标 v${r.to}，请重试或检查网络。`;
    })
    .catch(() => {});
  pollTimer = window.setInterval(refreshStatus, 3000);
  await runFlow();
  // 自动检查更新（DSH）关闭时不启动静默版本查询（启动时初始检查 + 每 5 分钟
  // 轮询都属于「自动检查」；设置分区的手动「检查更新」按钮不受影响）。
  if (settings.value?.dshAutoCheckUpdate) {
    checkLatest();
    versionTimer = window.setInterval(checkLatest, 5 * 60 * 1000);
  }
});

onUnmounted(() => {
  window.removeEventListener("message", handleWhalitoMessage);
  window.removeEventListener("contextmenu", onPanelContextMenu);
  if (toastTimer !== undefined) window.clearTimeout(toastTimer);
  unlisteners.forEach((u) => u());
  if (pollTimer) window.clearInterval(pollTimer);
  if (versionTimer) window.clearInterval(versionTimer);
  if (dshUpdateTimer !== undefined) window.clearInterval(dshUpdateTimer);
});

// 离开内嵌页视图时收起右键菜单。
watch(view, () => {
  ctxMenu.value = null;
});

// 更新进行中 / 忙碌结束时补弹排队中的更新提示。
watch([installingDsh, whalitoUpdating, busy], maybeShowUpdateNotice);
</script>

<template>
  <!-- ============ 内嵌页面（单窗口） ============ -->
  <div v-if="view === 'embed'" class="embed">
    <!-- DSH 更新 / 服务器启停重启中：全屏统一 loading，替代「服务器未运行」闪屏 -->
    <LoadingScreen
      v-if="installingDsh"
      status="更新 DeepSeek Harness 中"
      :detail="dshUpdateMessage"
      :timer="dshUpdateElapsed"
    />
    <LoadingScreen
      v-else-if="serverBusy"
      :status="serverBusy"
      detail="服务器状态切换中，请稍候…"
    />
    <template v-else>
      <iframe
        v-if="server.url"
        ref="embedFrame"
        :key="embedNonce"
        :src="server.url"
        class="embed-frame"
        @load="onEmbedLoad"
      />
      <div v-else class="embed-empty">
        <p>服务器未运行</p>
        <div class="row">
          <button class="primary" @click="startServer">启动服务器</button>
          <button @click="goPanel">进入鲸仔管理后台</button>
          <button @click="showSettings = true">打开设置</button>
        </div>
      </div>
    </template>
  </div>

  <!-- ============ 主窗口：流程 / 面板 ============ -->
  <div v-else class="app">
    <!-- 引导式主流程 -->
    <div v-if="view === 'flow'" class="flow">
      <!-- 全屏统一 loading：检测环境 / 安装 DSH / 启动服务器（出错时自动切回卡片） -->
      <template v-if="flowLoading && !flowError">
        <LoadingScreen
          :status="flowLoadingStatus"
          :detail="flowLoadingDetail"
          :timer="dshUpdateElapsed"
        />
        <button class="ghost link flow-exit" @click="goPanel">进入鲸仔管理后台</button>
      </template>
      <template v-else>
        <div class="flow-card">
          <div class="flow-brand">
            <img class="flow-brand-logo" src="/logo.png" alt="鲸仔" />
            <div>
              <h1>鲸仔</h1>
              <p class="sub en">Whalito</p>
            </div>
          </div>

          <!-- 出错：错误信息 + 重试（loading 阶段失败也会走到这里，不会无限转圈） -->
          <template v-if="flowError">
            <p class="flow-title">操作未完成</p>
            <p class="banner error">{{ flowError }}</p>
            <div class="flow-actions">
              <button class="primary" @click="resumeFlow">重试</button>
              <!-- 启动失败等事故场景：优先引导「更新鲸仔」与「禁用插件」恢复 -->
              <button
                v-if="whalitoUpdateAvailable"
                class="primary"
                :disabled="whalitoUpdating"
                @click="updateWhalitoNow"
              >
                尝试更新鲸仔
              </button>
              <button class="ghost" @click="focusPluginsCard">管理插件（禁用异常插件）</button>
              <button class="ghost" @click="goPanel">进入鲸仔管理后台</button>
            </div>
          </template>

          <!-- Node 缺失 / 版本过低 -->
          <template v-else-if="stage === 'node'">
            <p class="flow-title">
              {{ env?.found ? `当前 Node 版本过低（${env?.version}）` : "未检测到 Node.js" }}
            </p>

            <!-- nvm 里已有合格版本：优先提示切换，无需重新安装 -->
            <template v-if="env?.nvmSwitchVersion">
              <p class="hint good">
                检测到 nvm 中已安装更高版本 Node {{ env.nvmSwitchVersion }}，可直接切换使用，无需重新下载安装。
              </p>
              <div class="flow-actions">
                <button class="primary" :disabled="installingNode" @click="switchNodeNvm">
                  {{
                    installingNode
                      ? "正在切换…"
                      : confirmSwitchNode
                        ? `再次点击确认切换到 Node ${env.nvmSwitchVersion}`
                        : `切换到 Node ${env.nvmSwitchVersion}（nvm 已安装）`
                  }}
                </button>
                <button
                  v-if="confirmSwitchNode"
                  :disabled="installingNode"
                  @click="confirmSwitchNode = false"
                >
                  取消
                </button>
              </div>
              <p class="hint">切换会执行 nvm use，把当前默认 Node 改为 {{ env.nvmSwitchVersion }}。也可以选择下方其他方式：</p>
            </template>
            <p v-else class="hint">DeepSeek Harness 需要 Node.js ≥ 22.19.0。请选择一种安装方式：</p>

            <div class="flow-actions">
              <button
                v-if="env?.nvmFound"
                class="primary"
                :disabled="installingNode"
                @click="installNodeNvm"
              >
                {{ installingNode ? "正在安装…" : "用 nvm 安装 Node 22" }}
              </button>
              <button
                class="primary"
                :disabled="installingNode"
                @click="env?.found ? upgradeNode() : installNode()"
              >
                {{
                  installingNode
                    ? "正在安装…"
                    : env?.found
                      ? platform === "windows"
                        ? "一键升级（winget）"
                        : "一键升级 Node"
                      : platform === "windows"
                        ? "一键安装（winget）"
                        : "一键安装 Node 22"
                }}
              </button>
              <button :disabled="installingNode" @click="installNodePortable">
                自定义安装目录…
              </button>
            </div>
            <p v-if="env?.nvmFound" class="hint good">已检测到 nvm（{{ env.nvmPath }}）</p>
            <p v-if="platform === 'macos'" class="hint">
              鲸仔将下载 Node.js 22 官方安装包到「~/Library/Application
              Support」，无需管理员权限。
            </p>
          </template>

          <!-- 体检向导结果（「重新运行安装引导」）：只检测/修复，不自动启动与嵌页 -->
          <template v-else-if="stage === 'wizard-done'">
            <p class="flow-title">环境体检完成</p>
            <div class="wizard-report">
              <p>
                <span class="hint">Node.js：</span>
                <b>{{ env?.version ?? "—" }}</b>
              </p>
              <p>
                <span class="hint">DeepSeek Harness：</span>
                <b>{{ env?.dshVersion ?? "未安装" }}</b>
              </p>
              <p>
                <span class="hint">服务器：</span>
                <b :class="server.phase === 'running' ? 'good' : 'warn'">
                  {{
                    server.phase === "running"
                      ? "运行中"
                      : server.phase === "external"
                        ? "外部运行中"
                        : "未运行"
                  }}
                </b>
              </p>
            </div>
            <p v-if="installingDsh" class="banner busy">⏳ {{ dshUpdateMessage || "正在重新安装…" }}</p>
            <div class="flow-actions">
              <button
                v-if="server.phase !== 'running' && server.phase !== 'external'"
                class="primary"
                :disabled="installingDsh || busy !== null"
                @click="startFromWizard"
              >
                启动服务器
              </button>
              <button class="ghost" :disabled="installingDsh" @click="repairDshFromWizard">
                重新安装 / 修复 Harness
              </button>
              <button class="ghost" @click="goPanel">返回管理后台</button>
            </div>
            <p class="hint">
              向导只做检测与安装/修复，不会自动启动服务器或跳转页面；启动后可在管理后台或
              DSH 页面操作。
            </p>
          </template>

          <p v-if="busy" class="banner busy">⏳ {{ busy }}</p>
          <p v-if="error && !flowError" class="banner error">{{ error }}</p>

          <button
            v-if="stage === 'node' && !flowError"
            class="ghost link"
            @click="goPanel"
          >
            进入鲸仔管理后台
          </button>
        </div>
      </template>
    </div>

    <!-- 鲸仔管理后台 -->
    <template v-else>
      <header class="topbar">
        <div class="brand">
          <img class="brand-logo" src="/logo.png" alt="鲸仔" />
          <div>
            <h1>鲸仔管理后台</h1>
            <p class="sub en">Whalito</p>
            <p class="sub">一键安装 · 启动 · 管理你的 Harness</p>
          </div>
        </div>
        <div class="top-actions">
          <button class="ghost" @click="showSettings = true">设置</button>
        </div>
      </header>

      <!-- 状态总览：Node / Harness / 服务器 一眼看清 -->
      <div class="overview">
        <div
          v-for="item in overviewItems"
          :key="item.label"
          class="overview-item"
          :class="item.state"
        >
          <span class="overview-title">{{ item.label }}</span>
          <span class="overview-value">{{ item.text }}</span>
        </div>
      </div>

      <div v-if="installingNode || installingDsh" class="progress-track">
        <div class="progress-bar"></div>
        <p v-if="dshUpdateMessage" class="hint">{{ dshUpdateMessage }}</p>
      </div>

      <p v-if="error" class="banner error">{{ error }}</p>
      <!-- 连续自动重启失败：给出「更新鲸仔 / 插件管理」两个恢复动作 -->
      <div v-if="autoRestartFailed" class="row fix-row">
        <button
          v-if="whalitoUpdateAvailable"
          class="primary small"
          :disabled="whalitoUpdating"
          @click="updateWhalitoNow"
        >
          {{ whalitoUpdating ? "正在更新…" : `尝试更新鲸仔到 v${whalitoVer?.latest}` }}
        </button>
        <button v-else class="ghost small" :disabled="whalitoUpdating" @click="checkWhalitoUpdateUi()">
          检查鲸仔更新
        </button>
        <button class="ghost small" @click="focusPluginsCard">管理插件（禁用异常插件）</button>
      </div>
      <p v-if="notice" class="banner notice">{{ notice }}</p>
      <p v-if="busy" class="banner busy">⏳ {{ busy }}</p>

      <!-- 服务器操作工具栏（去卡片化后保留的高频操作） -->
      <div class="server-tools">
        <button v-if="server.phase === 'stopped'" class="primary" @click="startServer">启动服务器</button>
        <button v-else-if="server.phase === 'error'" class="primary" @click="startServer">重新启动</button>
        <button v-else class="danger" @click="stopServer">
          {{ server.phase === "external" && confirmingStop ? "再次点击确认停止" : "停止服务器" }}
        </button>
        <button v-if="server.phase === 'external' && confirmingStop" class="ghost" @click="confirmingStop = false">取消</button>
        <button v-if="server.phase === 'running' || server.phase === 'external'" class="ghost" @click="restartServer">重启服务器</button>
        <button v-if="server.url" class="primary" @click="openEmbedded">应用内打开</button>
        <button v-if="server.url" @click="openUrl">在浏览器打开</button>
        <button v-if="server.url" @click="copyServerUrl">复制地址</button>
      </div>
      <p v-if="server.phase === 'external'" class="hint warn">
        该服务器由外部启动；点击「停止服务器」将按端口定位并结束对应进程（需二次确认）。
      </p>

      <!-- 双栏：主栏=运行日志；侧栏=插件市场 / 插件管理 / 关于（紧凑卡片） -->
      <div class="panel-grid">
      <!-- 插件市场（dsh-market）：启动前自动预装，这里可手动检查 / 显式重装 -->
      <div class="market-row panel-area-market">
        <span class="hint">
          插件市场：
          <b>{{
            marketInstalled
              ? "已就绪（DSH 设置页可见）"
              : marketOnce
                ? "已卸载（鲸仔不会自动装回）"
                : "未安装（首次启动时自动安装）"
          }}</b>
        </span>
        <button class="ghost small" :disabled="!!busy" @click="syncMarket(false)">检查 / 安装</button>
        <button
          v-if="marketOnce && !marketInstalled"
          class="ghost small"
          :disabled="!!busy"
          @click="syncMarket(true)"
        >
          重新安装
        </button>
      </div>

      <!-- 插件管理：禁用可能打断 DSH 启动的插件（bundle 层，含市场安装的）。
           禁用≠卸载：只写 disabled 覆盖行，重新启用即恢复；改动重启后生效。 -->
      <section id="panel-plugins" class="card plugins panel-area-plugins">
        <div class="plugins-head">
          <h2>插件管理</h2>
          <button
            v-if="server.phase === 'running' || server.phase === 'external'"
            class="ghost small"
            @click="restartServer"
          >
            重启服务器（使改动生效）
          </button>
        </div>
        <p class="hint">
          插件市场安装的插件可能导致 DSH 启动失败；DSH 起不来时可在此先禁用再重启，保障服务优先可用。禁用≠卸载。
        </p>
        <div v-if="plugins.length === 0" class="empty">暂无已安装插件</div>
        <div v-for="p in plugins" :key="p.id" class="plugin-row">
          <div class="plugin-info">
            <span class="plugin-name">
              {{ p.name }}
              <span v-if="p.builtin" class="badge-soft">内置</span>
            </span>
            <span class="hint plugin-desc">{{ p.description }}</span>
          </div>
          <div class="plugin-actions">
            <span v-if="p.installable && !p.installed" class="hint">未安装</span>
            <span v-else class="hint" :class="p.disabled ? 'warn' : 'good'">
              {{ p.disabled ? "已禁用（重启后不加载）" : "加载中" }}
            </span>
            <button
              v-if="p.installable && !p.installed"
              class="ghost small"
              :disabled="!!busy"
              @click="syncMarket(true)"
            >
              安装
            </button>
            <button
              v-else-if="!p.builtin"
              class="ghost small"
              :disabled="pluginsBusy === p.id || !!busy"
              @click="togglePluginEntry(p)"
            >
              {{ pluginsBusy === p.id ? "处理中…" : p.disabled ? "启用" : "禁用" }}
            </button>
            <span v-else class="hint">鲸仔入口，不提供禁用</span>
          </div>
        </div>
      </section>

      <div v-if="showSettings && settings" class="modal-backdrop" @click.self="showSettings = false">
        <div class="modal">
          <div class="modal-head">
            <h2>设置</h2>
            <button class="ghost small" @click="showSettings = false">✕</button>
          </div>
          <div class="form">
            <label>
              <span>端口</span>
              <input v-model.number="settings.port" type="number" min="0" max="65535" />
            </label>
            <label>
              <span>npm 镜像源</span>
              <input v-model="settings.registry" type="text" placeholder="https://registry.npmjs.org" />
            </label>
            <label>
              <span>工作目录（可选）</span>
              <div class="row">
                <input v-model="settings.workspaceDir" type="text" placeholder="留空使用默认目录" />
                <button class="ghost" @click="pickWorkspaceDir">选择…</button>
              </div>
              <p class="hint">DSH 服务器的工作目录，会话里终端等相对路径以此为基准；留空使用默认目录。</p>
            </label>
            <label>
              <span>Node 安装目录</span>
              <p class="hint">{{ settings.nodeDir || "自动检测" }}（鲸仔自动检测或安装时写入，无需手动填写）</p>
            </label>
            <label>
              <span>下载目录（可选）</span>
              <div class="row">
                <input v-model="settings.downloadDir" type="text" placeholder="留空使用系统下载目录" />
                <button class="ghost" @click="pickDownloadDir">选择…</button>
              </div>
              <p class="hint">会话日志等下载的保存位置；留空使用系统下载目录。</p>
            </label>
            <label class="check">
              <input v-model="settings.autostart" type="checkbox" @change="toggleAutostart" />
              <span>开机自启本程序</span>
            </label>
            <label class="check">
              <input v-model="settings.autoRestart" type="checkbox" />
              <span>服务器异常退出后自动重启</span>
            </label>
            <label class="check">
              <input v-model="settings.dshStopWithWhalito" type="checkbox" />
              <span>服务跟随鲸仔程序停止</span>
            </label>
            <label class="check">
              <input v-model="settings.dshAutoCheckUpdate" type="checkbox" />
              <span>开启自动检查更新（DeepSeek Harness）</span>
            </label>
            <label class="check">
              <input v-model="settings.whalitoAutoCheckUpdate" type="checkbox" />
              <span>开启自动检查更新（鲸仔）</span>
            </label>
            <label class="check">
              <input v-model="settings.petEnabled" type="checkbox" @change="togglePet" />
              <span>显示桌宠</span>
            </label>
          </div>
          <div class="row">
            <button class="primary" @click="saveSettings">保存设置</button>
            <button class="ghost" @click="showSettings = false">取消</button>
          </div>
          <p class="hint">npm 镜像源在安装/更新 Harness 时生效；国内网络慢可改为 https://registry.npmmirror.com</p>
        </div>
      </div>

      <section class="card logs panel-area-logs">
        <div class="logs-head">
          <h2>
            运行日志
            <span v-if="logs.length" class="logs-count">
              （共 {{ logs.length }} 条<span v-if="logHidden > 0">，显示后 {{ logs.length - logHidden }} 条</span>）
            </span>
          </h2>
          <div class="row logs-tools">
            <label class="check small-check">
              <input v-model="logAutoScroll" type="checkbox" />
              <span>自动滚动</span>
            </label>
            <button
              class="ghost small"
              :disabled="!haveErrors"
              @click="copyErrors"
              title="把全部错误日志复制到剪贴板"
            >
              复制错误
            </button>
            <button
              class="ghost small"
              :disabled="!haveErrors"
              @click="copyErrorsToAi"
              title="带版本信息的排障提示词，可直接粘贴给 AI"
            >
              复制到 AI
            </button>
            <button class="ghost small" @click="refreshLogs">刷新</button>
            <button class="ghost small" @click="clearLogs">清空</button>
          </div>
        </div>
        <div ref="logBox" class="logbox">
          <div v-if="logs.length === 0" class="empty">暂无日志</div>
          <template v-else>
            <button
              v-if="logHidden > 0"
              class="ghost small logs-earlier"
              @click="loadEarlierLogs"
            >
              加载更早（还有 {{ logHidden }} 条）
            </button>
            <div
              v-for="l in logVisible()"
              :key="l.id"
              class="log-row"
              :class="[`kind-${l.kind}`, { expanded: expandedLogIds.has(l.id) }]"
              @click="toggleLogExpanded(l.id)"
            >
              <span class="log-badge">{{ l.kind === "system" ? "系统" : l.kind === "error" ? "错误" : "信息" }}</span>
              <span class="log-time">{{ fmtTime(l.ts) }}</span>
              <span class="log-text">{{ l.text }}</span>
              <span class="log-actions" @click.stop>
                <button
                  v-if="l.kind === 'error'"
                  type="button"
                  class="ghost small"
                  @click="copyLogRow(l, false)"
                  title="复制该行内容"
                >
                  复制
                </button>
                <button
                  v-if="l.kind === 'error'"
                  type="button"
                  class="ghost small"
                  @click="copyLogRow(l, true)"
                  title="该行 + 版本信息，生成排障提示词"
                >
                  复制到 AI
                </button>
                <span class="log-expand-hint">{{ expandedLogIds.has(l.id) ? "收起" : "展开" }}</span>
              </span>
            </div>
          </template>
        </div>
      </section>

      <section class="card about panel-area-about">
        <div class="about-head">
          <h2>关于</h2>
          <span class="about-version">
            鲸仔 Whalito v{{ whalitoVer?.current ?? "—" }}{{ whalitoVer?.testBuild ? "（测试版）" : "" }}
            <span v-if="whalitoUpdateAvailable" class="badge-soft">
              可更新至 v{{ whalitoVer?.latest }}
            </span>
          </span>
        </div>
        <div class="about-versions">
          <span class="hint">DeepSeek Harness：v{{ env?.dshVersion ?? "未安装" }}</span>
          <span class="hint">通道：{{ settings?.dshChannel === "next" ? "预发布（next）" : "稳定（latest）" }}</span>
        </div>
        <div class="row">
          <button class="primary" @click="runWizard">重新运行安装引导</button>
          <button class="ghost" :disabled="whalitoUpdating" @click="checkWhalitoUpdateUi()">
            检查鲸仔更新
          </button>
          <button
            v-if="whalitoUpdateAvailable"
            class="primary"
            :disabled="whalitoUpdating"
            @click="updateWhalitoNow"
          >
            更新鲸仔到 v{{ whalitoVer?.latest }}
          </button>
          <button
            class="ghost"
            @click="invoke('open_url', { url: 'https://github.com/entireyu/dsh-whalito-desk' }).catch(() => {})"
          >
            GitHub 项目主页
          </button>
          <button class="ghost" @click="hideToTray">隐藏到托盘</button>
        </div>
        <p class="hint">Node / Harness 安装、升级、切换与校验统一由「安装引导」自动检测处理；关闭窗口即隐藏到托盘。</p>
      </section>
      </div>
      <!-- /双栏 -->
    </template>
  </div>

  <!-- 内嵌 DSH 页面右键自定义菜单（复制/粘贴 ─ 刷新页面/重启服务器/显示隐藏桌宠） -->
  <div
    v-if="ctxMenu"
    class="ctx-menu"
    :style="{ left: ctxMenu.x + 'px', top: ctxMenu.y + 'px' }"
    @contextmenu.prevent="ctxMenu = null"
  >
    <button type="button" @click="ctxCopy">复制</button>
    <button type="button" @click="ctxPaste">粘贴</button>
    <div class="ctx-sep" />
    <button type="button" @click="ctxReload">刷新页面</button>
    <button type="button" @click="ctxRestart">重启服务器</button>
    <button type="button" @click="ctxTogglePet">显示 / 隐藏桌宠</button>
  </div>

  <!-- 下载完成/复制成功提示（无路径时只显示纯文本提示，不出现文件夹按钮） -->
  <div v-if="toast" class="toast">
    <div class="toast-body">
      <span class="toast-text">{{ toast.text }}</span>
      <span v-if="toast.path" class="toast-path" :title="toast.path">{{ toast.path }}</span>
    </div>
    <div class="toast-actions">
      <button v-if="toast.path" type="button" class="toast-btn" @click="revealDownload(toast.path)">打开所在文件夹</button>
      <button type="button" class="toast-btn" @click="toast = null">✕</button>
    </div>
  </div>

  <!-- 后台发现的更新提示弹窗（每小时检查；含当前/最新版本与更新日志） -->
  <div v-if="currentUpdateNotice" class="modal-backdrop">
    <div class="modal notice-modal">
      <div class="modal-head">
        <h2>发现新版本</h2>
        <span class="notice-badge">{{ currentUpdateNotice.target === "whalito" ? "鲸仔" : "Harness" }}</span>
      </div>
      <p class="notice-target">
        {{ currentUpdateNotice.target === "whalito" ? "鲸仔 Whalito" : "DeepSeek Harness" }}
        有新版本可用
      </p>
      <ul class="notice-versions">
        <li><span>当前版本</span><code>{{ currentUpdateNotice.current }}</code></li>
        <li><span>最新版本</span><code>{{ currentUpdateNotice.latest }}</code></li>
      </ul>
      <div class="notice-changelog">
        <p class="notice-changelog-title">更新日志</p>
        <pre>{{
          currentUpdateNotice.changelog ||
          `新版本 ${currentUpdateNotice.latest} 已发布，点击「立即更新」即可升级。`
        }}</pre>
      </div>
      <a
        v-if="currentUpdateNotice.url"
        class="notice-link"
        href="#"
        @click.prevent="openNoticeUrl"
      >查看详情 ↗</a>
      <div class="row notice-actions">
        <button class="ghost" @click="snoozeUpdate">暂不更新（24 小时内不再提醒）</button>
        <button class="primary" @click="updateFromPopup">立即更新</button>
      </div>
    </div>
  </div>

  <!-- 鲸仔自更新：全屏统一 loading，直到应用退出并由安装链重启 -->
  <LoadingScreen
    v-if="whalitoUpdating"
    status="鲸仔更新中"
    :detail="whalitoUpdateMessage"
  />
</template>
