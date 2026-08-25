// 鲸仔设置分区 —— DSH Web 客户端插件（浏览器半体，手写 bundle，无构建步骤）。
// 装载契约：window.__ModuleLoader__.load({ id, factory }) 的 CJS 工厂格式；
// factory 的 require 仅解析平台模块表成员（react / react/jsx-runtime，共享 React 实例）。
// 与鲸仔主窗口通过 postMessage 通信（channel: 'whalito'）：
//   上行 { channel:'whalito', type:'ping' | 'action', action, value }
//   下行 { channel:'whalito', type:'hello' | 'settings' | 'status' | 'error', settings, status, message }
// 内嵌时接管页面右键：contextmenu 被 preventDefault（屏蔽 WebView2 默认菜单），
// 上行 context-menu(x,y) 通知鲸仔主窗口弹出「刷新页面 / 重启服务器」菜单，
// 点击/滚轮/Escape 上行 context-menu-close 关闭它。
// 内嵌时接管会话日志导出：包装 HTMLAnchorElement.prototype.click，导出锚点
// 上行 whalito-download(url, filename)，由鲸仔下载到配置目录并提示。
// 内嵌时接管外部链接：捕获阶段拦截 <a target="_blank"> 的 http(s) 链接，
// 上行 open-url，由鲸仔用系统默认浏览器打开（WebView2 会拦截 window.open）。
// 非鲸仔环境（普通浏览器，window.parent === window）不注册分区、不接管。
//
// 注意：jsx(type, config, maybeKey) 的第三个位置参数是 key 而不是 children，
// children 必须放进 config.children（否则 dev/prod 运行时都会丢弃子节点，分区空白）。
(function () {
  'use strict';

  if (window.__ModuleLoader__ === undefined) {
    throw new Error('whalito-dsh-settings: __ModuleLoader__ is missing (not a dsh web boot)');
  }

  window.__ModuleLoader__.load({
    id: '@entireyu/whalito-dsh-settings',
    factory: function (require) {
      var module = { exports: {} };
      var exports = module.exports;

      var React = require('react');
      var jsx = require('react/jsx-runtime').jsx;

      var CHANNEL = 'whalito';

      // jsx 包装：children 放进 config.children，key 走第三个位置参数
      // （jsx(type, config, maybeKey) —— 跨 dev/prod 都可靠的调用形式）。
      function h(type, props, kids) {
        var p = Object.assign({}, props || {});
        var key = p.key;
        delete p.key;
        if (kids !== undefined) p.children = kids;
        return key === undefined ? jsx(type, p) : jsx(type, p, key);
      }

      function postToParent(msg) {
        try {
          window.parent.postMessage(msg, '*');
        } catch (err) {
          /* parent 已关闭：分区保持现状即可 */
        }
      }

      function sendPing() {
        postToParent({ channel: CHANNEL, type: 'ping' });
      }

      function sendAction(action, extra) {
        var msg = { channel: CHANNEL, type: 'action', action: action };
        if (extra) Object.assign(msg, extra);
        postToParent(msg);
      }

      // 内嵌鲸仔时接管右键：屏蔽 WebView2 默认菜单，改为让鲸仔主窗口在
      // 点击位置弹出「刷新页面 / 重启服务器」自定义菜单。普通浏览器打开
      // （window.parent === window）不接管，保留浏览器原生右键菜单。
      if (window.parent !== window) {
        var contextMenuOpen = false;
        window.addEventListener('contextmenu', function (e) {
          e.preventDefault();
          contextMenuOpen = true;
          // 此刻 iframe 仍有焦点：先快照选区/光标，供后续复制/剪切/粘贴使用。
          snapshotClipboard();
          postToParent({
            channel: CHANNEL,
            type: 'action',
            action: 'context-menu',
            x: e.clientX,
            y: e.clientY,
            dbg: {
              ctx: clipboardCtx ? (clipboardCtx.field ? 'field' : 'doc') : null,
              sel: clipboardCtx
                ? (clipboardCtx.field ? clipboardCtx.end - clipboardCtx.start : clipboardCtx.text.length)
                : -1,
            },
          });
        });
        function closeContextMenu() {
          if (!contextMenuOpen) return;
          contextMenuOpen = false;
          postToParent({ channel: CHANNEL, type: 'action', action: 'context-menu-close' });
        }
        // 点击（捕获阶段）/滚轮/Escape 都视为关闭菜单的意图。
        window.addEventListener('click', closeContextMenu, true);
        window.addEventListener('wheel', closeContextMenu, true);
        window.addEventListener('keydown', function (e) {
          if (e.key === 'Escape') closeContextMenu();
        });
      }

      // ---- 剪贴板协作（右键菜单复制/剪切/粘贴） ----
      // 选区/光标在本页面（iframe）里，系统剪贴板由父窗口经 Rust 读写：
      // 上行选区文本（clipboard-set），下行粘贴文本 / 剪切回删确认。
      // 关键：右键后用户去点父窗口的菜单，iframe 会失焦——activeElement 变
      // body、输入框选区不可达，因此右键瞬间就把「输入框+光标区间」或
      // 「文档选区」快照下来，复制/剪切/粘贴全部基于快照执行。
      var clipboardCtx = null;

      function isTextField(el) {
        if (!el || !el.tagName) return false;
        if (el.tagName === 'TEXTAREA') return true;
        if (el.tagName !== 'INPUT') return false;
        var t = (el.type || '').toLowerCase();
        return t !== 'checkbox' && t !== 'radio' && t !== 'button' && t !== 'submit' &&
          t !== 'reset' && t !== 'file' && t !== 'image' && t !== 'color' && t !== 'range';
      }

      // 右键时快照当前选区/光标；无选中内容时输入框/可编辑区仍保留（粘贴目标）。
      function snapshotClipboard() {
        clipboardCtx = null;
        var el = document.activeElement;
        if (isTextField(el)) {
          var start = el.selectionStart == null ? 0 : el.selectionStart;
          var end = el.selectionEnd == null ? start : el.selectionEnd;
          // 文本在复制时才取（走组件自己的复制管线，见 clipboardText）。
          clipboardCtx = { field: el, start: start, end: end, text: '' };
          return;
        }
        var sel = window.getSelection();
        if (!sel || sel.rangeCount === 0) return;
        var editable = el && el.isContentEditable ? el : null;
        if (sel.isCollapsed) {
          // 可编辑区域（contenteditable，如聊天输入框）的光标也是粘贴目标。
          if (editable) {
            clipboardCtx = { editable: editable, range: sel.getRangeAt(0).cloneRange(), text: '' };
          }
          return;
        }
        var text = sel.toString();
        if (!text) return;
        clipboardCtx = { range: sel.getRangeAt(0).cloneRange(), text: text, editable: editable };
      }

      // 取复制文本：输入框走组件自己的复制管线（派发合成 copy 事件，claim
      // 令牌会展开成可见文本，与 Ctrl+C 一致——直接取 value 子串会把 U+FFFC
      // 隐形占位符带进剪贴板）；组件未接管（纯文本走原生路径）时回退原始子串。
      function clipboardText() {
        if (!clipboardCtx) return '';
        if (clipboardCtx.field) {
          var el = clipboardCtx.field;
          var dt = new DataTransfer();
          var ev = new ClipboardEvent('copy', { bubbles: true, cancelable: true });
          try { Object.defineProperty(ev, 'clipboardData', { value: dt }); } catch (e) { /* 忽略 */ }
          try {
            el.dispatchEvent(ev);
            var clean = dt.getData('text/plain');
            if (clean) return clean;
          } catch (e) { /* 忽略 */ }
          return el.value.substring(clipboardCtx.start, clipboardCtx.end);
        }
        return clipboardCtx.text || '';
      }

      // 可编辑区域插入：恢复选区后走 execCommand('insertText')（触发 input
      // 事件，React 等框架能感知）；失败则直接操作 DOM 节点兜底。
      function insertIntoEditable(el, text, savedRange) {
        try {
          el.focus();
          var sel = window.getSelection();
          if (!sel) return;
          if (savedRange) {
            sel.removeAllRanges();
            sel.addRange(savedRange.cloneRange());
          } else if (sel.rangeCount === 0) {
            var caret = document.createRange();
            caret.selectNodeContents(el);
            caret.collapse(false);
            sel.addRange(caret);
          }
          var ok = false;
          try { ok = document.execCommand('insertText', false, text); } catch (e) { ok = false; }
          if (!ok && sel.rangeCount > 0) {
            var rr = sel.getRangeAt(0);
            rr.deleteContents();
            var node = document.createTextNode(text);
            rr.insertNode(node);
            sel.removeAllRanges();
            var after = document.createRange();
            after.setStartAfter(node);
            after.collapse(true);
            sel.addRange(after);
          }
        } catch (e) { /* 忽略 */ }
      }

      // 向输入框派发合成 paste 事件：DSH 聊天输入框是受控组件（可见文字由
      // backdrop 从 React 状态渲染，textarea 字形透明），直接改 DOM 值不会
      // 同步状态 → 粘贴内容"选中才可见"。组件自带 onPaste 管线（preventDefault
      // + 机器插入），合成事件能走通它；defaultPrevented 为 false 时回退到
      // 普通输入框的 setRangeText。
      function pasteIntoField(el, text, start, end) {
        try {
          el.focus({ preventScroll: true });
          if (start != null && end != null) el.setSelectionRange(start, end);
        } catch (e) { /* 忽略 */ }
        var dt = new DataTransfer();
        dt.setData('text/plain', text);
        var ev = new ClipboardEvent('paste', { bubbles: true, cancelable: true });
        try { Object.defineProperty(ev, 'clipboardData', { value: dt }); } catch (e) { /* 忽略 */ }
        el.dispatchEvent(ev);
        if (!ev.defaultPrevented) {
          var s = el.selectionStart == null ? 0 : el.selectionStart;
          var en = el.selectionEnd == null ? s : el.selectionEnd;
          el.setRangeText(text, s, en, 'end');
        }
      }

      // 在快照位置插入文本（输入框走合成 paste 事件；可编辑区域走
      // insertIntoEditable；文档选区 deleteContents + insertNode）。
      // 快照失效时回退实时选区。
      function clipboardInsert(text) {
        if (!clipboardCtx) {
          // 无快照（右键点在空白处）：回退当前活动元素，尽量粘贴。
          var live = document.activeElement;
          if (isTextField(live)) {
            pasteIntoField(live, text, null, null);
          } else if (live && live.isContentEditable) {
            insertIntoEditable(live, text, null);
          }
          return;
        }
        try {
          if (clipboardCtx.field) {
            pasteIntoField(clipboardCtx.field, text, clipboardCtx.start, clipboardCtx.end);
            return;
          }
          if (clipboardCtx.editable) {
            insertIntoEditable(clipboardCtx.editable, text, clipboardCtx.range);
            return;
          }
          var r = clipboardCtx.range;
          r.deleteContents();
          var node = document.createTextNode(text);
          r.insertNode(node);
          var sel = window.getSelection();
          if (sel) {
            sel.removeAllRanges();
            var after = document.createRange();
            after.setStartAfter(node);
            after.collapse(true);
            sel.addRange(after);
          }
        } catch (e) {
          // 快照引用已失效（React 重渲染等）：尝试实时选区一次。
          try {
            var liveSel = window.getSelection();
            if (liveSel && !liveSel.isCollapsed) {
              var lr = liveSel.getRangeAt(0);
              lr.deleteContents();
              lr.insertNode(document.createTextNode(text));
            }
          } catch (e2) { /* 忽略 */ }
        }
      }

      // 下行剪贴板指令监听（工厂级，插件加载时注册一次）：
      // 右键菜单可在 DSH 页面任意位置弹出（聊天页/设置页），而 WhalitoSection
      // 组件只在「鲸仔设置」页挂载，其内部监听器在其它页面会被卸载——
      // 剪贴板指令必须在这里统一接收，否则点击菜单后消息石沉大海。
      window.addEventListener('message', function (event) {
        var data = event.data;
        if (!data || data.channel !== CHANNEL) return;
        if (data.type !== 'action') return;
        if (data.action === 'context-copy') {
          var text = clipboardText();
          if (text) {
            sendAction('clipboard-set', {
              text: text,
              dbg: {
                ctx: clipboardCtx && clipboardCtx.field ? 'field' : 'doc',
                len: text.length,
              },
            });
          } else {
            sendAction('clipboard-noop', { why: 'copy-no-selection' });
          }
        } else if (data.action === 'context-paste') {
          if (typeof data.text === 'string' && data.text !== '') clipboardInsert(data.text);
        }
      });

      // 接管 DSH 会话日志导出下载：DSH 用程序化 anchor.click() 触发浏览器
      // 下载，合成 click 事件不经过 document，只能包装原型方法。href 指向
      // /api/session.export 时阻止默认下载，改由鲸仔下载到配置目录并提示。
      if (
        window.parent !== window &&
        window.HTMLAnchorElement &&
        window.HTMLAnchorElement.prototype
      ) {
        var origAnchorClick = window.HTMLAnchorElement.prototype.click;
        window.HTMLAnchorElement.prototype.click = function () {
          var href = typeof this.href === 'string' ? this.href : '';
          if (href.indexOf('/api/session.export') !== -1) {
            postToParent({
              channel: CHANNEL,
              type: 'action',
              action: 'whalito-download',
              url: href,
              filename:
                typeof this.download === 'string' && this.download !== ''
                  ? this.download
                  : '',
            });
            return;
          }
          return origAnchorClick.call(this);
        };
      }

      // 接管 DSH 的剪贴板写入：内嵌 iframe（WebView2）里
      // navigator.clipboard.writeText 常因焦点/权限策略失败，导致 DSH 里所有
      // 「复制」按钮（消息 / 代码块 / 搜索结果 / 表格 / JSON）点了没反应。
      // 包装为：原生优先；失败则上行 clipboard-set，由鲸仔主窗口经 Rust
      // 写入系统剪贴板（与右键菜单复制同一链路）。patch 后始终 resolve，
      // DSH 侧的 await（writeClipboard / useCopyFeedback / JsonTree）会显示
      // 「复制成功」而非「复制失败」。
      if (window.parent !== window) {
        var bridgeWriteText = function (text) {
          return new Promise(function (resolve) {
            try {
              postToParent({
                channel: CHANNEL,
                type: 'action',
                action: 'clipboard-set',
                text: String(text),
              });
            } catch (e) {
              /* 父窗口已关闭：忽略 */
            }
            resolve();
          });
        };
        if (navigator.clipboard) {
          var origWriteText = typeof navigator.clipboard.writeText === 'function'
            ? navigator.clipboard.writeText.bind(navigator.clipboard)
            : null;
          navigator.clipboard.writeText = function (text) {
            if (origWriteText) {
              try {
                return Promise.resolve(origWriteText(text)).catch(function () {
                  return bridgeWriteText(text);
                });
              } catch (e) {
                return bridgeWriteText(text);
              }
            }
            return bridgeWriteText(text);
          };
        } else {
          // 极少数无 Clipboard API 的环境：直接注入桥实现。
          Object.defineProperty(navigator, 'clipboard', {
            configurable: true,
            value: { writeText: bridgeWriteText },
          });
        }
      }

      // 接管外部链接打开：DSH 把对话/搜索结果中的 http(s) 链接渲染为
      // <a target="_blank" rel="noopener noreferrer">，内嵌 iframe（WebView2）
      // 里点击会被 WebView 拦截（无反应）。捕获阶段拦截这类链接，上行
      // open-url，由鲸仔主窗口 invoke(open_url) 用系统默认浏览器打开。
      // 普通浏览器打开（window.parent === window）不接管，保留原生行为。
      if (window.parent !== window) {
        window.document.addEventListener(
          'click',
          function (e) {
            var t = e && e.target;
            var el = t instanceof window.HTMLElement ? t : null;
            while (el !== null && el.tagName !== 'A') el = el.parentElement;
            if (el === null || el.tagName !== 'A') return;
            var href = typeof el.getAttribute === 'function' ? el.getAttribute('href') : null;
            if (typeof href !== 'string' || href === '') return;
            // 只拦外部链接（含协议或 // 开头）；站内相对路径/锚点不动。
            if (href.indexOf('://') === -1 && href.indexOf('//') !== 0) return;
            var wantsBlank = el.target === '_blank' || el.getAttribute('target') === '_blank';
            if (!wantsBlank) return;
            e.preventDefault();
            e.stopPropagation();
            postToParent({
              channel: CHANNEL,
              type: 'action',
              action: 'open-url',
              url: href,
            });
          },
          true,
        );
      }

      // ---- 内联样式（中性色 + currentColor，适配明暗主题） ----
      var styles = {
        root: { display: 'flex', flexDirection: 'column', gap: '16px', fontSize: '13px', lineHeight: 1.5 },
        statusRow: { display: 'flex', alignItems: 'center', gap: '8px', fontSize: '13px', flexWrap: 'wrap' },
        field: { display: 'flex', flexDirection: 'column', gap: '4px' },
        label: { fontSize: '12px', opacity: 0.85 },
        input: {
          background: 'transparent',
          border: '1px solid rgba(127,127,127,.45)',
          borderRadius: '6px',
          padding: '6px 10px',
          fontSize: '13px',
          color: 'inherit',
          outline: 'none',
          width: '100%',
          boxSizing: 'border-box',
        },
        check: { display: 'flex', alignItems: 'center', gap: '8px' },
        row: { display: 'flex', flexWrap: 'wrap', gap: '8px' },
        btn: {
          background: 'transparent',
          border: '1px solid rgba(127,127,127,.45)',
          borderRadius: '6px',
          padding: '6px 14px',
          fontSize: '13px',
          color: 'inherit',
          cursor: 'pointer',
        },
        primary: {
          background: 'transparent',
          border: '1px solid #4f8cff',
          borderRadius: '6px',
          padding: '6px 14px',
          fontSize: '13px',
          color: '#4f8cff',
          cursor: 'pointer',
        },
        danger: {
          background: 'transparent',
          border: '1px solid rgba(255,107,107,.65)',
          borderRadius: '6px',
          padding: '6px 14px',
          fontSize: '13px',
          color: '#ff6b6b',
          cursor: 'pointer',
        },
        error: { color: '#ff6b6b', fontSize: '12px' },
        hint: { fontSize: '12px', opacity: 0.7 },
      };

      var PHASE_TEXT = {
        stopped: '已停止',
        starting: '启动中',
        running: '运行中',
        external: '运行中（外部）',
        error: '异常',
      };

      // npm 镜像源快捷切换预设。
      var REGISTRY_PRESETS = [
        { key: 'npm', label: 'npm 官方源', url: 'https://registry.npmjs.org' },
        { key: 'npmmirror', label: 'npmmirror（国内加速）', url: 'https://registry.npmmirror.com' },
      ];
      // DSH 版本偏好预设：对应 npm 发布标签，检查更新与更新安装都按所选标签查询。
      var CHANNEL_PRESETS = [
        { key: 'latest', label: '稳定版（latest）' },
        { key: 'next', label: '预发布版（next）' },
      ];
      var presetStyle = {
        background: 'transparent',
        border: '1px solid rgba(127,127,127,.45)',
        borderRadius: '6px',
        padding: '4px 10px',
        fontSize: '12px',
        color: 'inherit',
        cursor: 'pointer',
      };
      var presetActiveStyle = Object.assign({}, presetStyle, { borderColor: '#4f8cff', color: '#4f8cff' });
      // 目录行：输入框占满剩余宽度，选择按钮不收缩不换行。
      var pickRowStyle = { display: 'flex', gap: '8px', alignItems: 'center' };
      var pickInputStyle = Object.assign({}, styles.input, { flex: 1, minWidth: 0 });
      var pickButtonStyle = Object.assign({}, presetStyle, { flexShrink: 0, whiteSpace: 'nowrap' });

      // 鲸仔应用 logo（64x64 PNG 的 data URI，由打包时注入；展示 32px 即 2x 密度）。
      var APP_ICON_SRC = 'data:image/png;base64,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';

      function headerBlock() {
        return h('div', { key: 'whalito-head', style: { display: 'flex', alignItems: 'center', gap: '10px' } }, [
          h('img', {
            key: 'app-icon',
            src: APP_ICON_SRC,
            width: 32, height: 32,
            style: { borderRadius: '8px', display: 'block' },
            alt: '鲸仔',
          }),
          h('span', { key: 'whalito-sub', style: styles.hint }, '鲸仔（Whalito）桌面端设置'),
        ]);
      }

      function cloneSettings(s) {
        return s ? Object.assign({}, s) : null;
      }

      function WhalitoSection() {
        var connected = React.useState(false);
        var settings = React.useState(null);
        var status = React.useState(null);
        var notice = React.useState('');
        var dirty = React.useState(false);
        var draft = React.useState(null);
        var tries = React.useState(0);
        var versions = React.useState(null);
        var checking = React.useState('');
        var updating = React.useState('');
        var updateStage = React.useState('');
        // 消息监听器挂在只跑一次的 useEffect 里，闭包会捕获挂载时的 state 数组
        // （值恒为初始值）；用 ref 在每次渲染时刷新最新快照供监听器读取。
        var draftRef = React.useRef(null);
        draftRef.current = draft[0];
        var settingsRef = React.useRef(null);
        settingsRef.current = settings[0];
        var dirtyRef = React.useRef(false);
        dirtyRef.current = dirty[0];

        React.useEffect(function () {
          var done = false;
          function onMessage(event) {
            // channel 校验即可：消息来自本页所在 iframe 的父窗口，
            // 而父窗口只会向自己的 iframe 发送（事件源校验在 WebView2
            // 各版本间不稳定，不再强依赖）。
            var data = event.data;
            if (!data || data.channel !== CHANNEL) return;
            if (data.type === 'hello' || data.type === 'settings') {
              if (data.settings) {
                settings[1](data.settings);
                if (!dirtyRef.current) draft[1](cloneSettings(data.settings));
              }
              if (data.status) status[1](data.status);
              if (data.versions) versions[1](data.versions);
              connected[1](true);
              checking[1]('');
              updating[1]('');
              done = true;
            } else if (data.type === 'status') {
              if (data.status) status[1](data.status);
              connected[1](true);
              done = true;
            } else if (data.type === 'versions') {
              if (data.versions) versions[1](data.versions);
              checking[1]('');
              updating[1]('');
            } else if (data.type === 'picked-dir') {
              // 原生目录选择结果：写入草稿并标记脏（即使此前已编辑过）。
              if (typeof data.path === 'string' && data.field === 'workspaceDir') {
                var wsNext = Object.assign({}, draftRef.current || settingsRef.current || {});
                wsNext.workspaceDir = data.path;
                draft[1](wsNext);
                dirty[1](true);
              } else if (typeof data.path === 'string' && data.field === 'downloadDir') {
                var dlNext = Object.assign({}, draftRef.current || settingsRef.current || {});
                dlNext.downloadDir = data.path;
                draft[1](dlNext);
                dirty[1](true);
              }
            } else if (data.type === 'update-progress') {
              updateStage[1](typeof data.message === 'string' ? data.message : '');
            } else if (data.type === 'error') {
              // 失败时复位所有进行中状态：检查/更新按钮不再卡在「检查中…」「更新中…」，
              // 进度文案不再残留「正在下载更新…」之类的假更新提示（鲸仔自更新下载失败后
              // 设置分区仍显示「正在更新」的根因——此前只清了 checking，漏了 updating /
              // updateStage，而父窗口失败路径不会补发 hello 快照来复位）。
              notice[1](typeof data.message === 'string' ? data.message : '鲸仔操作失败');
              checking[1]('');
              updating[1]('');
              updateStage[1]('');
            }
          }
          window.addEventListener('message', onMessage);
          sendPing();
          // 握手可能因时序/丢消息失败：未连接期间每 2 秒重试。
          var pingTimer = window.setInterval(function () {
            if (done) return;
            sendPing();
            tries[1](function (n) { return n + 1; });
          }, 2000);
          return function () {
            window.clearInterval(pingTimer);
            window.removeEventListener('message', onMessage);
          };
        }, []);

        function updateDraft(key, value) {
          var next = Object.assign({}, draft[0] || {});
          next[key] = value;
          draft[1](next);
          dirty[1](true);
        }

        // 请求鲸仔主窗口弹原生目录选择器，结果经 picked-dir 消息回填草稿。
        function pickDir(field) {
          sendAction('pick-directory', { field: field });
        }

        function buildValue(base) {
          return {
            port: Number(base.port),
            registry: base.registry == null ? '' : base.registry,
            dshChannel: base.dshChannel === 'next' ? 'next' : 'latest',
            autostart: !!base.autostart,
            autoRestart: !!base.autoRestart,
            workspaceDir: base.workspaceDir == null || base.workspaceDir === '' ? null : base.workspaceDir,
            nodeDir: base.nodeDir == null || base.nodeDir === '' ? null : base.nodeDir,
            downloadDir: base.downloadDir == null || base.downloadDir === '' ? null : base.downloadDir,
            petEnabled: !!base.petEnabled,
          };
        }

        function save() {
          var d = draft[0];
          if (!d) return;
          var value = buildValue(d);
          if (!Number.isInteger(value.port) || value.port < 1 || value.port > 65535) {
            notice[1]('端口必须是 1–65535 之间的整数');
            return;
          }
          notice[1]('');
          sendAction('save-settings', { value: value });
        }

        // 镜像源快捷切换：写入 draft 并立即保存；端口非法时仅提示不保存。
        function switchRegistry(url) {
          var base = draft[0] || settings[0] || {};
          var next = Object.assign({}, base, { registry: url });
          draft[1](next);
          dirty[1](true);
          var value = buildValue(next);
          if (!Number.isInteger(value.port) || value.port < 1 || value.port > 65535) {
            notice[1]('端口必须是 1–65535 之间的整数，请修正后再切换镜像源');
            return;
          }
          notice[1]('');
          sendAction('save-settings', { value: value });
        }

        // 版本偏好快捷切换：点击即保存生效（与镜像源预设一致），
        // 主窗口保存后会自动按新偏好重查版本。
        function switchChannel(key) {
          var base = draft[0] || settings[0] || {};
          var next = Object.assign({}, base, { dshChannel: key });
          draft[1](next);
          dirty[1](true);
          var value = buildValue(next);
          if (!Number.isInteger(value.port) || value.port < 1 || value.port > 65535) {
            notice[1]('端口必须是 1–65535 之间的整数，请修正后再切换版本偏好');
            return;
          }
          notice[1]('');
          sendAction('save-settings', { value: value });
        }

        function checkButton(target) {
          var busy = checking[0] === target;
          return h('button', {
            key: target + '-check',
            type: 'button',
            style: presetStyle,
            disabled: busy,
            onClick: function () {
              checking[1](target);
              sendAction('check-update', { target: target });
            },
          }, busy ? '检查中…' : '检查更新');
        }

        function resultText(info, isWhalito, isTestBuild, channelLabel) {
          if (!info || !info.latest) return null;
          if (!info.updateAvailable) {
            // DSH 行附通道标注（鲸仔行不标注）：区分"按哪个通道查的"，
            // 避免 latest/next 指向同一版本时误以为切换没生效。
            return h('span', { key: 'result', style: styles.hint },
              '已是最新（' + info.latest + (isWhalito ? '' : '，' + channelLabel) + '）');
          }
          return h('span', { key: 'result', style: { fontSize: '12px', display: 'inline-flex', alignItems: 'center', gap: '8px', flexWrap: 'wrap' } }, [
            h('span', { key: 'result-text' },
              '发现新版本 ' + info.latest + (isWhalito ? '' : '（' + channelLabel + '）')),
            isWhalito && info.autoUpdate
              ? h('button', {
                  key: 'apply-update',
                  type: 'button',
                  style: presetActiveStyle,
                  onClick: function () {
                    // window.confirm 在 WebView2 中不可用：默认脚本对话框只支持
                    // alert，confirm 静默返回 false。确认由 Rust 侧原生对话框完成
                    // （whalito_apply_update 先弹确认，取消则静默结束）。
                    sendAction('apply-update');
                  },
                }, '立即更新')
              : null,
            isWhalito && info.url
              ? h('button', {
                  key: 'open-download',
                  type: 'button',
                  style: presetStyle,
                  onClick: function () { sendAction('open-url', { url: info.url }); },
                }, '打开下载页')
              : null,
            isWhalito
              ? (info.autoUpdate
                  ? null
                  : h('span', { key: 'no-auto-hint', style: styles.hint },
                      isTestBuild ? '（测试版不提供自动更新）' : '（该版本没有自动更新安装包）'))
              : h('button', {
                  key: 'update-dsh',
                  type: 'button',
                  style: presetActiveStyle,
                  disabled: updating[0] === 'dsh',
                  onClick: function () {
                    updating[1]('dsh');
                    sendAction('update-dsh');
                  },
                }, updating[0] === 'dsh' ? '更新中…' : '立即更新'),
          ]);
        }

        function versionBlock() {
          var v = versions[0];
          var vd = v && v.dsh ? v.dsh : null;
          var vw = v && v.whalito ? v.whalito : null;
          return h('div', {
            key: 'versions',
            style: { display: 'flex', flexDirection: 'column', gap: '8px', borderTop: '1px solid rgba(127,127,127,.25)', paddingTop: '12px' },
          }, [
            h('div', { key: 'versions-title', style: { fontWeight: 600, fontSize: '13px' } }, '版本信息'),
            vd && vd.current
              ? h('div', { key: 'row-dsh-channel', style: styles.statusRow }, [
                  h('span', { key: 'dsh-channel-label', style: styles.hint }, 'DSH 版本偏好：'),
                  CHANNEL_PRESETS.map(function (c) {
                    var active = (d.dshChannel === 'next' ? 'next' : 'latest') === c.key;
                    return h('button', {
                      key: 'dsh-channel-' + c.key,
                      type: 'button',
                      style: active ? presetActiveStyle : presetStyle,
                      onClick: function () { switchChannel(c.key); },
                    }, c.label);
                  }),
                  h('span', { key: 'dsh-channel-hint', style: styles.hint }, '选择 DSH 的更新来源，点击即保存并重新检查'),
                ])
              : null,
            h('div', { key: 'row-dsh', style: styles.statusRow }, [
              h('span', { key: 'dsh-label', style: { fontWeight: 600 } }, 'DSH：'),
              h('span', { key: 'dsh-current' }, vd && vd.current ? vd.current : '未安装'),
              checkButton('dsh'),
              resultText(vd, false, false, d.dshChannel === 'next' ? '预发布版' : '稳定版'),
            ]),
            h('div', { key: 'row-whalito', style: styles.statusRow }, [
              h('span', { key: 'whalito-label', style: { fontWeight: 600 } }, '鲸仔：'),
              h('span', { key: 'whalito-current' },
                (vw && vw.current ? vw.current : '未知') + (vw && vw.testBuild ? '（测试版）' : '')),
              checkButton('whalito'),
              resultText(vw, true, !!(vw && vw.testBuild)),
              h('button', {
                key: 'github',
                type: 'button',
                style: presetStyle,
                onClick: function () {
                  sendAction('open-url', { url: 'https://github.com/entireyu/dsh-whalito-desk' });
                },
              }, 'GitHub'),
            ]),
            updateStage[0]
              ? h('div', { key: 'update-stage', style: styles.statusRow },
                  h('span', { key: 'update-stage-text', style: styles.hint }, updateStage[0]))
              : null,
          ]);
        }

        if (!connected[0]) {
          return h('div', { style: styles.root }, [
            headerBlock(),
            h('div', { key: 'connecting', style: styles.hint },
              tries[0] > 0
                ? '正在连接鲸仔…（已尝试 ' + tries[0] + ' 次，请确认正在鲸仔内嵌页中打开）'
                : '正在连接鲸仔…'),
          ]);
        }

        var st = status[0];
        var d = draft[0] || settings[0] || {};
        var phase = st ? st.phase : null;
        var serverRunning = phase === 'running' || phase === 'external';

        return h('div', { style: styles.root }, [
          headerBlock(),
          h('div', { key: 'status', style: styles.statusRow }, [
            h('span', { key: 'status-label', style: { fontWeight: 600 } }, '服务器：'),
            h('span', { key: 'status-phase' }, st ? (PHASE_TEXT[phase] || phase) : '未知'),
            st && st.url ? h('span', { key: 'status-url', style: styles.hint }, st.url) : null,
          ]),
          h('div', { key: 'actions', style: styles.row }, [
            // 服务器运行中不显示启动按钮；停止/重启仅在运行时显示。
            serverRunning ? null : h('button', {
              key: 'start',
              style: styles.primary,
              onClick: function () { sendAction('start'); },
            }, '启动服务器'),
            serverRunning ? h('button', {
              key: 'stop',
              style: styles.danger,
              onClick: function () { sendAction('stop'); },
            }, '停止服务器') : null,
            serverRunning ? h('button', {
              key: 'restart',
              style: styles.btn,
              onClick: function () { sendAction('restart'); },
            }, '重启服务器') : null,
            h('button', {
              key: 'focus-panel',
              style: styles.btn,
              onClick: function () { sendAction('focus-panel'); },
            }, '进入鲸仔管理后台'),
          ]),
          h('label', { key: 'port', style: styles.field }, [
            h('span', { key: 'port-label', style: styles.label }, '端口'),
            h('input', {
              key: 'port-input',
              type: 'number', min: 1, max: 65535, style: styles.input,
              value: d.port == null ? '' : d.port,
              onInput: function (e) { updateDraft('port', e.target.value); },
            }),
          ]),
          h('div', { key: 'registry', style: styles.field }, [
            h('span', { key: 'registry-label', style: styles.label }, 'npm 镜像源'),
            h('input', {
              key: 'registry-input',
              type: 'text', style: styles.input,
              placeholder: 'https://registry.npmjs.org',
              value: d.registry == null ? '' : d.registry,
              onInput: function (e) { updateDraft('registry', e.target.value); },
            }),
            h('div', { key: 'registry-presets', style: styles.row }, [
              REGISTRY_PRESETS.map(function (p) {
                return h('button', {
                  key: p.key,
                  type: 'button',
                  style: d.registry === p.url ? presetActiveStyle : presetStyle,
                  onClick: function () { switchRegistry(p.url); },
                }, p.label);
              }),
            ]),
          ]),
          h('label', { key: 'workspace', style: styles.field }, [
            h('span', { key: 'workspace-label', style: styles.label }, '工作目录（可选）'),
            h('div', { key: 'workspace-row', style: pickRowStyle }, [
              h('input', {
                key: 'workspace-input',
                type: 'text', style: pickInputStyle,
                placeholder: '留空使用默认目录',
                value: d.workspaceDir == null ? '' : d.workspaceDir,
                onInput: function (e) { updateDraft('workspaceDir', e.target.value); },
              }),
              h('button', {
                key: 'workspace-pick',
                type: 'button',
                style: pickButtonStyle,
                onClick: function () { pickDir('workspaceDir'); },
              }, '选择…'),
            ]),
            h('span', { key: 'workspace-hint', style: styles.hint },
              'DSH 服务器的工作目录，会话里终端等相对路径以此为基准；留空使用默认目录。'),
          ]),
          d.nodeDir
            ? h('div', { key: 'node-dir-info', style: styles.statusRow }, [
                h('span', { key: 'node-dir-info-label', style: { fontWeight: 600 } }, 'Node 安装目录：'),
                h('span', { key: 'node-dir-info-path', style: styles.hint },
                  d.nodeDir + '（鲸仔自动检测或安装时写入）'),
              ])
            : null,
          h('label', { key: 'download-dir', style: styles.field }, [
            h('span', { key: 'download-dir-label', style: styles.label }, '下载目录（可选）'),
            h('div', { key: 'download-dir-row', style: pickRowStyle }, [
              h('input', {
                key: 'download-dir-input',
                type: 'text', style: pickInputStyle,
                placeholder: '留空使用系统下载目录',
                value: d.downloadDir == null ? '' : d.downloadDir,
                onInput: function (e) { updateDraft('downloadDir', e.target.value); },
              }),
              h('button', {
                key: 'download-dir-pick',
                type: 'button',
                style: pickButtonStyle,
                onClick: function () { pickDir('downloadDir'); },
              }, '选择…'),
            ]),
            h('span', { key: 'download-dir-hint', style: styles.hint },
              '会话日志等下载的保存位置；留空使用系统下载目录。'),
          ]),
          h('label', { key: 'autostart', style: styles.check }, [
            h('input', {
              key: 'autostart-input',
              type: 'checkbox',
              checked: !!d.autostart,
              onChange: function (e) { updateDraft('autostart', e.target.checked); },
            }),
            h('span', { key: 'autostart-label' }, '开机自启本程序'),
          ]),
          h('label', { key: 'auto-restart', style: styles.check }, [
            h('input', {
              key: 'auto-restart-input',
              type: 'checkbox',
              checked: !!d.autoRestart,
              onChange: function (e) { updateDraft('autoRestart', e.target.checked); },
            }),
            h('span', { key: 'auto-restart-label' }, '服务器异常退出后自动重启'),
          ]),
          h('label', { key: 'pet', style: styles.check }, [
            h('input', {
              key: 'pet-input',
              type: 'checkbox',
              checked: !!d.petEnabled,
              onChange: function (e) { updateDraft('petEnabled', e.target.checked); },
            }),
            h('span', { key: 'pet-label' }, '显示桌宠'),
          ]),
          h('div', { key: 'save-row', style: styles.row },
            h('button', { key: 'save', style: styles.primary, onClick: save }, '保存设置')),
          versionBlock(),
          notice[0] ? h('div', { key: 'notice', style: styles.error }, notice[0]) : null,
          h('div', { key: 'hint', style: styles.hint }, '端口变更会在保存后自动重启服务器生效。'),
        ]);
      }

      exports.inject = ['slots'];

      exports.apply = function (ctx) {
        // 普通浏览器打开（非鲸仔内嵌）不注册分区。
        if (window.parent === window) return;
        ctx.slots.inject('settings.section', function () {
          return ctx.slots.register({
            name: 'settings.section',
            id: 'whalito',
            order: 25,
            label: '鲸仔设置',
          }, WhalitoSection);
        });
      };

      return module.exports;
    },
  });
})();
