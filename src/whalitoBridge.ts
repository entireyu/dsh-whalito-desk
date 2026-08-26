// 与内嵌 DSH 页面"鲸仔"设置分区之间的 postMessage 桥。
// 下行（本窗口 → iframe）：hello / settings / status / error 快照、剪贴板指令；
// 上行（iframe → 本窗口）：ping / action（含剪贴板文本）。双方各自校验消息来源。

export interface WhalitoSettings {
  port: number;
  registry: string;
  /** DSH 版本偏好（npm 发布标签）："latest"（稳定版，默认）/ "next"（预发布版）。 */
  dshChannel: string;
  autostart: boolean;
  autoRestart: boolean;
  workspaceDir: string | null;
  nodeDir: string | null;
  /** DSH 会话日志导出等下载的保存目录；空 = 系统下载目录。 */
  downloadDir: string | null;
  petEnabled: boolean;
}

export interface WhalitoStatus {
  phase: string;
  url: string | null;
  pid: number | null;
}

/** DSH 版本行（当前/最新/是否有更新）。 */
export interface DshVersionInfo {
  current: string | null;
  latest: string | null;
  updateAvailable: boolean;
}

/** 鲸仔版本行（附测试标记、下载页地址与自动更新可用性）。 */
export interface WhalitoVersionInfo {
  current: string | null;
  testBuild: boolean;
  latest: string | null;
  updateAvailable: boolean;
  autoUpdate: boolean;
  url: string | null;
}

export interface VersionsSnapshot {
  dsh: DshVersionInfo;
  whalito: WhalitoVersionInfo;
}

/** 鲸仔内置插件状态行（hello 快照 / plugins 下行消息）。 */
export interface WhalitoPluginEntry {
  id: string;
  name: string;
  description: string;
  builtin: boolean;
  installable: boolean;
  installed: boolean;
  disabled: boolean;
}

/** dsh 命令注册到系统 PATH 的状态（hello 快照；level: user | system）。 */
export interface DshPathView {
  registered: boolean;
  prefix: string;
  platform: string;
  level: string;
}

/** 两级 PATH 状态（hello 快照 / dsh-path 下行消息携带单级别）。 */
export interface DshPathsSnapshot {
  user: DshPathView | null;
  system: DshPathView | null;
}

export interface WhalitoMessage {
  channel: "whalito";
  type: string;
  action?: string;
  value?: unknown;
  settings?: WhalitoSettings | null;
  status?: WhalitoStatus;
  message?: string;
  versions?: VersionsSnapshot;
  /** 内置插件列表（hello 快照 / plugins 下行消息）。 */
  plugins?: WhalitoPluginEntry[] | null;
  /** dsh 命令 PATH 注册状态（hello 快照）。 */
  dshPaths?: DshPathsSnapshot | null;
  /** dsh-path 下行消息的单级别视图。 */
  dshPath?: DshPathView | null;
  target?: string;
  url?: unknown;
  /** 右键菜单复制上行的选区文本、粘贴下行的待插入文本。 */
  text?: string;
  /** 剪贴板链路诊断信息（右键快照类型/长度等）。 */
  dbg?: unknown;
  /** 剪贴板空操作原因（无选中内容等）。 */
  why?: string;
  /** 右键菜单请求的点击位置（相对内嵌页面视口）。 */
  x?: number;
  y?: number;
  /** 会话日志导出下载的建议文件名。 */
  filename?: string;
  /** 目录选择请求的目标字段（workspaceDir / downloadDir）。 */
  field?: string;
  /** 目录选择结果（picked-dir 下行消息）。 */
  path?: unknown;
}

/** 当前 DSH 服务源（用于校验 iframe 消息的 event.origin）。 */
export function dshOrigin(url: string | null | undefined): string | null {
  if (!url) return null;
  try {
    return new URL(url).origin;
  } catch {
    return null;
  }
}

/**
 * 深度去代理/去响应式：Vue 的 reactive Proxy 无法通过 postMessage 的
 * structured clone（实测抛 DataCloneError），发送前必须 JSON 深拷贝成普通对象。
 */
export function toPlain<T>(value: T): T {
  if (value === null || value === undefined) return value;
  return JSON.parse(JSON.stringify(value)) as T;
}

/** 向 iframe 内的"鲸仔"设置分区发消息；返回 null 表示发送成功，否则返回错误信息。 */
export function postToDsh(
  frame: HTMLIFrameElement | null | undefined,
  msg: WhalitoMessage,
): string | null {
  if (!frame || !frame.contentWindow) {
    return "iframe 引用为空";
  }
  try {
    frame.contentWindow.postMessage(msg, "*");
    return null;
  } catch (e) {
    return typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
  }
}

/** 类型收窄：是否为鲸仔桥消息。 */
export function isWhalitoMessage(data: unknown): data is WhalitoMessage {
  if (typeof data !== "object" || data === null) return false;
  const d = data as Record<string, unknown>;
  return d.channel === "whalito" && typeof d.type === "string";
}
