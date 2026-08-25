<div align="center">
  <img src="public/logo.png" alt="鲸仔 Whalito" width="128" />
  <h1>鲸仔 · Whalito</h1>
  <p><strong>你的 DeepSeek Harness 桌面助手</strong></p>
  <p>一键安装 · 智能引导 · 常驻托盘 —— 让 AI 助手开箱即用</p>
</div>

---

## 鲸仔是什么

鲸仔（Whalito）是一款为普通用户打造的 DeepSeek Harness 桌面助手。它把原本需要在命令行里才能完成的「安装 Node.js → 安装 Harness → 启动服务」整套流程，简化成几个按钮：打开应用，它会自动检测环境、补齐缺失依赖、启动服务，并在内置窗口中直接打开 Harness。

不需要懂 Node，不需要记命令，开箱即用。

> 💡 **欢迎反馈**：使用中遇到问题或有功能建议，欢迎到 [GitHub Issues](https://github.com/entireyu/dsh-whalito-desk/issues) 提 issue，我们会认真对待每一条反馈。

## 📸 界面预览

![鲸仔界面](snap-layout.png)

## ✨ 核心亮点

- **全程引导，告别命令行** —— 打开即自动检测，缺什么装什么，一路点到能用。
- **一键补齐环境** —— Node.js、DeepSeek Harness 缺失或版本过低，自动安装 / 升级。
- **常驻托盘，随开随用** —— 关闭窗口不退出，最小化到托盘；支持开机自启。
- **应用内直达** —— 内置浏览器直接打开 Harness，无需记地址、另开浏览器。
- **状态一目了然** —— 已停止 / 启动中 / 运行中 / 异常实时反馈，日志随时可查。

## 💻 运行环境

鲸仔本身开箱即用，无需手动配置。运行前只需满足以下系统要求：

### 系统要求

| 项目 | 要求 |
| --- | --- |
| 操作系统 | Windows 10（版本 1809 及以上）/ Windows 11 · macOS 12+（Apple Silicon / Intel） |
| 架构 | Windows：x64（64 位）；macOS：universal（Apple Silicon 与 Intel 通用） |
| WebView2 Runtime | Windows：首次安装时由安装器自动安装（Windows 11 已内置）；macOS 无需 |

### DeepSeek Harness 依赖（鲸仔自动处理）

以下依赖由鲸仔自动检测并引导安装，你无需手动配置：

| 依赖 | 要求 | 说明 |
| --- | --- | --- |
| Node.js | ≥ 22.19.0 | 缺失或版本过低时，鲸仔会引导一键安装 / 升级 |
| npm | 随 Node.js 附带 | 用于安装 Harness |
| 网络连接 | 可访问 npm 源 | 首次安装需联网下载依赖 |

## 🚀 快速开始

### Windows

1. 在 [Releases](https://github.com/entireyu/dsh-whalito-desk/releases) 下载最新安装包 `Whalito_0.4.8_x64-setup.exe`
2. 双击安装（首次运行 Windows SmartScreen 会提示「未知发布者」，点击「仍要运行」即可）
3. 打开鲸仔，其余交给它 —— 检测、补齐、启动全程自动
4. 关闭窗口即最小化到托盘，需要时点击托盘图标唤回

### macOS（内部测试版）

1. 在 [Releases](https://github.com/entireyu/dsh-whalito-desk/releases) 下载最新 `Whalito_0.4.8_universal.dmg`
2. 双击挂载，把「Whalito」拖入「应用程序」（Applications）
3. **首次打开**：安装包为内部测试版（未签名公证），请勿直接双击图标——在 Finder 中**右键 App → 打开 → 再点「打开」**确认一次，之后即可正常双击启动
4. 打开鲸仔，其余交给它 —— 检测、补齐、启动全程自动；关闭窗口即隐藏到托盘（菜单栏图标）

macOS 环境检测与安装说明：

- 鲸仔按优先级检测 Node：自定义/便携目录 → nvm（`~/.nvm`）→ fnm/volta → Homebrew（`/opt/homebrew`、`/usr/local`）→ 系统自带（通常过旧，会提示升级）→ PATH
- 未检测到 Node 时，一键安装会下载 Node.js 22 官方 tar 包到 `~/Library/Application Support/com.deepseek.dsh-launcher/node/`，**全程无需管理员权限**；检测到 nvm 时也可用 nvm 安装
- 开机自启通过用户级 LaunchAgent（`~/Library/LaunchAgents/com.deepseek.dsh-launcher.plist`）实现，同样无需管理员权限
- 卸载：删除「应用程序/Whalito.app」即可；如需彻底清理，另删 `~/Library/Application Support/com.deepseek.dsh-launcher`、上述 plist 与 `~/.dsh`（注意后者包含你的 Harness 数据）

#### macOS 冒烟测试清单（内部测试者随包自检）

- [ ] dmg 挂载 → 拖入应用程序 → 首次右键「打开」绕过 Gatekeeper，之后双击可正常启动
- [ ] 全新机器：一键安装 Node 22（tar 包）→ 自动安装 Harness → 服务器启动 → 应用内打开 Harness
- [ ] 已装 Homebrew / nvm 的机器：检测即用，不重复安装
- [ ] 系统自带 `/usr/bin/node` 过旧时正确提示版本要求并引导安装
- [ ] Harness 原生插件（`node-addon-landlock-run-darwin-*`、`koffi-darwin-*` 等 optionalDependencies）可随 npm 正常安装（首验项）
- [ ] 托盘菜单：打开面板 / 启动 / 停止 / 在浏览器打开 / 桌宠 / 退出
- [ ] 桌宠透明、置顶、可拖拽，位置持久化；设置里开关即时生效
- [ ] 开机自启开关 → 注销重登后自动启动（LaunchAgent）
- [ ] 服务器运行中，DSH 内使用 git 等 shell 工具的技能正常（PATH 注入生效）
- [ ] 检查更新 → 立即更新：下载 dmg → 覆盖安装 → 自动重启为新版本

## 功能一览

| 能力 | 说明 |
| --- | --- |
| 环境检测 | 自动识别 Node.js / npm / Harness 是否就绪 |
| 一键安装 Node.js | Windows：winget / nvm / 便携 zip；macOS：官方 tar 包（免管理员）/ nvm / 自定义目录 |
| 安装 / 更新 Harness | 支持切换 npm 镜像源，国内更快 |
| 服务器管理 | 一键启动 / 停止 / 重启，异常自动拉起 |
| 鲸仔设置分区 | DSH 设置面板内直接管理鲸仔设置（端口 / 镜像 / 版本偏好 / 自启 / 桌宠 / 版本检查与一键更新等），取代悬浮按钮 |
| 托盘常驻 | 最小化到托盘，支持开机自启 |
| 实时日志 | 运行状态与输出实时可见 |

## 🛠 开发者

鲸仔基于 **Tauri 2 + Vue 3 + TypeScript** 构建，支持 Windows 与 macOS。

### 目录结构

```
src/                    # Vue 3 + TS 前端控制面板
  whalitoBridge.ts      # 与内嵌 DSH 页面"鲸仔"设置分区的 postMessage 桥
src-tauri/
  src/
    lib.rs              # 入口：托盘、窗口、命令注册
    state.rs            # 共享状态、配置、进程 / 日志 / 健康检查
    commands.rs         # 全部 Tauri 命令（检测 / 安装 / 启停 / 设置）
    settings_plugin.rs  # 鲸仔设置分区插件同步（幂等写入 web profile）
  whalito-dsh-settings/ # 内嵌的 DSH 客户端插件包（package.json / index.js / client.js）
  Cargo.toml
  tauri.conf.json
```

### 本地开发

```bash
pnpm install
pnpm tauri dev        # 前端 dev server + Rust debug
```

### 打包

```bash
pnpm tauri:build        # 生产包：Whalito_0.4.8_x64-setup.exe（DSH 端口 3080，数据目录 ~/.dsh）
pnpm tauri:build:test   # 测试包：Whalito-Test_0.4.8_x64-setup.exe（DSH 端口 30080，数据目录 ~/.dsh-test）
```

测试包与生产包三隔离（包名/标识符、默认端口、DSH 数据目录均不同），可同时安装、同时运行、互不干扰。测试开关是编译期的 `WHALITO_TEST_BUILD=1`（见 `src-tauri/src/state.rs` 的 `TEST_BUILD`），生产构建不设置该变量，测试代码被编译器折叠、零残留。

```
安装器输出目录：src-tauri/target/release/bundle/nsis/
```

> 安装包当前未签名，Windows SmartScreen 会提示「未知发布者」，属预期；正式分发建议配置代码签名证书。

### macOS 构建（GitHub Actions）

macOS 产物（`.app` / `.dmg`）无法在 Windows/Linux 上交叉编译，由 GitHub Actions 的 macOS runner 构建（见 `.github/workflows/release.yml`）：

```bash
# 本地 macOS 机器上等价命令：
rustup target add x86_64-apple-darwin
pnpm tauri build --target universal-apple-darwin --config src-tauri/tauri.macos.conf.json
```

- 打 `v*` tag 或手动触发 Release workflow → CI 同时构建 Windows NSIS 与 macOS universal dmg，并上传到 GitHub Release（资产名如 `Whalito_0.4.4_universal.dmg`）
- 产物为 ad-hoc 签名（内部测试分发；未公证，用户右键 → 打开）。正式对外分发需配置 Apple Developer ID 证书并启用 notarytool 公证（工作流内已预留注释位）
- macOS 版本更新器匹配 Release 中的 `.dmg` 资产（Windows 匹配 `_x64-setup.exe`），一键更新会自动挂载 dmg 覆盖安装并重启

### 桌宠样式 API（pet-style.json）

桌宠外观由样式契约文件驱动，文件变更 2 秒内热更新（无需重启）：

| 版本 | 契约文件路径 |
| --- | --- |
| 生产 | `~/.dsh/pet-style.json` |
| 测试 | `~/.dsh-test/pet-style.json` |

```json
{
  "schemaVersion": 1,
  "size": 96,
  "position": { "x": 1280, "y": 720 },
  "avatar": null,
  "accent": "#f87171",
  "bubble": { "bg": "#171a21", "fg": "#e8eaf0", "sub": "#9aa3b2", "fontSize": 12 },
  "animations": { "bob": true, "float": true, "attention": true }
}
```

- `size`：鲸仔直径（px，48–160）；`position`：窗口物理坐标（null = 默认右下角，拖拽后自动写回）；
- `avatar`：头像图片路径或 data URI（null = 内置 logo，超 256KB 忽略）；
- `accent`：徽标强调色；`bubble`：气泡背景/前景/副文本色与字号（10–18）；`animations`：三项动画开关；
- 所有字段均可省略（缺省走内置默认值），非法值自动回退默认；
- **通过 DSH 调整**：直接对 DSH 说"把桌宠改小一点/换成红色气泡/换个头像"，让它编辑该 JSON 即可；也可经「鲸仔设置」未来的样式界面调整。

桌宠窗口可按住鲸仔拖拽（轻点打开主界面），位置自动持久化；桌宠状态异常时可查看 `%TEMP%\whalito-pet.log` 诊断日志。

## 🔍 原理：背后的真实命令

鲸仔对用户透明 —— 每个按钮背后执行的命令都清晰可查：

| 动作 | 实际执行 |
| --- | --- |
| 检测 Node | Windows：`where.exe node` / `node --version` / 兜底 `C:\Program Files\nodejs\node.exe`；macOS：按优先级探测 自定义目录 → nvm（`~/.nvm`）→ fnm/volta → Homebrew → `/usr/bin/node` → PATH，并逐个 `node --version` 验证 |
| 装 Node | Windows：`winget install/upgrade OpenJS.NodeJS.LTS`；nvm：`nvm install 22.x` + `nvm use`；便携：下载 node zip 解压到用户目录。macOS：下载 `node-v22.x-darwin-{arm64,x64}.tar.gz` → `/usr/bin/tar` 解压到 `~/Library/Application Support/com.deepseek.dsh-launcher/node/`（免管理员）；nvm：`bash/zsh -lc 'source nvm.sh && nvm install 22 && nvm alias default 22'` 并回写解析出的 node 绝对路径 |
| 装 / 更新 Harness | 首次安装 `node <npm-cli.js> install -g @deepseek-ai/dsh`；更新按「DSH 版本偏好」安装 `@deepseek-ai/dsh@latest`（稳定版）或 `@deepseek-ai/dsh@next`（预发布版）（应用专用前缀，隔离免管理员） |
| 检查 Harness 更新 | `node <npm-cli.js> view @deepseek-ai/dsh@<latest|next> version`（标签按「DSH 版本偏好」，走所选 npm 镜像源） |
| 校验 | `node <dsh/bin.js> --version` + `--dump-default-config` |
| 启动 | `node <dsh/bin.js> web --port <port>`（macOS 额外注入用户 shell PATH） |
| 停止 | Windows：`taskkill /PID <pid> /T /F`；macOS/Linux：`kill <pid>` |
| 开机自启 | Windows：注册表 Run 键；macOS：`~/Library/LaunchAgents/com.deepseek.dsh-launcher.plist` |
| 健康检查 | `GET <解析出的 URL>`（800ms 超时） |

## 💬 反馈与贡献

欢迎提 issue 和参与改进：

- **遇到问题 / 功能建议**：到 [GitHub Issues](https://github.com/entireyu/dsh-whalito-desk/issues) 提交，附上系统版本、鲸仔版本、日志或截图会更快定位问题。
- **参与开发**：欢迎提交 Pull Request（见上方「开发者」一节的本地开发与打包说明）。

## 📄 许可证

本项目基于 [MIT License](LICENSE) 开源。Copyright (c) 2026 Evan Hang。详见 [LICENSE](LICENSE)。
