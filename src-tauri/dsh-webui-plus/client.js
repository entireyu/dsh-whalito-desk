// DSH WebUI+ 增强 —— DSH Web 客户端插件（浏览器半体，手写 bundle，无构建步骤）。
// 装载契约：window.__ModuleLoader__.load({ id, factory }) 的 CJS 工厂格式；
// factory 的 require 仅解析平台模块表成员（react / react/jsx-runtime，共享 React 实例）。
//
// 功能（全部走加性座位，不替换任何 shipped UI）：
//   1. 输入工具行显示当前模型供应商 chip（可设置开关，默认开）
//   2. 每轮完成后对话尾部显示耗时 + 输入/输出 token（可设置开关，默认关）
//   3. 模型请求最终失败后「重试」按钮 + 可折叠详细错误（可设置开关，默认关）
//   4. 设置页（WebUI+ 增强）内含鲸仔（Whalito）推荐位；设置项分组
//      「对话区域 / 侧边栏」，配置行为自绘 Switch 开关（DSH 无原生 Switch），
//      「显示归档数量角标」依赖「显示归档任务入口」开启方可配置。
//   5. 归档任务查看面板：工作区栏头部「搜索」左侧新增归档图标按钮（与下拉菜单
//      「归档会话」同款 IconArchiveOutline 图标；数量角标可在设置中开关，默认关，
//      中性样式），点击弹出浮层面板：支持按标题/工作区/会话 id 搜索，列出全部
//      归档会话（标题/工作区/时间/状态），每行右侧「在新会话中分支」与
//      「展开/收起预览」两个图标按钮。DSH 无任何层级的取消归档/删除会话 API，
//      恢复路径 = 在新会话中分支。
//   6. 对话页左侧用户消息锚点条：在对话滚动容器左缘固定一根细竖条，一条用户
//      消息（user/steering）= 一根等距横条；视口顶部那条用户消息的横条最长，
//      点击横条平滑滚动到对应消息（可在设置中开关，默认开）。
// （编辑重发不实现：DSH 原生 fork 已覆盖该需求，保留默认行为。）
//
// 数据来源（与 shipped 插件同款，均为客户端已有接口）：
//   - 模型目录：ctx.get('connection').api.sessions.models({ sessionId })
//   - 会话快照：标准 props useSession → snapshot.chat.nodes（Map，entry.data 为视图节点）
//   - 每轮统计：owner props turn.steps[].data.get('assistant-step').finalNode（usage/timing）
//   - 发送/草稿：标准 props inputActions.setDraft / submit
//   - fork：ctx.get('sessions').fork({ sessionId, atSeq, increaseTitle }) + open(childId)
// 设置持久化：localStorage（按浏览器保存，跨设备不同步——v1 假设）。
//
// 注意：jsx(type, config, maybeKey) 的第三个位置参数是 key 而不是 children，
// children 必须放进 config.children（否则 dev/prod 运行时都会丢弃子节点）。
(function () {
  'use strict';

  if (window.__ModuleLoader__ === undefined) {
    throw new Error('dsh-webui-plus: __ModuleLoader__ is missing (not a dsh web boot)');
  }

  window.__ModuleLoader__.load({
    id: '@entireyu/dsh-webui-plus',
    factory: function (require) {
      var module = { exports: {} };
      var exports = module.exports;

      var React = require('react');
      var jsx = require('react/jsx-runtime').jsx;

      // jsx 包装：children 放进 config.children，key 走第三个位置参数。
      function h(type, props, kids) {
        var p = Object.assign({}, props || {});
        var key = p.key;
        delete p.key;
        if (kids !== undefined) p.children = kids;
        return key === undefined ? jsx(type, p) : jsx(type, p, key);
      }

      // ── 设置（localStorage + 页内事件）──────────────────────────────────────
      var LS_KEY = 'whalito.dsh-webui-plus.settings';
      var SETTINGS_EVENT = 'dsh-webui-plus:settings';
      var DEFAULT_SETTINGS = { showProvider: true, showDetailedError: false, showTurnStats: false, showArchives: true, showArchiveCount: false, showChatMap: true };

      function loadSettings() {
        try {
          var raw = window.localStorage.getItem(LS_KEY);
          if (!raw) return Object.assign({}, DEFAULT_SETTINGS);
          var parsed = JSON.parse(raw);
          var out = Object.assign({}, DEFAULT_SETTINGS);
          if (parsed && typeof parsed === 'object') {
            if (typeof parsed.showProvider === 'boolean') out.showProvider = parsed.showProvider;
            if (typeof parsed.showDetailedError === 'boolean') out.showDetailedError = parsed.showDetailedError;
            if (typeof parsed.showTurnStats === 'boolean') out.showTurnStats = parsed.showTurnStats;
            if (typeof parsed.showArchives === 'boolean') out.showArchives = parsed.showArchives;
            if (typeof parsed.showArchiveCount === 'boolean') out.showArchiveCount = parsed.showArchiveCount;
            if (typeof parsed.showChatMap === 'boolean') out.showChatMap = parsed.showChatMap;
          }
          return out;
        } catch (err) {
          return Object.assign({}, DEFAULT_SETTINGS);
        }
      }

      function saveSettings(next) {
        try {
          window.localStorage.setItem(LS_KEY, JSON.stringify(next));
        } catch (err) {
          /* 存储不可用时忽略 */
        }
        window.dispatchEvent(new window.CustomEvent(SETTINGS_EVENT));
      }

      /** 订阅设置变化的 hook；设置面板改动会让所有组件即时刷新。 */
      function useSettings() {
        var state = React.useState(loadSettings);
        var s = state[0];
        var setS = state[1];
        React.useEffect(function () {
          function onSettings() { setS(loadSettings()); }
          window.addEventListener(SETTINGS_EVENT, onSettings);
          return function () { window.removeEventListener(SETTINGS_EVENT, onSettings); };
        }, []);
        return s;
      }

      // ── 通用小工具 ──────────────────────────────────────────────────────────
      /** 从 ContentBlock[] 取纯文本。 */
      function textOfContent(blocks) {
        if (!Array.isArray(blocks)) return '';
        return blocks
          .filter(function (b) { return b && b.type === 'text' && typeof b.text === 'string'; })
          .map(function (b) { return b.text; })
          .join('\n');
      }

      /** 毫秒 → 人类可读耗时。 */
      function formatDuration(ms) {
        if (!Number.isFinite(ms) || ms < 0) return null;
        if (ms < 1000) return Math.round(ms) + 'ms';
        if (ms < 60000) return (ms / 1000).toFixed(1) + 's';
        var m = Math.floor(ms / 60000);
        var s = Math.round((ms % 60000) / 1000);
        return s === 0 ? m + 'm' : m + 'm ' + s + 's';
      }

      /** token 数 → 人类可读。 */
      function formatTokens(n) {
        if (!Number.isFinite(n) || n < 0) return null;
        if (n >= 10000) return (n / 1000).toFixed(1) + 'k';
        if (n >= 1000) return (n / 1000).toFixed(2) + 'k';
        return String(n);
      }

      function finiteNonNegative(n) {
        return typeof n === 'number' && Number.isFinite(n) && n >= 0;
      }

      /** 从 usage 对象取 token 数（字段缺失返回 null）。 */
      function usageField(usage, key) {
        return usage && finiteNonNegative(usage[key]) ? usage[key] : null;
      }

      /** 字符串截断（超长加省略号）。 */
      function truncate(s, n) {
        if (typeof s !== 'string') return '';
        if (s.length <= n) return s;
        return s.slice(0, n) + '…';
      }

      /** 毫秒时间戳 → 相对/绝对时间文案。 */
      function formatRelativeTime(ms) {
        if (!Number.isFinite(ms) || ms <= 0) return '';
        var diff = Date.now() - ms;
        if (diff < 0) diff = 0;
        if (diff < 60000) return '刚刚';
        if (diff < 3600000) return Math.floor(diff / 60000) + ' 分钟前';
        if (diff < 86400000) return Math.floor(diff / 3600000) + ' 小时前';
        if (diff < 86400000 * 30) return Math.floor(diff / 86400000) + ' 天前';
        var d = new Date(ms);
        var pad = function (n) { return n < 10 ? '0' + n : String(n); };
        return d.getFullYear() + '-' + pad(d.getMonth() + 1) + '-' + pad(d.getDate());
      }

      /** 工作区路径的末段（显示名），空路径返回空串。 */
      function workspaceBasename(cwd) {
        if (typeof cwd !== 'string' || cwd === '') return '';
        var trimmed = cwd.replace(/[/\\]+$/, '');
        var parts = trimmed.split(/[/\\]/);
        var last = parts[parts.length - 1];
        return typeof last === 'string' ? last : '';
      }

      /** 复制文本到剪贴板（clipboard API 优先，兜底 execCommand）。 */
      function copyText(text) {
        try {
          if (window.navigator && window.navigator.clipboard && window.navigator.clipboard.writeText) {
            window.navigator.clipboard.writeText(text).catch(function () {});
            return;
          }
        } catch (err) { /* 继续走兜底 */ }
        try {
          var ta = window.document.createElement('textarea');
          ta.value = text;
          ta.style.position = 'fixed';
          ta.style.opacity = '0';
          window.document.body.appendChild(ta);
          ta.select();
          window.document.execCommand('copy');
          window.document.body.removeChild(ta);
        } catch (err) { /* 复制失败忽略 */ }
      }

      /**
       * 用系统默认浏览器打开外部链接。
       *  - 鲸仔（Whalito）内嵌环境：DSH 页面被嵌入跨源 iframe，window.open 会被
       *    Tauri WebView 拦截；改走既有桥通道 postMessage → 父窗口 invoke(open_url)
       *    → 系统默认浏览器。
       *  - 普通浏览器（DSH 原生 web / 直接访问 30080）：回退 window.open。
       */
      function isEmbeddedCrossOrigin() {
        try {
          if (window.parent === window || window.parent === null) return false;
          if (window.location.origin === window.parent.location.origin) return false;
          return true;
        } catch (err) {
          // 跨源读取 parent.origin 会抛 SecurityError → 必然被嵌入跨源 iframe。
          return window.parent !== window && window.parent !== null;
        }
      }

      function openExternal(url) {
        if (typeof url !== 'string' || url === '') return;
        try {
          if (isEmbeddedCrossOrigin()) {
            window.parent.postMessage({
              channel: 'whalito',
              type: 'action',
              action: 'open-url',
              url: url,
            }, '*');
            return;
          }
        } catch (err) { /* 桥不可用则回退 */ }
        try {
          window.open(url, '_blank', 'noopener');
        } catch (err) { /* 弹窗被拦截时忽略 */ }
      }

      /**
       * 全局外部链接拦截（仅鲸仔内嵌环境安装）：
       *  DSH 原生把对话中的网址渲染为 <a target="_blank" rel="noopener noreferrer">，
       *  在跨源 iframe 里点击会被 Tauri WebView 拦截（无反应）。这里用捕获阶段
       *  监听文档级 click，拦截所有 target=_blank 的 <a>，改走 openExternal 桥通道。
       *  普通浏览器不安装，保持原生行为。返回卸载函数。
       */
      function installExternalLinkCapture() {
        if (typeof window === 'undefined' || typeof window.document === 'undefined') return function () {};
        if (!isEmbeddedCrossOrigin()) return function () {};
        function onClick(e) {
          var target = e && e.target;
          var el = target instanceof window.HTMLElement ? target : null;
          while (el !== null && el.tagName !== 'A') el = el.parentElement;
          if (el === null || el.tagName !== 'A') return;
          var href = el.getAttribute && el.getAttribute('href');
          if (typeof href !== 'string' || href === '') return;
          var isExternal = href.indexOf('://') !== -1 || href.indexOf('//') === 0;
          if (!isExternal) return;
          var wantsBlank = el.target === '_blank' || el.getAttribute('target') === '_blank';
          if (!wantsBlank) return;
          e.preventDefault();
          e.stopPropagation();
          openExternal(href);
        }
        window.document.addEventListener('click', onClick, true);
        return function () {
          window.document.removeEventListener('click', onClick, true);
        };
      }

      /** 一行 caption 的样式。 */
      var captionStyle = {
        color: 'var(--dsw-alias-label-tertiary, rgba(127,127,127,0.9))',
        fontSize: '12px',
        lineHeight: '18px',
        display: 'inline-flex',
        alignItems: 'center',
        gap: '6px',
      };
      var chipStyle = {
        display: 'inline-flex',
        alignItems: 'center',
        height: '18px',
        maxWidth: '140px',
        marginLeft: '2px',
        padding: '0 6px',
        borderRadius: '9px',
        fontSize: '11px',
        lineHeight: '18px',
        color: 'var(--dsw-alias-label-secondary, #555)',
        background: 'var(--dsw-alias-interactive-bg-hover, rgba(127,127,127,0.12))',
        whiteSpace: 'nowrap',
        overflow: 'hidden',
        textOverflow: 'ellipsis',
      };
      var ghostButtonStyle = {
        display: 'inline-flex',
        alignItems: 'center',
        height: '24px',
        padding: '0 10px',
        borderRadius: '12px',
        fontSize: '12px',
        lineHeight: '24px',
        cursor: 'pointer',
        color: 'var(--dsw-alias-label-secondary, #555)',
        background: 'var(--dsw-alias-interactive-bg-hover, rgba(127,127,127,0.12))',
        border: '1px solid var(--dsw-alias-border-inverted, rgba(127,127,127,0.25))',
      };
      var errorTextStyle = {
        color: 'var(--dsw-alias-state-error-primary, #d03050)',
        fontSize: '12px',
        lineHeight: '18px',
        wordBreak: 'break-all',
        whiteSpace: 'pre-wrap',
      };

      // ── 功能 5：归档任务面板（模块级状态，按钮席与浮层席共享）───────────────
      // 数据源（与 shipped workspace UI 同款，均为客户端已有服务）：
      //   - ctx.get('workspaces').list.getSnapshot().archivedSessionIds（实时，host 帧驱动）
      //   - ctx.get('sessions').list.getSnapshot().byId（含归档会话的摘要行）
      //   - ctx.get('connection').api.sessions.history（面板内文本预览）
      //   - ctx.get('sessions').fork + open（分支查看）
      var archiveState = { open: false, expandedId: null, busyId: null, previewByRow: {}, forkErrorByRow: {}, rev: 0 };
      var archiveListeners = [];

      function setArchiveState(patch) {
        archiveState = Object.assign({}, archiveState, patch, { rev: archiveState.rev + 1 });
        for (var i = 0; i < archiveListeners.length; i++) {
          try { archiveListeners[i](archiveState); } catch (err) { /* 监听器异常不阻断 */ }
        }
      }

      function subscribeArchiveState(fn) {
        archiveListeners.push(fn);
        return function () {
          var i = archiveListeners.indexOf(fn);
          if (i >= 0) archiveListeners.splice(i, 1);
        };
      }

      /** 订阅归档面板状态（按钮/浮层两个席位共享同一份模块级状态）。 */
      function useArchiveState() {
        var pair = React.useState(archiveState);
        var s = pair[0];
        var setS = pair[1];
        React.useEffect(function () {
          return subscribeArchiveState(function (next) { setS(next); });
        }, []);
        return s;
      }

      /** 订阅 workspaces.list 与 sessions.list 两个快照 store，归档集合/摘要变化即重渲染。 */
      function useArchiveData() {
        function read() {
          return {
            ws: workspacesFace && workspacesFace.list && typeof workspacesFace.list.getSnapshot === 'function'
              ? workspacesFace.list.getSnapshot() : null,
            ss: appSessions && appSessions.list && typeof appSessions.list.getSnapshot === 'function'
              ? appSessions.list.getSnapshot() : null,
          };
        }
        var pair = React.useState(read);
        var s = pair[0];
        var setS = pair[1];
        React.useEffect(function () {
          var stops = [];
          if (workspacesFace && workspacesFace.list && typeof workspacesFace.list.subscribe === 'function') {
            stops.push(workspacesFace.list.subscribe(function () { setS(read()); }));
          }
          if (appSessions && appSessions.list && typeof appSessions.list.subscribe === 'function') {
            stops.push(appSessions.list.subscribe(function () { setS(read()); }));
          }
          return function () {
            for (var i = 0; i < stops.length; i++) {
              var stop = stops[i];
              if (typeof stop === 'function') stop();
            }
          };
        }, []);
        return s;
      }

      /** 从 session.history 尾页提取「最近提问 + 最近回复」纯文本片段。 */
      function summarizeHistory(value) {
        var events = value && Array.isArray(value.events) ? value.events : [];
        var lastUser = '';
        var lastAssistant = '';
        for (var i = 0; i < events.length; i++) {
          var entry = events[i];
          var ev = entry && entry.event;
          if (!ev || !ev.data) continue;
          if (ev.type === 'user/message') {
            var ut = textOfContent(ev.data.content);
            if (ut) lastUser = ut;
          } else if (ev.type === 'assistant/message') {
            var msg = ev.data.message;
            var at = textOfContent(msg && msg.content);
            if (at) lastAssistant = at;
          }
        }
        var parts = [];
        if (lastUser) parts.push('【最近提问】' + truncate(lastUser, 120));
        if (lastAssistant) parts.push('【最近回复】' + truncate(lastAssistant, 240));
        return parts.join('\n\n');
      }

      /** 懒加载某归档会话的预览片段。 */
      function fetchPreview(sessionId) {
        var loading = Object.assign({}, archiveState.previewByRow, { [sessionId]: { state: 'loading' } });
        setArchiveState({ expandedId: sessionId, previewByRow: loading });
        if (!connectionFace || !connectionFace.api || !connectionFace.api.sessions) {
          var fail0 = Object.assign({}, archiveState.previewByRow, { [sessionId]: { state: 'error', error: '连接不可用' } });
          setArchiveState({ previewByRow: fail0 });
          return;
        }
        connectionFace.api.sessions.history({ sessionId: sessionId, maxMessages: 20 }).then(function (res) {
          var result = res && res.result ? res.result : res;
          if (result && result.ok && result.value) {
            var text = summarizeHistory(result.value);
            var ok = Object.assign({}, archiveState.previewByRow, { [sessionId]: { state: 'ok', text: text } });
            setArchiveState({ previewByRow: ok });
          } else {
            var msg = (result && result.error && (result.error.message || result.error.code)) || '加载失败';
            var fail = Object.assign({}, archiveState.previewByRow, { [sessionId]: { state: 'error', error: msg } });
            setArchiveState({ previewByRow: fail });
          }
        }).catch(function () {
          var fail2 = Object.assign({}, archiveState.previewByRow, { [sessionId]: { state: 'error', error: '加载失败' } });
          setArchiveState({ previewByRow: fail2 });
        });
      }

      /** 分支查看：原生 fork 生成新会话副本并打开；归档原会话不受影响。 */
      function forkArchivedSession(sessionId) {
        if (!appSessions) return;
        setArchiveState({ busyId: sessionId });
        appSessions.fork({ sessionId: sessionId, increaseTitle: true }).then(function (childId) {
          appSessions.open(childId);
          setArchiveState({ open: false, expandedId: null, busyId: null });
        }).catch(function (err) {
          var msg = err && err.message ? err.message : String(err);
          var errors = Object.assign({}, archiveState.forkErrorByRow, { [sessionId]: '分支失败：' + msg });
          setArchiveState({ busyId: null, forkErrorByRow: errors });
        });
      }

      // ── 功能 1：模型供应商 chip（conversation.input.right）──────────────────
      // owner props: InputZone { session, input }；标准 props: sessionId 等。
      // 防换行：徽标若把输入工具行挤到第二行则自动隐藏（隐藏后不再自行复现，
      // 仅在窗口 resize 且空间足够时才重新显示，避免显示/隐藏来回振荡）。
      function ProviderChip(props) {
        var enabled = useSettings().showProvider;
        var sessionId = props.sessionId;
        var catalog = React.useState(null);
        var state = catalog[0];
        var setState = catalog[1];
        var lastSessionRef = React.useRef(null);
        var elRef = React.useRef(null);
        var hiddenRef = React.useRef(false);
        var fitsState = React.useState(true);
        var fits = fitsState[0];
        var setFits = fitsState[1];

        // 首次挂载 / sessionId 变化：加载目录并订阅刷新事件。
        React.useEffect(function () {
          var cancelled = false;
          function load() {
            if (!connectionFace || !connectionFace.api || !connectionFace.api.sessions) return;
            connectionFace.api.sessions.models({ sessionId: sessionId }).then(function (res) {
              if (cancelled) return;
              var result = res && res.result ? res.result : res;
              if (result && result.ok && result.value) setState(result.value);
            }).catch(function () { /* 目录加载失败保持现状 */ });
          }
          load();
          var stops = [];
          if (remoteFace && typeof remoteFace.$on === 'function') {
            stops.push(remoteFace.$on('llm/adapters-updated', load));
            stops.push(remoteFace.$on('settings/document-updated', load));
          }
          if (appCtx && typeof appCtx.on === 'function') {
            stops.push(appCtx.on('connection/reset', load));
          }
          return function () {
            cancelled = true;
            for (var i = 0; i < stops.length; i++) {
              var stop = stops[i];
              if (typeof stop === 'function') stop();
            }
          };
        }, [sessionId]);

        // 快照变化（如切换模型）时重新拉目录，让 chip 保持最新。
        React.useEffect(function () {
          if (lastSessionRef.current !== props.session) {
            lastSessionRef.current = props.session;
            if (connectionFace && connectionFace.api && connectionFace.api.sessions) {
              connectionFace.api.sessions.models({ sessionId: sessionId }).then(function (res) {
                var result = res && res.result ? res.result : res;
                if (result && result.ok && result.value) setState(result.value);
              }).catch(function () {});
            }
          }
        });

        // 换行检测：找到输入工具行的 wrap 容器，徽标相对行顶的偏移超过一行高度即隐藏。
        React.useEffect(function () {
          function findWrapRow(node) {
            var n = node;
            while (n && n !== document.body) {
              var style = window.getComputedStyle(n);
              if (style.flexWrap === 'wrap' || style.flexWrap === 'wrap-reverse') return n;
              n = n.parentElement;
            }
            return null;
          }
          function check() {
            if (hiddenRef.current) return; // 隐藏期间不自行复现，避免振荡
            var el = elRef.current;
            if (!el) { setFits(true); return; }
            var row = findWrapRow(el);
            if (!row) { setFits(true); return; }
            var rowTop = row.getBoundingClientRect().top;
            var elTop = el.getBoundingClientRect().top;
            var wrapped = elTop - rowTop >= 24; // 落到第二行：偏移超过一行高度
            if (wrapped) hiddenRef.current = true;
            setFits(!wrapped);
          }
          function onResize() {
            hiddenRef.current = false; // 窗口变化是强信号：重新评估
            check();
          }
          check();
          window.addEventListener('resize', onResize);
          var ro = null;
          if (typeof window.ResizeObserver === 'function') {
            ro = new window.ResizeObserver(check);
            var row = findWrapRow(elRef.current);
            if (row) ro.observe(row);
          }
          return function () {
            window.removeEventListener('resize', onResize);
            if (ro) ro.disconnect();
          };
        }, []);

        // 徽标从无到有时补测一次（目录异步加载完成、或开关打开后）。
        var visible = enabled && !!state && !!state.current;
        React.useEffect(function () {
          if (visible) {
            var el = elRef.current;
            if (!el) return;
            var n = el;
            var row = null;
            while (n && n !== document.body) {
              var style = window.getComputedStyle(n);
              if (style.flexWrap === 'wrap' || style.flexWrap === 'wrap-reverse') { row = n; break; }
              n = n.parentElement;
            }
            if (!row) return;
            var rowTop = row.getBoundingClientRect().top;
            var elTop = el.getBoundingClientRect().top;
            var wrapped = elTop - rowTop >= 24;
            if (wrapped) hiddenRef.current = true;
            setFits(!wrapped);
          } else {
            setFits(true);
          }
        }, [visible]);

        if (!enabled || !state || !state.current || !fits) return null;
        var group = null;
        for (var i = 0; i < state.groups.length; i++) {
          if (state.groups[i].id === state.current.provider) { group = state.groups[i]; break; }
        }
        var label = group ? (group.name || group.id) : state.current.provider;
        return h('span', {
          key: 'provider',
          ref: elRef,
          style: chipStyle,
          title: state.current.provider + ' / ' + state.current.model,
        }, label);
      }

      // ── 功能 2/4：turnTail 链条目（conversation.chat.turnTail）────────────────
      // owner props: { turn: TurnLocation, seq, openFile }；标准 props: useSession/sessionId/inputActions。
      // 每轮完成后挂载：统计行（功能 2，设置开关默认关）、
      // 失败轮「重试」+ 可折叠详细错误（功能 4）。
      function TurnTailEnhance(props) {
        var settings = useSettings();
        var turn = props.turn;
        var nodesMap = typeof props.useSession === 'function'
          ? props.useSession(function (s) { return s && s.chat ? s.chat.nodes : undefined; })
          : undefined;

        // ── 每轮统计（功能 2）：从 turn.steps 的 assistant-step 数据聚合 ──
        var durationMs = null;
        var inTokens = null;
        var outTokens = null;
        var cacheReadTokens = null;
        var cacheWriteTokens = null;
        var hasTokens = false;
        if (turn && turn.start && turn.end) {
          durationMs = turn.end.time - turn.start.time;
        } else if (turn && turn.start) {
          // 兜底：最后一个有 completedTime 的 assistant 节点 − turn 开始时间
          var steps = turn.steps || [];
          var lastCompleted = null;
          for (var si = 0; si < steps.length; si++) {
            var stepData = steps[si].data ? steps[si].data.get('assistant-step') : undefined;
            if (stepData && stepData.finalNode && stepData.finalNode.timing && finiteNonNegative(stepData.finalNode.timing.completedTime)) {
              lastCompleted = stepData.finalNode.timing.completedTime;
            }
          }
          if (lastCompleted !== null) durationMs = lastCompleted - turn.start.time;
        }
        {
          var steps2 = turn ? (turn.steps || []) : [];
          for (var j = 0; j < steps2.length; j++) {
            var sd = steps2[j].data ? steps2[j].data.get('assistant-step') : undefined;
            if (!sd || !sd.finalNode) continue;
            var usage = sd.finalNode.usage;
            if (!usage) continue;
            var o = usageField(usage, 'outputTokens');
            var i = usageField(usage, 'inputTokens');
            var cr = usageField(usage, 'cacheReadTokens');
            var cw = usageField(usage, 'cacheWriteTokens');
            if (o === null && i === null && cr === null && cw === null) continue;
            if (o !== null) outTokens = (outTokens || 0) + o;
            if (i !== null) inTokens = (inTokens || 0) + i;
            if (cr !== null) cacheReadTokens = (cacheReadTokens || 0) + cr;
            if (cw !== null) cacheWriteTokens = (cacheWriteTokens || 0) + cw;
            hasTokens = true;
          }
        }

        // ── 从快照派生本轮信息 ──
        var entries = [];
        if (nodesMap && typeof nodesMap.values === 'function') {
          entries = Array.from(nodesMap.values());
        }
        var turnNumber = turn ? turn.turn : undefined;
        var userMsgForTurn = null;
        var failedNode = null;
        var retryAttempts = [];
        for (var k = 0; k < entries.length; k++) {
          var entry = entries[k];
          var data = entry && entry.data;
          if (!data) continue;
          if (data.kind === 'user' && entry.location && entry.location.turn && entry.location.turn.turn === turnNumber) {
            if (userMsgForTurn === null || data.seq > userMsgForTurn.seq) userMsgForTurn = data;
          }
          if (data.kind === 'turn-error' && data.turn === turnNumber && failedNode === null) failedNode = data;
          if (data.kind === 'model-retry' && data.turn === turnNumber) {
            if (Array.isArray(data.attempts)) {
              for (var a = 0; a < data.attempts.length; a++) retryAttempts.push(data.attempts[a]);
            } else {
              retryAttempts.push(data);
            }
          }
        }

        var userText = userMsgForTurn ? textOfContent(userMsgForTurn.content) : '';
        var settingsNow = settings;
        var canRetry = failedNode !== null && userText !== '';

        // ── 动作 ──
        function onRetry() {
          if (!props.inputActions || userText === '') return;
          props.inputActions.setDraft(userText);
          props.inputActions.submit();
        }

        var expanded = React.useState(false);
        var detailOpen = expanded[0];
        var setDetailOpen = expanded[1];

        // ── 渲染 ──
        var kids = [];

        // 统计行
        var statParts = [];
        var dur = formatDuration(durationMs);
        if (dur !== null) statParts.push('⏱ ' + dur);
        if (hasTokens) {
          var tokenParts = [];
          if (inTokens !== null || cacheReadTokens !== null || cacheWriteTokens !== null) {
            var inTotal = (inTokens || 0) + (cacheReadTokens || 0) + (cacheWriteTokens || 0);
            var inFmt = formatTokens(inTotal);
            if (inFmt !== null) tokenParts.push('↑' + inFmt);
          }
          var outFmt = formatTokens(outTokens);
          if (outFmt !== null) tokenParts.push('↓' + outFmt);
          if (tokenParts.length > 0) statParts.push(tokenParts.join(' · '));
        }
        if (settingsNow.showTurnStats && statParts.length > 0) {
          kids.push(h('span', { key: 'stats', style: captionStyle }, statParts.join('  ·  ')));
        }

        // 操作按钮行
        var actions = [];
        if (canRetry) {
          actions.push(h('button', {
            key: 'retry',
            style: ghostButtonStyle,
            onClick: onRetry,
            title: '以同一条用户消息重新发起一轮',
          }, '重试'));
        }
        if (settingsNow.showDetailedError && failedNode) {
          actions.push(h('button', {
            key: 'detail-toggle',
            style: ghostButtonStyle,
            onClick: function () { setDetailOpen(!detailOpen); },
          }, detailOpen ? '收起错误详情' : '展开错误详情'));
        }
        if (actions.length > 0) {
          kids.push(h('span', { key: 'actions', style: { display: 'inline-flex', alignItems: 'center', gap: '6px' } }, actions));
        }

        // 详细错误块（折叠态=关键错误；展开态=关键错误 + 逐次重试失败详情）
        if (settingsNow.showDetailedError && failedNode) {
          var detailLines = [];
          var keyError = failedNode.message || '';
          if (failedNode.code) keyError = '[' + failedNode.code + '] ' + keyError;
          if (!detailOpen) {
            detailLines.push(keyError);
          } else {
            detailLines.push(keyError);
            if (retryAttempts.length > 0) {
              detailLines.push('');
              detailLines.push('—— 自动重试记录 ——');
              for (var r = 0; r < retryAttempts.length; r++) {
                var att = retryAttempts[r];
                var line = '第 ' + (att.retry || (r + 1)) + ' 次重试';
                if (finiteNonNegative(att.delayMs)) line += '（延迟 ' + formatDuration(att.delayMs) + '）';
                line += '：' + (att.retryState || 'scheduled');
                if (att.failure) {
                  var fm = att.failure.message || '';
                  if (att.failure.code) fm = '[' + att.failure.code + '] ' + fm;
                  if (att.failure.status) fm += '（HTTP ' + att.failure.status + '）';
                  if (att.failure.requestId) fm += ' · ' + att.failure.requestId;
                  line += ' — ' + fm;
                } else if (att.retryState === 'cancelled') {
                  line += '（已取消）';
                }
                detailLines.push(line);
              }
            }
          }
          kids.push(h('div', {
            key: 'error-detail',
            style: Object.assign({}, errorTextStyle, {
              padding: '6px 10px',
              borderRadius: '8px',
              background: 'var(--dsw-alias-interactive-bg-hover-danger, rgba(208,48,80,0.08))',
              maxWidth: 'min(560px, 100%)',
            }),
          }, detailLines.join('\n')));
        }

        if (kids.length === 0) return null;
        return h('div', {
          key: 'webui-plus-tail',
          style: {
            display: 'flex',
            flexDirection: 'column',
            alignItems: 'flex-start',
            gap: '6px',
            padding: '2px 0 6px',
          },
        }, kids);
      }

      // ── 功能 5：设置页（settings.section）+ 鲸仔推荐位 ───────────────────────
      // ── 功能 4：设置页（settings.section）+ 鲸仔推荐位 ───────────────────────
      // 插件版本（与 package.json version 同步；发布新版本时两处需一致）。
      var PLUGIN_VERSION = '0.1.2';
      var PLUGIN_GITHUB = 'https://github.com/entireyu/dsh-webui-plus';
      // 自绘 Switch 开关（DSH 前端无原生 Switch 组件，需自绘）：
      //   - 语义：<button role="switch" aria-checked>，轨道 + 滑块，CSS transition
      //   - 开启色 var(--dsw-alias-brand-primary)，关闭轨道 var(--dsw-alias-border-l2)
      //   - disabled 态：降透明度 + 禁点击；带 disabledTip 时悬浮显示自绘提示
      //     （自绘浮层即时出现，规避原生 title 的系统级延迟）。
      function Switch(props) {
        var checked = !!props.checked;
        var disabled = !!props.disabled;
        var disabledTip = props.disabledTip || '';
        var onChange = props.onChange;
        var tipState = React.useState(false);
        var tipOpen = tipState[0];
        var setTipOpen = tipState[1];
        var btnRef = React.useRef(null);
        var tipRef = React.useRef(null);

        var trackStyle = {
          position: 'relative',
          boxSizing: 'border-box',
          display: 'inline-flex',
          alignItems: 'center',
          width: '36px',
          height: '20px',
          flex: 'none',
          padding: '0',
          borderRadius: '10px',
          border: 'none',
          cursor: disabled ? 'not-allowed' : 'pointer',
          opacity: disabled ? 0.45 : 1,
          background: checked
            ? 'var(--dsw-alias-brand-primary, #4c8dff)'
            : 'var(--dsw-alias-border-l2, rgba(127,127,127,0.35))',
          transition: 'background .2s ease',
          outline: 'none',
        };
        var knobStyle = {
          position: 'absolute',
          top: '2px',
          left: '2px',
          width: '16px',
          height: '16px',
          borderRadius: '8px',
          background: 'var(--dsw-alias-bg-base, #ffffff)',
          boxShadow: 'var(--dsw-shadow-lv1, 0 1px 3px rgba(0,0,0,0.25))',
          transform: checked ? 'translateX(16px)' : 'translateX(0)',
          transition: 'transform .2s ease',
        };

        function onClick() {
          if (disabled || typeof onChange !== 'function') return;
          onChange(!checked);
        }

        // 悬浮提示浮层（仅禁用且有提示文案时出现）。
        var tip = null;
        if (disabled && disabledTip !== '' && tipOpen && btnRef.current) {
          var r = btnRef.current.getBoundingClientRect();
          tip = h('div', {
            key: 'tip',
            ref: tipRef,
            style: {
              position: 'fixed',
              left: r.left,
              top: r.bottom + 6,
              zIndex: 60,
              pointerEvents: 'none',
              maxWidth: '280px',
              padding: '5px 10px',
              borderRadius: '8px',
              border: '1px solid var(--dsw-alias-border-inverted, rgba(127,127,127,0.3))',
              background: 'var(--dsw-specific-menu, #ffffff)',
              boxShadow: 'var(--dsw-shadow-lv2, 0 4px 16px rgba(0,0,0,0.14))',
              color: 'var(--dsw-alias-label-primary, #222)',
              fontSize: '12px',
              lineHeight: '18px',
            },
          }, disabledTip);
        }

        return h('span', {
          style: { display: 'inline-flex', alignItems: 'center', flex: 'none' },
          onMouseEnter: function () {
            if (disabled && disabledTip !== '') setTipOpen(true);
          },
          onMouseLeave: function () { setTipOpen(false); },
        }, [
          h('button', {
            key: 'track',
            ref: btnRef,
            type: 'button',
            role: 'switch',
            'aria-checked': checked,
            'aria-disabled': disabled || undefined,
            style: trackStyle,
            onClick: onClick,
          }, h('span', { key: 'knob', style: knobStyle })),
          tip,
        ]);
      }

      function SettingsSection(props) {
        var settings = useSettings();
        function update(key, value) {
          var next = Object.assign({}, settings);
          next[key] = value;
          saveSettings(next);
        }
        function openWhalito() {
          openExternal('https://whalito.jniantic.cn');
        }

        // 与原生设置页一致的排版 token。
        var sectionStyle = {
          maxWidth: '720px',
          color: 'var(--dsw-alias-label-primary, #222)',
          display: 'flex',
          flexDirection: 'column',
          gap: '4px',
          width: '100%',
        };
        var titleStyle = {
          color: 'var(--dsw-alias-label-primary, #222)',
          margin: '0 0 2px',
          fontSize: '16px',
          fontWeight: '500',
          lineHeight: '24px',
        };
        var titleRowStyle = {
          display: 'flex',
          alignItems: 'center',
          gap: '10px',
          flexWrap: 'wrap',
        };
        var versionBadgeStyle = {
          display: 'inline-flex',
          alignItems: 'center',
          height: '20px',
          padding: '0 8px',
          borderRadius: '10px',
          fontSize: '12px',
          lineHeight: '20px',
          color: 'var(--dsw-alias-label-secondary, #555)',
          background: 'var(--dsw-alias-interactive-bg-hover, rgba(127,127,127,0.12))',
          whiteSpace: 'nowrap',
        };
        var githubBtnStyle = {
          display: 'inline-flex',
          alignItems: 'center',
          height: '24px',
          padding: '0 10px',
          borderRadius: '12px',
          fontSize: '12px',
          lineHeight: '24px',
          cursor: 'pointer',
          color: 'var(--dsw-alias-label-secondary, #555)',
          background: 'var(--dsw-alias-interactive-bg-hover, rgba(127,127,127,0.12))',
          border: '1px solid var(--dsw-alias-border-inverted, rgba(127,127,127,0.25))',
        };
        var introStyle = {
          color: 'var(--dsw-alias-label-tertiary, #777)',
          margin: '0 0 8px',
          fontSize: '14px',
          lineHeight: '22px',
        };
        var groupTitleStyle = {
          color: 'var(--dsw-alias-label-secondary, #555)',
          margin: '20px 0 4px',
          fontSize: '13px',
          fontWeight: '500',
          lineHeight: '20px',
        };
        var rowStyle = {
          display: 'flex',
          alignItems: 'center',
          gap: '8px',
          minWidth: '0',
          padding: '14px 0',
          borderBottom: '1px solid var(--dsw-alias-border-l2, rgba(127,127,127,0.15))',
        };
        var rowLabelStyle = {
          flex: '1',
          minWidth: '0',
          color: 'var(--dsw-alias-label-primary, #222)',
          fontSize: '14px',
          lineHeight: '22px',
        };
        var recommendCardStyle = {
          display: 'flex',
          alignItems: 'center',
          gap: '10px',
          marginTop: '20px',
          padding: '12px 14px',
          border: '1px solid var(--dsw-alias-border-l2, rgba(127,127,127,0.2))',
          borderRadius: '12px',
          background: 'var(--dsw-alias-bg-module-platform, transparent)',
        };
        var recommendTextStyle = {
          flex: '1',
          minWidth: '0',
          color: 'var(--dsw-alias-label-secondary, #555)',
          fontSize: '14px',
          lineHeight: '22px',
        };

        function row(key, label, checked, onChange, disabled, disabledTip) {
          return h('div', { key: key, style: rowStyle }, [
            h('span', { key: 'label', style: rowLabelStyle }, label),
            h(Switch, {
              key: 'switch',
              checked: checked,
              disabled: disabled,
              disabledTip: disabledTip,
              onChange: onChange,
            }),
          ]);
        }

        return h('div', { key: 'webui-plus-settings', style: sectionStyle }, [
          h('div', { key: 'titleRow', style: titleRowStyle }, [
            h('h2', { key: 'title', style: titleStyle }, 'WebUI Plus 设置'),
            h('span', { key: 'version', style: versionBadgeStyle }, 'v' + PLUGIN_VERSION),
            h('button', {
              key: 'github',
              type: 'button',
              style: githubBtnStyle,
              title: PLUGIN_GITHUB,
              onClick: function () { openExternal(PLUGIN_GITHUB); },
            }, 'GitHub'),
          ]),
          h('p', { key: 'intro', style: introStyle },
            'DeepSeek Harness WebUI+ 增强插件（@entireyu/dsh-webui-plus）的设置项。'),
          h('div', { key: 'group-conversation', style: groupTitleStyle }, '对话区域'),
          row('showProvider', '输入框显示当前选中模型的供应商', !!settings.showProvider, function (v) { update('showProvider', v); }),
          row('showDetailedError', '模型请求失败时展示详细错误', !!settings.showDetailedError, function (v) { update('showDetailedError', v); }),
          row('showChatMap', '对话锚点', !!settings.showChatMap, function (v) { update('showChatMap', v); }),
          row('showTurnStats', '每轮对话尾部显示耗时与 token 消耗', !!settings.showTurnStats, function (v) { update('showTurnStats', v); }),
          h('div', { key: 'group-sidebar', style: groupTitleStyle }, '侧边栏'),
          row('showArchives', '显示归档任务入口', !!settings.showArchives, function (v) { update('showArchives', v); }),
          row('showArchiveCount', '显示归档数量角标', !!settings.showArchiveCount, function (v) { update('showArchiveCount', v); },
            !settings.showArchives, '请先开启「显示归档任务入口」，方可配置此项。'),
          h('div', { key: 'recommend', style: recommendCardStyle }, [
            h('span', { key: 'text', style: recommendTextStyle },
              '通过桌面端（Windows / MacOS）一键安装、访问、升级 DeepSeek Harness，推荐使用「鲸仔Whalito」'),
            h('button', { key: 'link', style: ghostButtonStyle, onClick: openWhalito },
              '了解详情'),
          ]),
        ]);
      }

      // ── 功能 5：归档入口图标 + 锚点追踪（shell.overlay 席位，加性）───────────
      // 入口按钮锚定在工作区栏头部「搜索」按钮左侧（标题与搜索之间）：
      //   以搜索按钮的实时位置为锚（position: fixed 覆盖层），设置按钮可见时
      //   才显示；搜索展开（设置按钮被隐藏）或整栏不可见时自动隐藏。
      // 图标与行内下拉菜单「归档会话」同款（IconArchiveOutline20 的 SVG 路径）。
      function ArchiveIcon(props) {
        var size = props.size || 16;
        return h('svg', {
          key: 'archive-icon',
          width: size,
          height: size,
          viewBox: '0 0 20 20',
          fill: 'none',
          xmlns: 'http://www.w3.org/2000/svg',
          'aria-hidden': 'true',
          style: { display: 'block' },
        }, [
          h('path', {
            key: 'body',
            fillRule: 'evenodd',
            clipRule: 'evenodd',
            d: 'M15.8659 2.05975C17.2603 2.05995 18.3913 3.19096 18.3914 4.58527V5.4874C18.3914 6.02747 18.2192 6.52672 17.9303 6.93735C17.9336 6.96524 17.9388 6.99318 17.9388 7.02195V12.8884C17.9388 13.6345 17.9395 14.2379 17.8996 14.7254C17.8642 15.1593 17.7936 15.5499 17.6373 15.9141L17.5654 16.0685C17.278 16.6328 16.8405 17.1046 16.3038 17.434L16.0679 17.5661C15.66 17.7739 15.2196 17.8598 14.7237 17.9003C14.2362 17.9401 13.6327 17.9405 12.8867 17.9405H7.11122C6.36511 17.9405 5.76171 17.9401 5.27418 17.9003C4.84051 17.8649 4.44949 17.7952 4.08545 17.6391L3.93104 17.5661C3.36673 17.2785 2.89392 16.8414 2.56465 16.3044L2.43245 16.0685C2.22473 15.6608 2.13878 15.2211 2.09825 14.7254C2.05841 14.2379 2.05912 13.6345 2.05912 12.8884V7.02195C2.05912 6.99284 2.06422 6.96449 2.06758 6.93629C1.77931 6.52592 1.60858 6.02687 1.60858 5.4874V4.58527C1.60876 3.19084 2.73962 2.05975 4.1341 2.05975H15.8659ZM16.4984 7.92936C16.296 7.98169 16.0847 8.01288 15.8659 8.01291H4.1341C3.91478 8.01291 3.70246 7.98194 3.49955 7.92936V12.8884C3.49955 13.6582 3.50053 14.1927 3.53445 14.608C3.56769 15.0146 3.62923 15.244 3.71635 15.415L3.7925 15.5514C3.98339 15.8627 4.25749 16.1165 4.58464 16.2833L4.72529 16.3435C4.88095 16.3993 5.08638 16.4402 5.39158 16.4651C5.80685 16.4991 6.34138 16.5001 7.11122 16.5001H12.8867C13.6564 16.5001 14.1911 16.499 14.6063 16.4651C15.0128 16.432 15.2423 16.3703 15.4133 16.2833L15.5508 16.2061C15.8618 16.0152 16.116 15.7419 16.2827 15.415L16.3429 15.2732C16.3985 15.1177 16.4396 14.9128 16.4645 14.608C16.4985 14.1927 16.4984 13.6583 16.4984 12.8884V7.92936ZM4.1341 3.50019C3.53511 3.50019 3.0492 3.98631 3.04902 4.58527V5.4874C3.04902 6.08649 3.535 6.57248 4.1341 6.57248H15.8659C16.4648 6.57228 16.951 6.08638 16.951 5.4874V4.58527C16.9509 3.98644 16.4647 3.50038 15.8659 3.50019H4.1341Z',
            fill: 'currentColor',
          }),
          h('path', {
            key: 'slot',
            d: 'M12.7962 12.5661V11.0832H7.20548V12.5661L12.7962 12.5661Z',
            fill: 'currentColor',
          }),
        ]);
      }

      // 分支图标（IconBranchOutline16，与行内菜单「在新会话中分支」同款）。
      function BranchIcon(props) {
        var size = props.size || 16;
        return h('svg', {
          key: 'branch-icon',
          width: size,
          height: size,
          viewBox: '0 0 16 16',
          fill: 'none',
          xmlns: 'http://www.w3.org/2000/svg',
          'aria-hidden': 'true',
          style: { display: 'block' },
        }, h('path', {
          key: 'p',
          fillRule: 'evenodd',
          clipRule: 'evenodd',
          d: 'M13.0762 1.37207C14.0846 1.37228 14.9021 2.19077 14.9023 3.19922C14.9022 4.20772 14.0847 5.02518 13.0762 5.02539C12.2967 5.02539 11.6325 4.53691 11.3701 3.84961H4.35547C4.79397 4.26458 5.15861 4.7644 5.41699 5.33496L7.10645 9.06738C7.88526 10.7875 9.55104 11.9228 11.4189 12.0371C11.7085 11.4109 12.3411 10.9756 13.0762 10.9756C14.0843 10.9759 14.9023 11.7936 14.9023 12.8018C14.9023 13.81 14.0843 14.6277 13.0762 14.6279C12.2534 14.6279 11.5574 14.0832 11.3291 13.335C8.9868 13.1879 6.89981 11.7612 5.92285 9.60352L4.23242 5.87109C3.67503 4.64033 2.44878 3.84961 1.09766 3.84961V2.54883C1.10665 2.54883 1.11601 2.54975 1.125 2.5498L11.3701 2.54883C11.6326 1.86151 12.2969 1.37207 13.0762 1.37207ZM13.0762 12.2764C12.7858 12.2764 12.5508 12.5114 12.5508 12.8018C12.5508 13.0921 12.7858 13.3281 13.0762 13.3281C13.3664 13.3279 13.6025 13.092 13.6025 12.8018C13.6025 12.5115 13.3664 12.2766 13.0762 12.2764ZM13.0762 2.67285C12.7855 2.67285 12.55 2.90861 12.5498 3.19922C12.5499 3.48987 12.7855 3.72559 13.0762 3.72559C13.3667 3.72538 13.6024 3.48975 13.6025 3.19922C13.6023 2.90874 13.3666 2.67306 13.0762 2.67285Z',
          fill: 'currentColor',
        }));
      }

      // 展开/收起箭头（IconChevronDownOutline14 / IconChevronUpOutline14，up=true 为上箭头）。
      var CHEVRON_DOWN_D = 'M11.8486 5.5L11.4238 5.92383L8.69727 8.65137C8.44157 8.90706 8.21562 9.13382 8.01172 9.29785C7.79912 9.46883 7.55595 9.61756 7.25 9.66602C7.08435 9.69222 6.91565 9.69222 6.75 9.66602C6.44405 9.61756 6.20088 9.46883 5.98828 9.29785C5.78438 9.13382 5.55843 8.90706 5.30273 8.65137L2.57617 5.92383L2.15137 5.5L3 4.65137L3.42383 5.07617L6.15137 7.80273C6.42595 8.07732 6.59876 8.24849 6.74023 8.3623C6.87291 8.46904 6.92272 8.47813 6.9375 8.48047C6.97895 8.48703 7.02105 8.48703 7.0625 8.48047C7.07728 8.47813 7.12709 8.46904 7.25977 8.3623C7.40124 8.24849 7.57405 8.07732 7.84863 7.80273L10.5762 5.07617L11 4.65137L11.8486 5.5Z';
      var CHEVRON_UP_D = 'M2.15137 8.5L2.57617 8.07617L5.30273 5.34863C5.55843 5.09294 5.78438 4.86618 5.98828 4.70215C6.20088 4.53117 6.44405 4.38244 6.75 4.33398C6.91565 4.30778 7.08435 4.30778 7.25 4.33398C7.55595 4.38244 7.79912 4.53117 8.01172 4.70215C8.21561 4.86618 8.44157 5.09294 8.69727 5.34863L11.4238 8.07617L11.8486 8.5L11 9.34863L10.5762 8.92383L7.84863 6.19727C7.57405 5.92269 7.40124 5.75152 7.25977 5.6377C7.12709 5.53096 7.07728 5.52187 7.0625 5.51953C7.02105 5.51297 6.97895 5.51297 6.9375 5.51953C6.92272 5.52187 6.87291 5.53096 6.74023 5.6377C6.59876 5.75152 6.42595 5.92268 6.15137 6.19727L3.42383 8.92383L3 9.34863L2.15137 8.5Z';
      function ChevronIcon(props) {
        var size = props.size || 14;
        return h('svg', {
          key: 'chevron-icon',
          width: size,
          height: size,
          viewBox: '0 0 14 14',
          fill: 'none',
          xmlns: 'http://www.w3.org/2000/svg',
          'aria-hidden': 'true',
          style: { display: 'block' },
        }, h('path', {
          key: 'p',
          d: props.up ? CHEVRON_UP_D : CHEVRON_DOWN_D,
          fill: 'currentColor',
        }));
      }

      /** 追踪工作区栏头部搜索按钮的实时位置（锚点），并判断「设置（视图选项）」按钮是否可见。 */
      function useAnchorRect(enabled) {
        var pair = React.useState({ rect: null, visible: false, wide: true });
        var s = pair[0];
        var setS = pair[1];
        React.useEffect(function () {
          if (!enabled) {
            setS({ rect: null, visible: false, wide: true });
            return;
          }
          var SEARCH = 'button[aria-label="搜索会话"], button[aria-label="Search sessions"]';
          var SETTINGS = 'button[aria-label="视图选项"], button[aria-label="View options"]';
          function visibleEl(selector) {
            var els = window.document.querySelectorAll(selector);
            for (var i = 0; i < els.length; i++) {
              var el = els[i];
              if (el.offsetParent !== null && el.getBoundingClientRect().width > 0) return el;
            }
            return null;
          }
          function sameRect(a, b) {
            return a !== null && b !== null &&
              Math.round(a.left) === Math.round(b.left) &&
              Math.round(a.top) === Math.round(b.top) &&
              Math.round(a.width) === Math.round(b.width) &&
              Math.round(a.height) === Math.round(b.height);
          }
          function update() {
            var anchor = visibleEl(SEARCH);
            if (anchor === null) {
              setS(function (prev) {
                return prev.rect === null && !prev.visible ? prev : { rect: null, visible: false, wide: true };
              });
              return;
            }
            // 设置按钮存在但被隐藏（搜索展开态）→ 隐藏归档按钮；rail 模式无设置按钮 → 照常显示。
            var settingsCount = window.document.querySelectorAll(SETTINGS).length;
            var settingsVisible = visibleEl(SETTINGS) !== null;
            var show = settingsCount === 0 || settingsVisible;
            var wide = settingsCount > 0;
            var rect = anchor.getBoundingClientRect();
            setS(function (prev) {
              return prev.visible === show && prev.wide === wide && sameRect(prev.rect, rect)
                ? prev : { rect: rect, visible: show, wide: wide };
            });
          }
          update();
          var timer = window.setInterval(update, 400);
          window.addEventListener('resize', update);
          return function () {
            window.clearInterval(timer);
            window.removeEventListener('resize', update);
          };
        }, [enabled]);
        return s;
      }

      var headerBtnStyle = {
        position: 'fixed',
        width: '28px',
        height: '28px',
        display: 'inline-flex',
        alignItems: 'center',
        justifyContent: 'center',
        borderRadius: '8px',
        border: 'none',
        padding: '0',
        color: 'var(--dsw-alias-label-secondary, #555)',
        cursor: 'pointer',
        zIndex: 26,
      };
      var headerCountStyle = {
        position: 'absolute',
        top: '-3px',
        right: '-3px',
        display: 'inline-flex',
        alignItems: 'center',
        justifyContent: 'center',
        minWidth: '14px',
        height: '14px',
        padding: '0 4px',
        borderRadius: '7px',
        fontSize: '10px',
        lineHeight: '14px',
        color: 'var(--dsw-alias-label-tertiary, #777)',
        background: 'var(--dsw-alias-interactive-bg-hover, rgba(127,127,127,0.18))',
      };

      // ── 功能 5：归档浮层（按钮 + 面板一体，shell.overlay 席位，加性）──────────
      // 入口按钮为 fixed 覆盖按钮（锚定搜索/设置之间）；面板为浮层卡片：
      // 全屏透明 backdrop 接管点击（点击关闭）+ Esc 关闭。
      function ArchiveOverlay(props) {
        var settings = useSettings();
        var enabled = !!settings.showArchives;
        var st = useArchiveState();
        var data = useArchiveData();
        var anchor = useAnchorRect(enabled);
        var rect = anchor.rect;
        var anchored = anchor.visible;
        var hover = React.useState(false);
        var hovering = hover[0];
        var setHovering = hover[1];
        var queryState = React.useState('');
        var query = queryState[0];
        var setQuery = queryState[1];

        React.useEffect(function () {
          if (!st.open) return;
          function onKey(e) { if (e.key === 'Escape') setArchiveState({ open: false }); }
          window.addEventListener('keydown', onKey);
          return function () { window.removeEventListener('keydown', onKey); };
        }, [st.open]);

        // 关闭入口开关时同时收起面板，避免重新开启时突然弹出。
        React.useEffect(function () {
          if (!enabled && st.open) setArchiveState({ open: false });
        }, [enabled, st.open]);

        if (!enabled) return null;

        var archived = data.ws && Array.isArray(data.ws.archivedSessionIds) ? data.ws.archivedSessionIds : [];
        var byId = data.ss && data.ss.byId ? data.ss.byId : {};
        var rows = archived.map(function (id) {
          var sum = byId[id];
          return {
            id: id,
            title: sum ? (sum.displayTitle || id) : id,
            missing: !sum,
            running: sum ? !!sum.running : false,
            blank: sum ? !!sum.blank : false,
            updatedAt: sum ? sum.updatedAt : 0,
            cwd: sum ? sum.cwd : undefined,
          };
        }).sort(function (a, b) { return b.updatedAt - a.updatedAt; });

        var count = data.ws && Array.isArray(data.ws.archivedSessionIds) ? data.ws.archivedSessionIds.length : 0;

        // 搜索过滤：匹配标题 / 工作区路径 / 会话 id（不区分大小写）。
        var q = query.trim().toLowerCase();
        var visibleRows = rows;
        if (q !== '') {
          visibleRows = [];
          for (var fi = 0; fi < rows.length; fi++) {
            var fr = rows[fi];
            var hay = ((fr.title || '') + ' ' + (fr.cwd || '') + ' ' + fr.id).toLowerCase();
            if (hay.indexOf(q) !== -1) visibleRows.push(fr);
          }
        }

        function onToggle() {
          if (!anchored) return;
          setArchiveState({ open: !st.open });
        }

        function onTogglePreview(id) {
          if (st.expandedId === id) {
            setArchiveState({ expandedId: null });
            return;
          }
          setArchiveState({ expandedId: id });
          if (!archiveState.previewByRow[id]) fetchPreview(id);
        }

        var backdropStyle = {
          position: 'fixed',
          inset: '0',
          zIndex: 30,
          background: 'transparent',
        };
        var panelStyle = {
          position: 'fixed',
          zIndex: 31,
          width: '420px',
          maxWidth: 'calc(100vw - 24px)',
          maxHeight: 'min(60vh, 520px)',
          display: 'flex',
          flexDirection: 'column',
          overflow: 'hidden',
          borderRadius: '12px',
          border: '1px solid var(--dsw-alias-border-inverted, rgba(127,127,127,0.3))',
          background: 'var(--dsw-specific-menu, #ffffff)',
          boxShadow: 'var(--dsw-shadow-lv3, 0 8px 32px rgba(0,0,0,0.18))',
          color: 'var(--dsw-alias-label-primary, #222)',
        };
        var headerStyle = {
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          flex: 'none',
          minHeight: '44px',
          padding: '10px 12px',
          borderBottom: '1px solid var(--dsw-alias-border-inverted, rgba(127,127,127,0.15))',
        };
        var bodyStyle = {
          flex: '1',
          minHeight: '0',
          padding: '8px 12px 12px',
          overflowY: 'auto',
        };
        var rowStyle = {
          padding: '10px 8px',
          borderRadius: '10px',
          borderBottom: '1px solid var(--dsw-alias-border-inverted, rgba(127,127,127,0.12))',
        };
        var metaStyle = {
          display: 'flex',
          alignItems: 'center',
          gap: '8px',
          marginTop: '2px',
          fontSize: '12px',
          lineHeight: '18px',
          color: 'var(--dsw-alias-label-tertiary, #777)',
          minWidth: '0',
        };
        var noteStyle = {
          margin: '4px 0 0',
          fontSize: '12px',
          lineHeight: '18px',
          color: 'var(--dsw-alias-label-tertiary, #777)',
        };
        var previewStyle = {
          marginTop: '6px',
          padding: '8px 10px',
          borderRadius: '8px',
          background: 'var(--dsw-alias-interactive-bg-hover, rgba(127,127,127,0.1))',
          fontSize: '12px',
          lineHeight: '18px',
          color: 'var(--dsw-alias-label-secondary, #555)',
          whiteSpace: 'pre-wrap',
          wordBreak: 'break-word',
          maxHeight: '160px',
          overflowY: 'auto',
        };
        var statusBadgeStyle = {
          flex: 'none',
          padding: '0 6px',
          borderRadius: '7px',
          fontSize: '11px',
          lineHeight: '16px',
        };
        var iconBtnStyle = {
          display: 'inline-flex',
          alignItems: 'center',
          justifyContent: 'center',
          width: '26px',
          height: '26px',
          borderRadius: '8px',
          border: 'none',
          padding: '0',
          background: 'transparent',
          color: 'var(--dsw-alias-label-secondary, #555)',
          cursor: 'pointer',
        };
        var searchInputStyle = {
          flex: '1',
          minWidth: '0',
          height: '26px',
          margin: '0 8px',
          padding: '0 8px',
          borderRadius: '8px',
          border: '1px solid var(--dsw-alias-border-inverted, rgba(127,127,127,0.25))',
          background: 'var(--dsw-alias-interactive-bg-hover, rgba(127,127,127,0.08))',
          color: 'var(--dsw-alias-label-primary, #222)',
          fontSize: '12px',
          lineHeight: '26px',
          outline: 'none',
        };

        function previewBlock(id) {
          var preview = st.previewByRow[id];
          if (!preview || preview.state === 'loading') {
            return h('div', { key: 'prev', style: noteStyle }, '预览加载中…');
          }
          if (preview.state === 'error') {
            return h('div', { key: 'prev', style: Object.assign({}, errorTextStyle, { marginTop: '6px' }) },
              '预览失败：' + (preview.error || '未知错误'));
          }
          if (!preview.text) {
            return h('div', { key: 'prev', style: noteStyle }, '该任务没有可预览的文本内容');
          }
          return h('div', { key: 'prev', style: previewStyle }, preview.text);
        }

        var kids = [
          h('div', { key: 'header', style: headerStyle }, [
            h('span', { key: 'title', style: { fontSize: '14px', fontWeight: '500', lineHeight: '20px', flex: 'none' } },
              '归档任务（' + rows.length + '）'),
            h('input', {
              key: 'search',
              type: 'text',
              value: query,
              placeholder: '搜索归档任务…',
              'aria-label': '搜索归档任务',
              style: searchInputStyle,
              onChange: function (e) { setQuery(e.target.value); },
            }),
            h('button', {
              key: 'close',
              type: 'button',
              'aria-label': '关闭',
              style: { cursor: 'pointer', border: 'none', background: 'transparent', color: 'var(--dsw-alias-label-tertiary, #777)', fontSize: '16px', lineHeight: '20px', padding: '2px 6px', borderRadius: '8px', flex: 'none' },
              onClick: function () { setArchiveState({ open: false }); },
            }, '✕'),
          ]),
        ];
        // 每行一次函数调用（独立作用域）：修复 var 循环变量被所有闭包共享、
        // 导致点击任意一行的按钮都作用于最后一行的经典问题。
        // 行布局：左侧标题+元信息（flex 撑满），右侧两个 icon 按钮（在新会话中分支 / 展开收起预览）。
        function renderRow(row) {
          var expanded = st.expandedId === row.id;
          var busy = st.busyId === row.id;
          var forkError = st.forkErrorByRow[row.id];
          var rowKids = [
            h('div', { key: 'main', style: { display: 'flex', alignItems: 'center', gap: '6px', minWidth: '0' } }, [
              h('div', { key: 'info', style: { flex: '1', minWidth: '0' } }, [
                h('div', { key: 'head', style: { display: 'flex', alignItems: 'center', gap: '6px', minWidth: '0' } }, [
                  h('span', { key: 'title', style: { flex: '1', minWidth: '0', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', fontSize: '14px', lineHeight: '20px', color: 'var(--dsw-alias-label-primary, #222)' } }, row.title),
                  row.running && h('span', { key: 'run', style: Object.assign({}, statusBadgeStyle, { color: 'var(--dsw-alias-state-success-primary, #1a7f37)', background: 'rgba(26,127,55,0.12)' }) }, '运行中'),
                  row.blank && h('span', { key: 'blank', style: Object.assign({}, statusBadgeStyle, { color: 'var(--dsw-alias-label-tertiary, #777)', background: 'var(--dsw-alias-interactive-bg-hover, rgba(127,127,127,0.12))' }) }, '空白'),
                ]),
                h('div', { key: 'meta', style: metaStyle }, [
                  row.cwd ? h('span', { key: 'ws', title: row.cwd, style: { overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', maxWidth: '40%', flex: 'none' } }, workspaceBasename(row.cwd)) : null,
                  h('span', { key: 'time', style: { flex: 'none' } }, formatRelativeTime(row.updatedAt)),
                  row.missing ? h('span', { key: 'miss', style: { flex: 'none' } }, '（记录缺失）') : null,
                ]),
              ]),
              h('div', { key: 'ops', style: { display: 'flex', alignItems: 'center', gap: '2px', flex: 'none' } }, [
                h('button', {
                  key: 'fork',
                  type: 'button',
                  title: '在新会话中分支',
                  'aria-label': '在新会话中分支',
                  disabled: !!busy,
                  style: iconBtnStyle,
                  onClick: function () { forkArchivedSession(row.id); },
                }, h(BranchIcon, { key: 'ic', size: 16 })),
                h('button', {
                  key: 'preview',
                  type: 'button',
                  title: expanded ? '收起预览' : '展开预览',
                  'aria-label': expanded ? '收起预览' : '展开预览',
                  style: iconBtnStyle,
                  onClick: function () { onTogglePreview(row.id); },
                }, h(ChevronIcon, { key: 'ic', up: expanded, size: 14 })),
              ]),
            ]),
          ];
          if (forkError) rowKids.push(h('div', { key: 'ferr', style: Object.assign({}, errorTextStyle, { marginTop: '6px' }) }, forkError));
          if (expanded) rowKids.push(previewBlock(row.id));
          return h('div', { key: row.id, style: rowStyle }, rowKids);
        }

        var bodyKids = [];
        if (rows.length === 0) {
          bodyKids.push(h('p', { key: 'empty', style: Object.assign({}, noteStyle, { padding: '16px 4px', textAlign: 'center' }) },
            '暂无归档任务。\n在侧边栏会话的更多菜单中可归档会话。'));
        } else if (visibleRows.length === 0) {
          bodyKids.push(h('p', { key: 'empty', style: Object.assign({}, noteStyle, { padding: '16px 4px', textAlign: 'center' }) },
            '没有匹配「' + query.trim() + '」的归档任务。'));
        } else {
          for (var r = 0; r < visibleRows.length; r++) bodyKids.push(renderRow(visibleRows[r]));
        }
        kids.push(h('div', { key: 'body', style: bodyStyle }, bodyKids));
        kids.push(h('div', { key: 'foot', style: Object.assign({}, noteStyle, { flex: 'none', padding: '8px 12px', borderTop: '1px solid var(--dsw-alias-border-inverted, rgba(127,127,127,0.15))' }) },
          '暂不支持取消归档和删除归档；需要重新使用请点「在新会话中分支」生成新会话副本。'));

        // 入口按钮：fixed 覆盖，锚定搜索按钮左侧（工作区栏头部「标题与搜索之间」；
        // rail 模式无设置按钮 → 放在搜索按钮右侧，避免贴出屏幕左缘）。
        // 注意：rect 首次渲染时为 null（锚点 hook 效果尚未运行），必须判空再取坐标。
        var btnLeft = rect !== null ? (anchor.wide ? rect.left - 32 : rect.right + 4) : 0;
        var button = null;
        if (anchored) {
          button = h('button', {
            key: 'archive-btn',
            type: 'button',
            'aria-label': '归档任务',
            'aria-expanded': st.open,
            title: '归档任务',
            style: Object.assign({}, headerBtnStyle, {
              left: btnLeft,
              top: rect.top + Math.max(0, (rect.height - 28) / 2),
              background: hovering ? 'var(--dsw-alias-interactive-bg-hover, rgba(127,127,127,0.12))' : 'transparent',
              pointerEvents: 'auto',
            }),
            onClick: onToggle,
            onMouseEnter: function () { setHovering(true); },
            onMouseLeave: function () { setHovering(false); },
          }, [
            h(ArchiveIcon, { key: 'ic', size: 16 }),
            settings.showArchiveCount && count > 0 && h('span', { key: 'cnt', style: headerCountStyle }, String(count)),
          ]);
        }

        // 面板：锚定入口按钮下方；按钮不可见时兜底贴左下角。
        var panel = null;
        if (st.open) {
          var panelPos = rect && rect.width > 0
            ? { top: rect.bottom + 6, left: btnLeft }
            : { left: '12px', bottom: '64px' };
          panel = h('div', {
            key: 'archives-backdrop',
            style: Object.assign({}, backdropStyle, { pointerEvents: 'auto' }),
            onClick: function () { setArchiveState({ open: false }); },
          }, [
            h('section', {
              key: 'panel',
              'aria-label': '归档任务',
              style: Object.assign({}, panelStyle, panelPos),
              onClick: function (e) { e.stopPropagation(); },
            }, kids),
          ]);
        }

        // shell.overlay 默认点击穿透；根容器 pointer-events none，按钮/面板各自接管。
        return h('div', {
          key: 'archives-root',
          style: { position: 'fixed', inset: '0', zIndex: 25, pointerEvents: 'none' },
        }, [button, panel]);
      }

      // ── 功能 6：对话页左侧用户消息锚点条（shell.overlay 席位，加性）─────────
      // 锚定对话滚动容器 [data-conversation-scroll] 的左缘，画一根细竖条：
      // 每条用户消息（data-chat-flow-kind="user"/"steering"）= 一根等距横条；
      // 视口顶部那条用户消息的横条最长（唯一）；点击横条平滑滚动到对应消息。
      // 隐藏条件：设置关闭、无会话/空白会话、无用户消息、滚动容器过窄（<480px，
      // 内容可能贴左缘）或非 chat 视图（无 [data-chat-flow]）。
      function ChatMapOverlay() {
        var settings = useSettings();
        var enabled = !!settings.showChatMap;
        var state = React.useState({ visible: false, rows: [], texts: [], current: -1, rect: null });
        var s = state[0];
        var setS = state[1];
        var cacheRef = React.useRef({ scrollport: null, flow: null, rows: [], texts: [] });
        var rafRef = React.useRef(0);
        var hoverState = React.useState(-1);
        var hoverIdx = hoverState[0];
        var setHoverIdx = hoverState[1];

        React.useEffect(function () {
          if (!enabled) {
            setS({ visible: false, rows: [], texts: [], current: -1, rect: null });
            return;
          }
          var cache = cacheRef.current;

          /** 取行的可见文本（压缩空白；供悬浮提示）。
           *  用户消息行 DOM：wrapper → userRow([data-time-hover-root]) →
           *  [userStack(正文气泡 + 引用摘要), actions(时间戳/复制/分支按钮)]。
           *  textContent 会把 actions 行的文本（含时间）也抓进来，故克隆后
           *  只移除 userRow 直属的 actions 行（稳定锚点：复制按钮 aria-label
           *  复制/Copy；上溯到 data-time-hover-root 的直属子层为止）。 */
          function rowText(row) {
            var clone = row.cloneNode(true);
            var host = clone.querySelector('[data-time-hover-root]');
            var copyBtn = clone.querySelector('button[aria-label="复制"], button[aria-label="Copy"], button[aria-label="已复制"], button[aria-label="Copied"]');
            if (copyBtn !== null && host !== null) {
              var node = copyBtn;
              while (node !== null && node.parentElement !== null && node.parentElement !== host) node = node.parentElement;
              if (node !== null && node.parentElement === host) host.removeChild(node);
              else copyBtn.remove();
            }
            var t = clone.textContent ? clone.textContent : '';
            return t.replace(/\s+/g, ' ').trim();
          }

          function requery() {
            var scrollport = window.document.querySelector('[data-conversation-scroll]');
            var flow = scrollport === null ? null : scrollport.querySelector('[data-chat-flow]');
            var rows = flow === null
              ? []
              : Array.prototype.slice.call(flow.querySelectorAll('[data-chat-flow-kind="user"], [data-chat-flow-kind="steering"]'));
            cache.scrollport = scrollport;
            cache.flow = flow;
            cache.rows = rows;
            cache.texts = rows.map(rowText);
            return rows;
          }

          /** 最后一个视口顶部线（topLine）上方的用户消息下标；无则 0。 */
          function computeCurrent(rows, topLine) {
            var lo = 0;
            var hi = rows.length - 1;
            var ans = -1;
            while (lo <= hi) {
              var mid = (lo + hi) >> 1;
              if (rows[mid].getBoundingClientRect().top <= topLine) {
                ans = mid;
                lo = mid + 1;
              } else {
                hi = mid - 1;
              }
            }
            return ans < 0 ? 0 : ans;
          }

          function sameRect(a, b) {
            return a !== null && b !== null &&
              Math.round(a.left) === Math.round(b.left) &&
              Math.round(a.top) === Math.round(b.top) &&
              Math.round(a.width) === Math.round(b.width) &&
              Math.round(a.height) === Math.round(b.height);
          }

          function commit(next) {
            setS(function (prev) {
              var same = prev.visible === next.visible &&
                prev.current === next.current &&
                prev.rows.length === next.rows.length &&
                (next.rows.length === 0 || prev.rows[0] === next.rows[0]) &&
                (next.texts.length === 0 || prev.texts[0] === next.texts[0]) &&
                sameRect(prev.rect, next.rect);
              return same ? prev : next;
            });
          }

          /** 重新查询 DOM 并整帧更新（开关/换会话/加载更早消息/窗口变化时用）。 */
          function tick(full) {
            if (full) {
              requery();
              attach();
            }
            var rows = cache.rows;
            var texts = cache.texts;
            var scrollport = cache.scrollport;
            if (scrollport === null || rows.length === 0) {
              commit({ visible: false, rows: [], texts: [], current: -1, rect: null });
              return;
            }
            var rect = scrollport.getBoundingClientRect();
            if (rect.width < 480 || rect.height <= 0) {
              commit({ visible: false, rows: [], texts: [], current: -1, rect: null });
              return;
            }
            // 贴底判定：已滚到最底部（或内容不满一屏）时，用户在看最新内容，
            // 当前条应指向最后一条用户消息，而不是视口顶部那条（短对话时顶部
            // 可能还是前几条）。未贴底时维持「视口顶部那条」判定。
            var atBottom = scrollport.scrollHeight - scrollport.scrollTop - scrollport.clientHeight <= 25;
            var current = atBottom ? rows.length - 1 : computeCurrent(rows, rect.top + 4);
            commit({ visible: true, rows: rows, texts: texts, current: current, rect: rect });
          }

          function onScroll() {
            if (rafRef.current !== 0) return;
            rafRef.current = window.requestAnimationFrame(function () {
              rafRef.current = 0;
              tick(false);
            });
          }

          function onResize() { tick(true); }

          // 首次查询 + 常驻轮询兜底（flow 被整体替换、视图切换等 MutationObserver
          // 监听不到的场景）。
          var timer = window.setInterval(function () { tick(true); }, 300);
          var mo = null;
          var ro = null;
          var scrollport = null;
          var flow = null;
          /** 对齐当前 scrollport/flow 的滚动、变更、尺寸监听（首次与替换后都调用）。 */
          function attach() {
            if (cache.scrollport !== scrollport) {
              if (scrollport !== null) scrollport.removeEventListener('scroll', onScroll);
              scrollport = cache.scrollport;
              if (scrollport !== null) scrollport.addEventListener('scroll', onScroll, { passive: true });
            }
            if (cache.flow !== flow) {
              if (mo !== null) { mo.disconnect(); mo = null; }
              flow = cache.flow;
              if (flow !== null && typeof window.MutationObserver === 'function') {
                mo = new window.MutationObserver(function () { tick(true); });
                mo.observe(flow, { childList: true, subtree: true });
              }
            }
            if (scrollport !== null && typeof window.ResizeObserver === 'function' && ro === null) {
              ro = new window.ResizeObserver(onResize);
              ro.observe(scrollport);
            }
          }
          window.addEventListener('resize', onResize);
          tick(true);

          return function () {
            window.clearInterval(timer);
            window.removeEventListener('resize', onResize);
            if (rafRef.current !== 0) window.cancelAnimationFrame(rafRef.current);
            if (mo !== null) mo.disconnect();
            if (ro !== null) ro.disconnect();
            if (scrollport !== null) scrollport.removeEventListener('scroll', onScroll);
          };
        }, [enabled]);

        if (!enabled || !s.visible || s.rows.length === 0 || s.rect === null) return null;
        var N = s.rows.length;
        var stripW = 14;
        var stripLeft = s.rect.left + 6;
        // 锚点条收缩到对话区域中部：固定高度居中，横条间距紧凑、不铺满整屏。
        var stripH = Math.min(s.rect.height - 20, 120);
        var stripTop = s.rect.top + Math.max(0, (s.rect.height - stripH) / 2);
        var slot = stripH / N;
        var bars = [];

        function jumpTo(idx) {
          var row = s.rows[idx];
          var sp = cacheRef.current.scrollport;
          if (!row || !sp) return;
          var sr = sp.getBoundingClientRect();
          var rr = row.getBoundingClientRect();
          var target = sp.scrollTop + (rr.top - sr.top) - 16;
          var floor = sp.scrollHeight - sp.clientHeight;
          if (target < 0) target = 0;
          if (target > floor) target = floor;
          try { sp.scrollTo({ top: target, behavior: 'smooth' }); } catch (err) { sp.scrollTop = target; }
        }

        for (var i = 0; i < N; i++) {
          (function (idx, isCurrent) {
            var btnH = Math.max(3, Math.min(10, slot));
            bars.push(h('button', {
              key: 'cm-' + idx,
              type: 'button',
              'aria-label': '跳转到第 ' + (idx + 1) + ' 条用户消息',
              style: {
                position: 'absolute',
                left: 0,
                top: (idx + 0.5) * slot - btnH / 2,
                width: stripW,
                height: btnH,
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'flex-start',
                border: 'none',
                padding: 0,
                cursor: 'pointer',
                background: 'transparent',
                pointerEvents: 'auto',
              },
              onMouseEnter: function () { setHoverIdx(idx); },
              onMouseLeave: function () { setHoverIdx(-1); },
              onClick: function () { jumpTo(idx); },
            }, h('span', {
              style: {
                display: 'block',
                // 当前条或悬浮条变长；颜色只区分「当前」：当前条 #679efe，其余灰色。
                width: (isCurrent || hoverIdx === idx) ? stripW : 8,
                height: 3,
                borderRadius: 2,
                background: isCurrent
                  ? '#679efe'
                  : 'var(--dsw-alias-label-tertiary, rgba(127,127,127,0.55))',
                transition: 'width .15s ease, background .15s ease',
              },
            })));
          })(i, i === s.current);
        }

        // 自绘即时提示（原生 title 有系统级延迟，改为 mouseenter 立即渲染、
        // mouseleave 立即消失的浮层；定位在锚点条右侧，视口边缘自动回夹）。
        var tooltip = null;
        if (hoverIdx >= 0 && hoverIdx < N) {
          var tipText = truncate(s.texts[hoverIdx], 20);
          if (tipText === '') tipText = '第 ' + (hoverIdx + 1) + ' 条用户消息';
          var tipCenter = stripTop + (hoverIdx + 0.5) * slot;
          var tipW = 240;
          var tipH = 28;
          var tipLeft = stripLeft + stripW + 8;
          var tipTop = tipCenter - tipH / 2;
          if (tipLeft + tipW > window.innerWidth - 8) tipLeft = Math.max(8, stripLeft - tipW - 8);
          if (tipTop < 8) tipTop = 8;
          if (tipTop + tipH > window.innerHeight - 8) tipTop = window.innerHeight - tipH - 8;
          tooltip = h('div', {
            key: 'cm-tip',
            style: {
              position: 'fixed',
              left: tipLeft,
              top: tipTop,
              maxWidth: tipW,
              zIndex: 25,
              pointerEvents: 'none',
              padding: '5px 10px',
              borderRadius: '8px',
              border: '1px solid var(--dsw-alias-border-inverted, rgba(127,127,127,0.3))',
              background: 'var(--dsw-specific-menu, #ffffff)',
              boxShadow: 'var(--dsw-shadow-lv2, 0 4px 16px rgba(0,0,0,0.14))',
              color: 'var(--dsw-alias-label-primary, #222)',
              fontSize: '12px',
              lineHeight: '18px',
              whiteSpace: 'nowrap',
              overflow: 'hidden',
              textOverflow: 'ellipsis',
            },
          }, tipText);
        }

        return h('div', {
          key: 'chatmap-root',
          style: { position: 'fixed', inset: '0', zIndex: 23, pointerEvents: 'none' },
        }, [
          h('div', {
            key: 'strip',
            style: {
              position: 'fixed',
              left: stripLeft,
              top: stripTop,
              width: stripW,
              height: stripH,
              zIndex: 24,
              pointerEvents: 'auto',
            },
          }, bars),
          tooltip,
        ]);
      }

      // ── 插件注册 ────────────────────────────────────────────────────────────
      var appCtx = null;
      var connectionFace = null;
      var remoteFace = null;
      var appSessions = null;
      var workspacesFace = null;

      exports.inject = ['slots', 'connection', 'sessions', 'remote', 'workspaces'];

      exports.apply = function (ctx) {
        appCtx = ctx;
        connectionFace = ctx.get('connection') || null;
        remoteFace = ctx.get('remote') || null;
        appSessions = ctx.get('sessions') || null;
        workspacesFace = ctx.get('workspaces') || null;
        var slots = ctx.get('slots');
        if (slots === undefined) return;

        // 全局外部链接捕获（仅鲸仔内嵌环境生效）：对话/搜索结果里的
        // <a target="_blank"> 点击在跨源 iframe 中会被 Tauri WebView 拦截，
        // 捕获阶段统一改走桥通道 → 系统默认浏览器。
        var uninstallLinks = installExternalLinkCapture();
        if (typeof ctx.on === 'function') {
          ctx.on('dispose', uninstallLinks);
        }

        // 功能 2/3/4：每轮完成后的尾部扩展链（首个且唯一条目，每轮都挂载）。
        slots.inject('conversation.chat.turnTail', function () {
          return slots.register({
            name: 'conversation.chat.turnTail',
            select: function () { return true; },
          }, TurnTailEnhance);
        });

        // 功能 1：输入工具行右侧的供应商 chip。
        slots.inject('conversation.input.right', function () {
          return slots.register({
            name: 'conversation.input.right',
            id: 'dsh-webui-plus-provider',
            order: 60,
          }, ProviderChip);
        });

        // 功能 5：归档入口 + 面板（shell.overlay 浮层；按钮 fixed 锚定到工作区栏头部
        // 搜索按钮左侧，面板点击穿透由自身接管）。
        slots.inject('shell.overlay', function () {
          return slots.register({
            name: 'shell.overlay',
            id: 'dsh-webui-plus-archives',
            order: 10,
          }, ArchiveOverlay);
        });

        // 功能 6：对话页左侧用户消息锚点条（shell.overlay 浮层；加性，点击穿透由自身接管）。
        slots.inject('shell.overlay', function () {
          return slots.register({
            name: 'shell.overlay',
            id: 'dsh-webui-plus-chatmap',
            order: 20,
          }, ChatMapOverlay);
        });

        // 功能 4：设置页 + 鲸仔推荐位。
        slots.inject('settings.section', function () {
          return slots.register({
            name: 'settings.section',
            id: 'dsh-webui-plus',
            order: 30,
            label: 'WebUI+ 增强',
          }, SettingsSection);
        });
      };

      return module.exports;
    },
  });
})();
