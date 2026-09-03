//! 把 dsh 会话 cookie 注入主窗口 WebView2 的 cookie 仓库（仅 Windows）。
//!
//! ## 为什么需要注入
//!
//! 鲸仔主窗口运行在 `http://tauri.localhost`，用**跨站 iframe** 内嵌
//! DSH（`http://127.0.0.1:<port>`）。DSH ≥ 0.1.2 的信任握手在 303 应答里
//! 签发 `SameSite=Strict` 的会话 cookie（`dsh-auth-<authority>`）：
//!
//! - 原生 HTTP/WS 客户端（Rust 侧）没有 SameSite 概念，Rust 握手拿到
//!   cookie 后一切正常（桌宠轮询、审批等）；
//! - 但浏览器里，跨站 iframe 上下文既**保存不了**（第三方 Set-Cookie 被
//!   丢弃）也**带不上**（Strict 只发给同站请求）这条 cookie —— 于是 iframe
//!   落到 `/` 时未认证 → 401 白页（鲸仔「打开直接白屏」的根因）。
//!
//! 修法：Rust 握手成功后，把同一 cookie 以 `SameSite=None; Secure` 重新
//! 写入 WebView2 cookie 仓库（`AddOrUpdateCookie` 同名覆盖）。此后 iframe
//! 的每个请求都能带上会话，页面正常渲染。
//!
//! cookie 名由 DSH 按 authority（host:port）派生，同端口每次重启同名，
//! 注入天然覆盖旧值，不会残留多个会话 cookie。
//!
//! macOS（WKWebView）的等价注入尚未实现：跨站 iframe 会命中同样的
//! SameSite 限制，属待办项（见 `inject` 的 cfg 门控）。

use tauri::{AppHandle, Manager};

/// cookie 应注入的窗口（主窗口持有内嵌 DSH 的 iframe）。
#[cfg(windows)]
const TARGET_WINDOW: &str = "main";

/// WebView2 内 cookie 的归属域 / 路径：与 dsh 启动参数一致（只服务
/// 本机 127.0.0.1）。域 cookie 与 host-only cookie 对 127.0.0.1 的匹配
/// 行为一致，这里显式给域即可。
#[cfg(windows)]
const COOKIE_DOMAIN: &str = "127.0.0.1";
#[cfg(windows)]
const COOKIE_PATH: &str = "/";

/// 尝试把 `name=value` 会话 cookie 注入主窗口 WebView2（Windows）。
/// 失败静默（cookie 缺失时原生客户端仍可用；白屏场景在 Windows 上
/// 注入后由 `dsh-auth-ready` 事件驱动前端重建 iframe 恢复）。
///
/// COM 调用必须发生在创建 WebView2 的线程（主 UI 线程），因此这里只
/// 投递任务，不等待结果；调用方随后再发 `dsh-auth-ready` 事件让前端
/// 决定是否重建 iframe，保证注入先于重载执行。
pub fn inject(app: &AppHandle, pair: &str) {
    #[cfg(windows)]
    inject_windows(app, pair);
    #[cfg(not(windows))]
    let _ = (app, pair); // macOS/WKWebView 注入待实现（见模块注释）
}

/// 在 WebView2 主窗口线程上执行真正的 COM 注入。
#[cfg(windows)]
fn inject_windows(app: &AppHandle, pair: &str) {
    let Some((name, value)) = pair.split_once('=') else {
        return;
    };
    if name.is_empty() || value.is_empty() {
        return;
    }
    let Some(window) = app.get_webview_window(TARGET_WINDOW) else {
        return;
    };
    let (name, value) = (name.to_string(), value.to_string());
    let window_for_main = window.clone();
    let _ = window.run_on_main_thread(move || {
        window_for_main.with_webview(move |platform| {
            inject_into_controller(&platform, &name, &value);
        });
    });
}

/// 走 ICoreWebView2Controller → ICoreWebView2_2 → CookieManager 链路写入
/// cookie（CookieManager 挂在 ICoreWebView2_2 上，需先 cast）。
#[cfg(windows)]
fn inject_into_controller(
    platform: &tauri::webview::PlatformWebview,
    name: &str,
    value: &str,
) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_COOKIE_SAME_SITE_KIND_NONE, ICoreWebView2_2,
    };
    use windows_core::{HSTRING, Interface as _};

    let controller = platform.controller();
    let Ok(core) = (unsafe { controller.CoreWebView2() }) else {
        return;
    };
    let Ok(core2) = core.cast::<ICoreWebView2_2>() else {
        return;
    };
    // COM 调用均为 unsafe（windows-rs 绑定）；整个写入过程放在同一 unsafe 块。
    let Ok(manager) = (unsafe { core2.CookieManager() }) else {
        return;
    };
    let name_h = HSTRING::from(name);
    let value_h = HSTRING::from(value);
    let domain_h = HSTRING::from(COOKIE_DOMAIN);
    let path_h = HSTRING::from(COOKIE_PATH);
    let cookie = match unsafe { manager.CreateCookie(&name_h, &value_h, &domain_h, &path_h) } {
        Ok(cookie) => cookie,
        Err(_) => return,
    };
    // SameSite=None 需要 Secure（http 环回地址上 Chromium 同样放行）。
    unsafe {
        let _ = cookie.SetIsSecure(true);
        let _ = cookie.SetSameSite(COREWEBVIEW2_COOKIE_SAME_SITE_KIND_NONE);
        let _ = manager.AddOrUpdateCookie(&cookie);
    }
}

