# Changelog

本文件记录项目所有显著变更。

格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

## [0.4.7] - 2026-08-25

### 新增
- 预置 WebUI+ 增强插件（`@entireyu/dsh-webui-plus`）：随鲸仔启动同步到 web profile 的 `node_modules` 与 `cordis.patch.yml`（与既有鲸仔设置分区同一托管标记块，双插件共存）；WebUI+ 设置页新增插件版本徽标、GitHub 链接与鲸仔推荐卡片（`whalito.jniantic.cn`）。

### 修复
- 修复内嵌 DSH 页面里链接点击无反应：DSH 原生把对话/搜索结果中的 http(s) 链接渲染为 `<a target="_blank">`，在 Tauri WebView 的跨源 iframe 中点击会被拦截。改由鲸仔专属插件（`whalito-dsh-settings`）在捕获阶段接管这类链接，经桥 `postMessage(open-url)` → 父窗口 `open_url` 命令 → 系统默认浏览器打开；WebUI+ 等通用插件保持原生 `window.open`，不感知宿主环境（普通浏览器行为不受影响）。
- 修复旧版测试构建把托管标记 `⟪` 写成 `隡` 的编码损坏：同步时识别旧标记并统一替换为标准标记，避免 cordis.patch.yml 出现两个托管块重复 insert。

## [0.4.6] - 2026-08-20

### 新增
- DSH 更新/安装的全屏 loading 增加「已用时」计时器：`installingDsh` 期间每秒刷新「已用时 mm:ss」，长时间更新也有明确时间反馈（弹窗 / 设置页 / 引导流程三个入口共用，覆盖「停止服务器 → 更新 → 启动」全程；引导流程「安装 DeepSeek Harness 中」同样显示）

### 变更
- 启动服务器不再自动打开系统浏览器：DSH rc8+ 默认在 Web 就绪后拉起默认浏览器，与鲸仔内嵌页面重复——现向 `dsh web` 追加 `--no-open` 抑制（rc8 源码确认 `--no-open` 为唯一权威开关，配置文件无法覆盖）。老版本 dsh 不认识该参数（commander 遇未知选项会报错退出），故首次启动用 `dsh web --help` 探测一次并缓存支持情况，不支持的版本自动不加参数；`install_dsh` / `update_dsh` 成功后失效缓存重新探测（覆盖会话内 DSH 升级）。`--no-open` 不影响 `printUrl`，日志仍打印 `dsh web: http://…` 供地址抽取；需要外部浏览器时可用管理后台「在浏览器打开」按钮

## [0.4.5] - 2026-08-19

### 新增
- 后台定时（每小时，首次延迟 60 秒）检查 DSH 与鲸仔是否有新版本：发现新版且未在静默期时自动唤起主窗口并弹「更新提示」，展示目标、当前版本、最新版本与更新日志；「暂不更新」对该目标静默 24 小时（持久化，重启仍生效），「立即更新」直接走对应更新流程（鲸仔弹窗视为确认、不再弹原生二次确认）。检查放在 Rust 后台线程，窗口隐藏/后台节流时依然可靠
- nvm 切换检测：当前 Node 缺失 / 版本过低时，若 nvm 中已安装满足要求（≥ 22.19.0）的更高版本，检测结果新增 `nvmSwitchVersion`，引导流程与鲸仔管理后台优先提示「切换到 Node X（nvm 已安装）」而非重新下载安装；切换需两步确认（防误触），确认后执行 `nvm use`（Windows）/ `nvm alias default + nvm use`（macOS）并重新检测。此前 Windows 完全不扫描 nvm-windows 已装版本，只能让用户重装
- 全屏 loading 统一：新增 `LoadingScreen` 组件（居中 logo → 鲸仔 Whalito → DeepSeek Harness 桌面版 → 进度条 → 状态文字），引导流程（检测环境 / 安装 DSH / 启动服务器）、内嵌页服务器启停重启与 DSH 更新、鲸仔自更新全部改用统一布局与状态文案（检测环境中 / 启动服务器中 / 停止服务器中 / 重启服务器中 / 安装 DeepSeek Harness 中 / 更新 DeepSeek Harness 中 / 鲸仔更新中）；引导流程 loading 阶段出错时自动切回错误卡片（不再无限转圈），右下角保留「进入鲸仔管理后台」入口
- 鲸仔自更新成功后校验：更新链启动前写入目标版本标记（`update_marker.json`），应用重启后对比当前运行版本——一致则 toast「已成功更新到 vX」，不一致则红色横幅提示「更新未完成（当前 vX / 目标 vY）请重试或检查网络」；标记读后即删，只提示一次

### 变更
- 管理后台重构：移除「环境检测 / DeepSeek Harness / 服务器」三张卡片，页面精简为「logo+标题 → 状态总览条（Node / Harness / 服务器，颜色区分状态）→ 服务器操作工具栏（启动/停止/重启/应用内打开/在浏览器打开/复制地址）→ 运行日志（支持手动刷新并显示条数）→ 关于区（鲸仔版本 / GitHub 链接 / 隐藏到托盘 / 重新运行安装引导）」；Node/Harness 安装、升级、切换、校验统一由「重新运行安装引导」入口承接（引导流程自动检测补齐）
- 入口文案统一：「返回鲸仔助手」「进入高级面板」等全部改为「进入鲸仔管理后台」；管理后台与引导流程顶部加入鲸仔 logo 与「鲸仔管理后台」标题

### 修复
- 修复 DSH 对话中模型接口重试耗尽（失败中止）后桌宠误报「任务已完成」或无提示：mux 流捕获 `turn/end` 的 `reason=error`，立即向桌宠发「任务失败」通知（即使主窗口聚焦也提醒），并记录失败会话防止轮询覆盖为 completed；桌宠气泡显示「任务失败 + 失败内容」，打开主界面后清除
- 修复服务器自动重启耗尽（连续 3 次失败）后桌宠无提示：新增桌宠「系统提醒」通道（pet-alert kind=system），主窗口在重试停止时同步触发，桌宠气泡显示「服务器异常」+ 失败详情，可点 ✕ 关闭；主窗口隐藏到托盘/后台时提示依然可见
- 修复 DSH 内所有「复制」按钮（消息 / 代码块 / 搜索结果 / 表格 / JSON）点击无效：内嵌 iframe 里 `navigator.clipboard.writeText` 常因焦点/权限策略失败，插件现将其包装为「原生优先、失败经 clipboard-set 桥由鲸仔主窗口（Rust clipboard_write）写入系统剪贴板」，DSH 侧始终显示「复制成功」
- 修复内嵌页右键「重启服务器」无 loading 反馈、直接跳到「服务器未运行」：服务器启停/重启期间（url 会因 3 秒轮询读到中间态而短暂为空）内嵌页显示「正在重启服务器… / 正在启动服务器… / 正在停止服务器…」loading，不再误显示停服页；操作失败时按真实状态同步
- 修复鲸仔自更新下载失败后「鲸仔设置」分区仍显示「正在下载更新…」等更新中提示：插件错误处理只复位了 `checking`，漏了 `updating` / `updateStage`，而父窗口失败路径不会补发 hello 快照来复位——失败时统一复位所有进行中状态（检查按钮 / 更新按钮 / 进度文案）
- 修复鲸仔自更新在原生确认框点「取消」后主窗口全屏遮罩一直停在「鲸仔正在更新…」：命令正常返回（取消）时同样收起遮罩
- 修复 macOS 鲸仔自更新「只重启、版本不变」：更新链安装目录误用 `current_exe().parent()`（`Contents/MacOS`），导致新包被拷进旧包内部（`Contents/MacOS/Whalito.app` 嵌套垃圾）、重启的仍是旧二进制——改为向上定位 `.app` 包根目录；更新脚本同步加固：按制表符解析 dmg 卷挂载点（兼容卷名含空格）、新包先拷到同卷暂存再原子替换（失败保留旧包并重启旧版本，不丢应用）、执行日志写 `/tmp/whalito-update.log` 便于排查
- 引导流程 Node 阶段的操作失败（安装 / 升级 / nvm 切换）现在会显示具体错误信息，不再静默无反馈

## [0.4.4] - 2026-08-18

### 新增
- DSH 更新期间内嵌页显示「正在更新 DeepSeek Harness…」loading（含阶段文案），不再在服务器重启时误显示「服务器未运行」；鲸仔自更新前主窗口显示「鲸仔正在更新…」全屏遮罩

### 变更
- DSH 更新编排改为「停止服务器 → 更新安装 → 重新启动」，避免 Windows 下运行中的服务器锁定原生模块导致 `remove_dir_all` 半装 / 长耗时；更新结束统一刷新环境 / 服务器状态 / 版本快照并清除更新中状态

### 修复
- 修复 DSH 更新完成后鲸仔面板仍显示 loading / 旧版本 / 更新中：更新入口统一走 `performDshUpdate`，`finally` 中 `refreshEnv + refreshStatus + pushSnapshot` 并复位 `installingDsh`
- 修复鲸仔自更新弹出 `ping 127.0.0.1` 控制台窗口：更新链由 `cmd /C "ping ... & start ..."` 改为临时 WScript 脚本（`wscript.exe`，GUI 子系统无控制台），等待/静默安装/重启全程无窗口

## [0.4.3] - 2026-08-17

### 新增
- DSH 版本偏好（稳定版 latest / 预发布版 next）：放在「版本信息」区顶部，点击即保存并自动按新偏好重查；检查结果显示所属通道（如「发现新版本 0.1.0-rc.7（预发布版）」）；「更新到新版本」按所选通道安装（首次安装仍固定稳定版）
- 内嵌 DSH 页面右键菜单增加「复制 / 粘贴」（与原菜单项用分割线分隔）：选区/光标在 iframe 内，剪贴板由主窗口经系统剪贴板（arboard）读写；支持聊天输入框（受控组件 + 透明字形 backdrop）、普通输入框、contenteditable 与文档选区

### 变更
- 内嵌页消息改串行队列处理：保存设置 / 检查更新 / 更新安装不再并发交错
- 保存设置后无条件重查版本（含切换镜像源），检查结果始终与当前设置一致
- 桌宠：主窗口聚焦时抑制「任务完成」通知（被阻塞 / 被中断仍提醒）；聚焦回主窗口时立即清除挂着的通知
- 复制成功提示 2 秒自动消失（下载提示仍 8 秒）
- 右键菜单移除「剪切」（保留复制 / 粘贴）

### 修复
- 修复切换版本偏好后检查结果残留旧通道（并发消息读到陈旧缓存判定"未变更"不重查）
- 修复复制 / 粘贴失效：右键后 iframe 失焦导致选区不可读——右键瞬间快照选区/光标；剪贴板指令监听从设置分区组件移到插件工厂级（聊天页等任意位置均可使用）
- 修复粘贴内容不可见（DSH 输入框为受控组件，直接改 DOM 值不同步状态）——粘贴改走组件自己的合成 paste 事件管线
- 修复菜单复制把 U+FFFC 隐形占位符（claim 令牌）带进剪贴板——复制复用组件自己的复制管线，与 Ctrl+C 输出一致

## [0.4.2] - 2026-08-17

### 变更
- Release workflow 仅由 `v*` tag 自动触发；分支推送不再触发构建（需要产物验证时手动触发，仅构建上传、不创建 Release）

### 修复
- 修复 Release workflow 发布说明提取失败导致不发布：`release-notes.js` 在 `"type": "module"` 包下按 ESM 运行、`require` 不可用——改名为 `.cjs` 强制 CommonJS
- 修复 macOS 安装 Harness 报「未找到 npm」：`npm_cli` 在符号链接解析后只探测 node 真实路径的同级 `node_modules/npm` 与上一级 `lib/node_modules/npm`，而 Homebrew（Apple Silicon）的 node 真实路径在 `Cellar/node/<ver>/bin`，npm 挂在 prefix 层 `/opt/homebrew/lib/node_modules/npm`（向上 4 级，旧实现永远找不到；官方 pkg 布局同样漏掉，需向上 2 级）——改为沿真实路径祖先目录逐级向上（6 级）探测两种挂载点，天然兼容 Homebrew / 官方 pkg / nvm / fnm / volta / 便携 tar 包；新增 Homebrew 与官方 pkg 布局单测；「未找到 npm」报错信息附带 node 路径与版本，便于区分 Node 未装完整的情况

## [0.4.1] - 2026-08-16

### 优化
- 预留：启动 DSH 时注入 `DSH_DIALOG_OWNER_HWND` 环境变量（鲸仔主窗口句柄），供未来支持 owner 窗口的 DSH 原生目录选择器使用——不改动 DSH 源码，当前 DSH 版本忽略该变量
- 内嵌 DSH 页面右键自定义菜单：屏蔽 WebView2 默认菜单，改为鲸仔风格的「刷新页面 / 重启服务器 / 显示或隐藏桌宠」（经鲸仔设置分区 postMessage 桥上报位置，面板视图右键不弹菜单）
- DSH 会话日志导出下载接管：导出不再静默落到系统下载目录——鲸仔设置新增「下载目录」（留空回退系统下载目录），下载完成后右下角弹提示（含保存路径与「打开所在文件夹」）；仅拦截本机 DSH 服务器的 `/api/session.export`，文件名冲突自动去重
- 设置目录字段优化：工作目录 / 下载目录补充说明文案并支持「选择…」按钮调原生目录选择器；Node 安装目录改为只读展示（由鲸仔自动检测或安装时写入，不再允许手动输入覆盖）

### 修复
- 修复「鲸仔设置」里检查更新发现新版本后「立即更新」按钮点击无反应：`window.confirm` 在 WebView2 中不可用（默认脚本对话框只支持 alert，confirm 静默返回 false），确认改为 Rust 侧 tauri-plugin-dialog 原生对话框（取消则静默退出）
- 修复桌宠「显示 / 隐藏」首次点击无反应的假象：pet 窗口创建时默认可见、未按 pet_enabled 隐藏（配置为隐藏时桌宠仍显示，首次切换翻转到 true 后无变化、第二次才隐藏）——改为显式 `visible(false)`，仅启用时显示

## [0.4.0] - 2026-08-16

### 新增
- macOS 版本（universal，Apple Silicon / Intel 通用）：
  - 环境检测适配 macOS：按优先级探测 自定义/便携目录 → nvm（`~/.nvm`，含 current 软链与版本目录）→ fnm/volta → Homebrew（`/opt/homebrew`、`/usr/local`）→ 系统 node → PATH；npm-cli.js 兼容 Homebrew / 官方 pkg / nvm 布局
  - Node 一键安装：下载官方 `node-v22.x-darwin-{arm64,x64}.tar.gz` 解压到 `~/Library/Application Support/com.deepseek.dsh-launcher/node/`（免管理员）；支持 nvm 安装（`source nvm.sh`）与自定义目录便携安装；安装后回写 node_dir，GUI 应用无 shell PATH 也能定位
  - macOS 启动 dsh 时注入用户 shell PATH；开机自启为用户级 LaunchAgent；停止服务器用 `kill`；按端口定位进程用 `lsof`；打开浏览器用 `open`
  - 鲸仔自动更新支持 macOS：匹配 Release 中的 `.dmg` 资产，挂载 dmg 覆盖安装并自动重启（hdiutil / ditto / xattr 去隔离）
  - 前端按平台切换安装 UI（winget 按钮仅 Windows 显示，macOS 显示 tar 包安装说明）
- GitHub Actions Release 工作流（`.github/workflows/release.yml`）：推送 `v*` tag 自动构建 Windows NSIS 与 macOS universal dmg 并上传 GitHub Release；分支推送仅构建并上传 Artifacts
- 修复 `whalito_apply_update` 命令未注册进 handler 的问题（此前鲸仔「立即更新」实际调用会失败）
- 桌宠任务完成 / 被阻塞 / 被中断通知：打开主界面后清除、回到空闲态
- 桌宠气泡贴紧鲸仔头顶，空闲时改为定时轻冒泡（不常驻对话框）

### 变更
- 环境检测重构为「平台候选链 + 逐个版本探测择优」（Windows 候选与优先级不变，新增版本择优：同序候选中优先选用版本达标的）；`pick_asset` 参数化为 `pick_asset_for`（按平台选资产）

### 修复
- 修复 macOS 安装 Harness 后仍提示「安装未完成」：npm 全局安装在 POSIX 的布局是 `<prefix>/lib/node_modules`（Windows 为 `<prefix>/node_modules`），`dsh_bin` 只探测了 Windows 布局导致 macOS 永远找不到入口——改为双布局探测并新增单测；安装完成后的校验改为显式执行 `dsh --version`，失败时把真实报错返回给用户
- 修复 macOS 安装 Harness 失败（koffi 等原生依赖的安装脚本报 `node: command not found`，退出码 127）：`run_output` / `run_streaming` 统一注入「GUI PATH + 用户 shell PATH」；`detect_env` 预热 shell PATH 捕获；npm 安装命令追加 `--scripts-prepend-node-path=true`，并将 node 所在目录置顶注入 npm 子进程 PATH（绝对兜底，不依赖 shell PATH 捕获与 npm flag）
- 修复 macOS 环境检测挂起（Node 检测不到）：shell PATH 捕获期间持有互斥锁与 `child_path` 的读锁同线程重入死锁——捕获改为无锁回填，`child_path` 改用 `try_lock` 退回当前 PATH
- 修复 macOS 编译失败：窗口透明（`transparent`）需启用 tauri `macos-private-api` feature

## [0.3.0] - 2026-08-16

### 新增
- DSH 设置面板内新增「鲸仔」分区：端口 / npm 镜像 / 开机自启 / 自动重启 / 工作目录 / Node 目录 / 桌宠开关，以及服务器启停 / 重启 / 返回鲸仔助手；与主窗口通过 postMessage 双向同步
- 鲸仔设置分区插件（@entireyu/whalito-dsh-settings，内嵌于应用）在启动服务器前幂等同步到 DSH web profile（node_modules + cordis.patch.yml 标记块），不依赖 pnpm，不动 DSH 源码

### 变更
- 移除内嵌页右下角悬浮按钮（fab）及独立 Webview 遗留代码（embed.rs / inject.js）；服务器未运行时内嵌页提供「启动服务器 / 返回鲸仔助手 / 打开设置」
- 在「鲸仔」分区保存端口变更后自动重启服务器生效
- 安装包名改为英文 Whalito（productName 变更，安装目录随之变为 %LOCALAPPDATA%\Whalito）
- 新增测试构建（pnpm tauri:build:test）：包名 Whalito-Test、标识符 com.deepseek.dsh-launcher.test、默认端口 30080、独立 DSH 数据目录 ~/.dsh-test，与生产包可共存
- 「鲸仔」设置分区握手加固：未连接时每 2 秒重试 ping、放宽 WebView2 的 event.source 校验、父窗口在未加载设置时也回握手
- 修复握手永远停留在「正在连接鲸仔…」的根因：postMessage 的 structured clone 不接受 Vue reactive Proxy，父窗口发送快照前改为 JSON 深拷贝（toPlain），并新增 %TEMP%\whalito-bridge.log 诊断通道
- 托盘图标悬浮提示显示应用名称「鲸仔 Whalito」，测试版末尾追加「（测试版）」
- 「鲸仔」设置分区更名为「鲸仔设置」，分区内容头部展示鲸仔应用 logo（64x64 PNG 以 data URI 内嵌，展示 32px/2x 密度），副标题「鲸仔（Whalito）桌面端设置」
- 服务器运行中不再显示「启动服务器」按钮（停止/重启仅运行时显示）
- npm 镜像源支持快速切换：一键切换 npmmirror（国内加速）/ npm 官方源并立即保存
- 「鲸仔设置」分区新增版本信息区块：分别展示 DSH 当前版本与鲸仔当前版本（测试版带标记），各自提供检查更新按钮——DSH 走 npm 镜像源检查，鲸仔走 GitHub releases（404 回退 tags）；鲸仔发现新版本附「打开下载页」；鲸仔行常驻「GitHub」按钮直达项目主页
- 鲸仔自动更新：分区内「立即更新」一键完成 下载 → 静默安装到当前目录 → 自动重启（安装包按变体自动选择 Whalito_/Whalito-Test_ 资产；下载经 Rust 直连，无浏览器 MOTW 标记，不会触发 SmartScreen 拦截）
- 桌宠右键菜单升级为与托盘同款：打开面板 / 启动服务器 / 停止服务器 / 在浏览器打开 / 隐藏桌宠 / 退出（菜单跟随鼠标位置并限制在桌宠窗口内）
- 桌宠修复与架构：服务器探测改为健康优先兜底链（记录地址 → 配置端口），API 失败显示具体原因并写 %TEMP%\whalito-pet.log 诊断日志；支持按住鲸仔拖拽窗口（4px 阈值区分点击）且位置持久化；点击桌宠不再重载内嵌页
- 桌宠样式 API：新增 ~/.dsh/pet-style.json 契约（尺寸/位置/头像/强调色/气泡配色/动画开关），变更 2 秒内热更新，Pet.vue 退化为默认渲染器；用户可经 DSH 编辑该文件调整外观（详见 README）
- 修复桌宠自上线以来一直显示「服务器未运行/正在连接」的根因：pet 窗口不在 Tauri capabilities 白名单，`plugin:event|listen` 被 ACL 拦截——已把 pet 加入白名单并授予 core:event:default 与窗口位置权限；拖拽改为手动移动窗口（缩放系数感知，4px 阈值区分点击），不再依赖透明窗口下不可靠的 startDragging
- 测试版安装包不上传 GitHub：版本检查新增 autoUpdate 标记（release 有当前变体匹配资产才为 true），测试版发现新版本时隐藏「立即更新」并提示「测试版不提供自动更新」

## [0.2.0] - 2026-08-14

### 新增
- 引导式主流程：启动即进入 loading，自动检测 Node/dsh/服务器状态，按状态进入「装 Node / 装 dsh / 启动服务器」并最终在应用内嵌打开 Harness 页面
- 内嵌浏览器打开：新增独立 Webview 窗口加载 dsh 页面，注入悬浮按钮（返回助手 / 启动 / 停止 / 重启 / 设置），动作通过虚拟主机名导航 + `on_navigation` 拦截实现，不开放远程 IPC
- Node 版本门槛：要求 Node ≥ 22.19.0，缺失或过低统一进入安装引导
- 安装 Node 三种方式：nvm 安装切换、winget 一键（安装/升级）、自定义目录便携版（下载 zip 解压）
- 服务器启动改为「等待就绪」（轮询健康检查，超时报错），不再依赖 stdout URL 抽取
- 服务器一键重启
- 单实例运行：二次启动时唤起已有窗口
- Harness 版本更新检查：自动比对最新版本并提示可更新 / 已最新
- 托盘状态同步、设置面板改为弹窗、安装 / 更新 / 校验进度提示

### 变更
- 品牌重构：应用更名「鲸仔（Whalito）」，更新 logo 与全套图标，二进制名改为 Whalito

### 修复
- 校验安装改用 `dsh web --dump-default-config`，修复 `--profile` 报错

## [0.1.1] - 2026-08-14

### 修复
- 修复残缺安装误判与启动失败自动重试死循环

## [0.1.0] - 2026-08-14

### 新增
- 环境检测：自动检测 Node.js / npm / `@deepseek-ai/dsh` 是否就绪（含 nvm、winget 等常见位置与 `Program Files` 兜底）
- 一键安装 Node.js（`winget install OpenJS.NodeJS.LTS`）
- 一键安装 / 更新 Harness（`npm install -g @deepseek-ai/dsh`，可切换 npm 镜像源）
- 安装校验（`dsh --version` + `--dump-default-config`）
- 服务器管理：启动 / 停止 `dsh web --port <port>`，实时状态与 HTTP 健康检查
- 托盘常驻、开机自启、异常自动重启
- 实时日志回显
- npm 前缀兜底、托盘菜单状态联动、自定义图标
- 支持停止外部启动的 DSH 服务器（按端口定位进程 + 二次确认）

### 修复
- 安装 / 启动等阻塞命令改为异步，修复点击安装后界面卡死
- 探测端口上外部已运行的 DSH 服务器，修复误报已停止

[0.4.4]: https://github.com/entireyu/dsh-whalito-desk/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/entireyu/dsh-whalito-desk/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/entireyu/dsh-whalito-desk/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/entireyu/dsh-whalito-desk/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/entireyu/dsh-whalito-desk/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/entireyu/dsh-whalito-desk/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/entireyu/dsh-whalito-desk/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/entireyu/dsh-whalito-desk/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/entireyu/dsh-whalito-desk/releases/tag/v0.1.0
