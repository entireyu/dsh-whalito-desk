//! 桌宠（Whalito Desktop Pet）：读取 Harness 会话状态并在需要用户确认时提醒。
//!
//! 通过 Harness 暴露的 `/api` JSON-RPC 风格 HTTP 接口 + `/api/events.mux` /
//! `/api/events.host` 两条 WebSocket 下行流，把“正在运行的任务 / 目标 / 待办”
//! 与“待处理的审批（approval）与提问（question）”投影到 pet 窗口。
//!
//! 网络路径全部走本机 loopback（`127.0.0.1`），满足 Harness 的 `/api` 信任栅栏，
//! 因此无需鉴权，也绕开了 Tauri WebView 的 Origin/CORS 限制。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::state::{self, AppState};

/// 轮询间隔（秒）。
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// 生成一个本地唯一的请求关联 id（无需 UUID 依赖；只需在进程内唯一且宿主原样回显）。
fn next_rpc_id() -> String {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("pet-{}-{}-{}", std::process::id(), ts, n)
}

/// 单条会话的摘要（映射 `session.list` 的 `SessionSummary`）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PetSessionInfo {
    pub session_id: String,
    pub running: bool,
    pub blank: bool,
    pub title: Option<String>,
    /// `projections.values.goal`（当前目标，可能为 null）。
    pub goal: Option<Value>,
    /// `projections.values.todos`（待办列表，可能为 null）。
    pub todos: Option<Value>,
    /// `origin == "subagent"`：后台子代理会话。
    pub is_subagent: bool,
}

/// 桌宠状态快照（每 2s 推送一次）。
/// phase: running / stopped（服务器未运行）/ error（API 不可达，见 error 字段）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PetState {
    pub phase: String,
    pub sessions: Vec<PetSessionInfo>,
    pub running_count: usize,
    pub subagent_count: usize,
    pub error: Option<String>,
}

impl PetState {
    fn stopped() -> Self {
        Self {
            phase: "stopped".to_string(),
            sessions: Vec::new(),
            running_count: 0,
            subagent_count: 0,
            error: None,
        }
    }

    fn failed(reason: String) -> Self {
        Self {
            phase: "error".to_string(),
            sessions: Vec::new(),
            running_count: 0,
            subagent_count: 0,
            error: Some(reason),
        }
    }
}

/// 桌宠待查看通知：任务完成 / 被阻塞 / 被中断 / 失败。
/// 设置后持续展示，直到用户打开主界面（show_main → clear_notice）才清除。
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PetNotice {
    /// completed / blocked / interrupted / failed。
    pub kind: String,
    /// 关联会话的标题（projections.title），可能为空。
    pub title: Option<String>,
    /// 关联会话的目标（projections.goal.objective），可能为空。
    pub goal: Option<String>,
}

/// 运行中的主会话集合：session_id → (标题, 目标文本, 目标是否 blocked)。
/// 排除空白会话与子代理（与 Pet.vue 的 runningSessions 过滤一致）。
type RunningMap = HashMap<String, (Option<String>, Option<String>, bool)>;

/// 从会话 projections 的 goal 值中取出 objective 文本。
fn goal_objective(goal: &Value) -> Option<String> {
    goal.get("objective")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

/// 计算一次轮询后的完成类通知：上一轮有运行中的主会话、这一轮全部结束
/// （结束 / 被阻塞 / 从列表消失）。任一结束会话的目标为 blocked 时报告
/// "blocked"，否则 "completed"；文案取最有信息量的会话（有目标者优先）。
fn detect_completion(prev: &RunningMap, current: &RunningMap) -> Option<PetNotice> {
    if prev.is_empty() || !current.is_empty() {
        return None;
    }
    let blocked = prev.values().any(|v| v.2);
    let best = prev
        .values()
        .find(|v| v.1.is_some())
        .or_else(|| prev.values().find(|v| v.0.is_some()))
        .or_else(|| prev.values().next());
    let (title, goal, _) = best?;
    Some(PetNotice {
        kind: if blocked { "blocked" } else { "completed" }.to_string(),
        title: title.clone(),
        goal: goal.clone(),
    })
}

/// 服务器不可达（stopped 阶段）时上一阶段仍有任务在跑 → 任务被中断。
/// running / error 阶段都可能仍有在跑的任务（error 仅表示 session.list 拉取失败）。
fn detect_interruption(last_phase: Option<&str>, had_running: bool) -> Option<PetNotice> {
    if matches!(last_phase, Some("running" | "error")) && had_running {
        Some(PetNotice {
            kind: "interrupted".to_string(),
            title: None,
            goal: None,
        })
    } else {
        None
    }
}

/// 解析当前服务器 URL：健康优先的兜底链——
/// 记录中的 server_url（健康）→ 配置端口探测（健康，覆盖外部实例/端口变更/
/// url 未提取成功的情况）→ None。
fn current_base(st: &AppState) -> Option<String> {
    let port = st.settings.lock().unwrap().port;
    let recorded = st.server_url.lock().unwrap().clone();
    if let Some(url) = recorded.as_deref() {
        if state::health(url) {
            // 记录值可能是带 ?token= 的嵌入地址；原生客户端拼接 /api 路径
            // 需要干净的基地址（token 只用于握手，cookie 单独携带）。
            return Some(state::clean_url(url));
        }
    }
    let probe = format!("http://127.0.0.1:{port}");
    if state::health(&probe) {
        return Some(probe);
    }
    None
}

/// 桌宠诊断日志：写 %TEMP%\whalito-pet.log（排障直接读文件）。
fn pet_log(line: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("whalito-pet.log");
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{secs}] {line}");
    }
}

/// 把 http(s) 基地址转成 ws(s) 基地址。
fn ws_base(base: &str) -> String {
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{base}")
    }
}

/// 向 Harness 发送一次 unary RPC（POST 指定的 `/api` 路径），返回解析后的响应 JSON。
/// DSH ≥ 0.1.2 的 /api 需要会话 cookie，握手成功后由调用方随 base 一并传入。
fn rpc_call(
    base: &str,
    cookie: Option<&str>,
    method: &str,
    url: &str,
    payload: Value,
) -> Result<Value, String> {
    let body = json!({
        "type": "client-request",
        "rpcId": next_rpc_id(),
        "method": method,
        "payload": payload,
    });
    let mut request = ureq::post(url).set("Content-Type", "application/json");
    if let Some(cookie) = cookie {
        request = request.set("Cookie", cookie);
    }
    let resp = request
        .send_string(&body.to_string())
        .map_err(|e| format!("{method}: {e}"))?;
    let text = resp.into_string().map_err(|e| e.to_string())?;
    serde_json::from_str::<Value>(&text).map_err(|e| format!("{method}: 解析响应失败 {e}"))
}

/// 拉取一次 `session.list` 并折叠为 `PetState`。
///
/// DSH 0.1.2 把 unary RPC 改为斜杠方法名并引入 typert 网关：路径 /api/session/list、
/// 信封 payload 必须是 `{args:{_request:{}}}`，且全程要求浏览器会话 cookie；
/// 0.1.1 及更早用点号名（/api/session.list）+ 空 payload、无认证。有无会话
/// cookie 恰好区分两者（0.1.2+ 必带 cookie，旧版没有认证概念），据此选择协议。
fn poll_state(base: &str, cookie: Option<&str>) -> Result<PetState, String> {
    let (method, url, payload) = if cookie.is_some() {
        (
            "session/list",
            format!("{base}/api/session/list"),
            json!({ "args": { "_request": {} } }),
        )
    } else {
        (
            "session.list",
            format!("{base}/api/session.list"),
            json!({}),
        )
    };
    let v = rpc_call(base, cookie, method, &url, payload)?;
    let result = v.get("result").ok_or(format!("{method}: 缺少 result"))?;
    if result.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        return Err(format!("{method}: ok=false"));
    }
    let items = result
        .get("value")
        .and_then(|x| x.get("items"))
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();

    let mut sessions = Vec::with_capacity(items.len());
    for item in items {
        let projections = item
            .get("projections")
            .and_then(|p| p.get("values"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        sessions.push(PetSessionInfo {
            session_id: item
                .get("sessionId")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            running: item.get("running").and_then(|x| x.as_bool()).unwrap_or(false),
            blank: item.get("blank").and_then(|x| x.as_bool()).unwrap_or(false),
            title: projections
                .get("title")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            goal: projections.get("goal").cloned(),
            todos: projections.get("todos").cloned(),
            is_subagent: item.get("origin").and_then(|x| x.as_str()) == Some("subagent"),
        });
    }

    let running_count = sessions.iter().filter(|s| s.running).count();
    let subagent_count = sessions.iter().filter(|s| s.running && s.is_subagent).count();
    Ok(PetState {
        phase: "running".to_string(),
        sessions,
        running_count,
        subagent_count,
        error: None,
    })
}

/// 推送一次状态快照到 pet 窗口，并缓存供 `pet_status` 即时返回。
/// 快照附带当前待查看通知（AppState.pet_notice）。
fn emit_state(app: &AppHandle, state: PetState) {
    if let Ok(mut snapshot) = serde_json::to_value(&state) {
        let st = app.state::<AppState>();
        let notice = st.pet_notice.lock().unwrap().clone();
        if let Ok(nv) = serde_json::to_value(notice) {
            snapshot["notice"] = nv;
        }
        *st.pet_snapshot.lock().unwrap() = Some(snapshot.clone());
        let _ = app.emit("pet-state", snapshot);
    }
}

/// 设置待查看通知（覆盖旧通知）；下一次 emit_state 会随快照广播到 pet 窗口。
pub fn set_notice(app: &AppHandle, notice: PetNotice) {
    let st = app.state::<AppState>();
    *st.pet_notice.lock().unwrap() = Some(notice);
}

/// 用户打开主界面后清除待查看通知，并立即重发一份去掉 notice 的快照，
/// 让 pet 马上回到"空闲中"，无需等待下一次轮询。
pub fn clear_notice(app: &AppHandle) {
    let st = app.state::<AppState>();
    *st.pet_notice.lock().unwrap() = None;
    let snapshot = st.pet_snapshot.lock().unwrap().clone();
    if let Some(mut s) = snapshot {
        s["notice"] = Value::Null;
        *st.pet_snapshot.lock().unwrap() = Some(s.clone());
        let _ = app.emit("pet-state", s);
    }
}

/// 处理一条 mux/host 流帧：审批 / 提问 → 告警；其余忽略（状态由轮询覆盖）。
fn handle_stream_frame(app: &AppHandle, text: &str) {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let rpc_id = v
        .get("rpcId")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let Some(payload) = v.get("payload") else {
        return;
    };
    let Some(ptype) = payload.get("type").and_then(|x| x.as_str()) else {
        return;
    };

    match ptype {
        "approval/requested" => {
            let key = payload
                .get("approvalId")
                .and_then(|x| x.as_str())
                .unwrap_or(&rpc_id)
                .to_string();
            let alert = json!({
                "kind": "approval",
                "key": key,
                "rpcId": rpc_id,
                "sessionId": payload.get("sessionId"),
                "approvalId": payload.get("approvalId"),
                "toolName": payload.get("toolName"),
                "reason": payload.get("reason"),
            });
            let _ = app.emit("pet-alert", &alert);
        }
        "question/requested" => {
            let alert = json!({
                "kind": "question",
                "key": rpc_id,
                "rpcId": rpc_id,
                "sessionId": payload.get("sessionId"),
                "questions": payload.get("questions"),
            });
            let _ = app.emit("pet-alert", &alert);
        }
        "approval/resolved" => {
            let key = payload
                .get("approvalId")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if !key.is_empty() {
                let _ = app.emit("pet-alert-clear", &key);
            }
        }
        "question/resolved" => {
            let key = payload
                .get("questionRpcId")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if !key.is_empty() {
                let _ = app.emit("pet-alert-clear", &key);
            }
        }
        // 会话事件透传（mux 流 `session/event`）：turn/end 携带本轮结束原因。
        // 模型调用失败（重试耗尽等 reason=error）→ 立即发「任务失败」通知，
        // 并记录失败会话，轮询检测完成时不再误报「已完成」。
        "session/event" => {
            let session_id = payload
                .get("sessionId")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let event = payload.get("event");
            if event
                .and_then(|e| e.get("type"))
                .and_then(|t| t.as_str())
                == Some("turn/end")
            {
                let reason = event
                    .and_then(|e| e.get("data"))
                    .and_then(|d| d.get("reason"))
                    .and_then(|r| r.get("kind"))
                    .and_then(|k| k.as_str());
                match reason {
                    Some("error") => {
                        app.state::<AppState>()
                            .pet_failed_sessions
                            .lock()
                            .unwrap()
                            .insert(session_id.clone());
                        let (title, goal) = session_title_goal(app, &session_id);
                        let notice = PetNotice {
                            kind: "failed".to_string(),
                            title,
                            goal,
                        };
                        pet_log("pet notice: failed (turn/end error)");
                        set_notice(app, notice);
                    }
                    Some("completed") | Some("blocked") => {
                        // 正常结束：清除该会话的失败标记（若有），完成通知交给轮询。
                        app.state::<AppState>()
                            .pet_failed_sessions
                            .lock()
                            .unwrap()
                            .remove(&session_id);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// 从最近一次桌宠快照里取会话的标题与目标（失败通知的上下文文案）。
fn session_title_goal(app: &AppHandle, session_id: &str) -> (Option<String>, Option<String>) {
    let snapshot = app.state::<AppState>().pet_snapshot.lock().unwrap().clone();
    let Some(snap) = snapshot else {
        return (None, None);
    };
    let Some(sessions) = snap.get("sessions").and_then(|s| s.as_array()) else {
        return (None, None);
    };
    for s in sessions {
        if s.get("sessionId").and_then(|x| x.as_str()) == Some(session_id) {
            let title = s.get("title").and_then(|t| t.as_str()).map(String::from);
            let goal = s.get("goal").and_then(goal_objective);
            return (title, goal);
        }
    }
    (None, None)
}

/// 一条 WebSocket 下行流（mux 或 host）。断线后指数退避重连。
/// DSH ≥ 0.1.2 的流式握手校验会话 cookie，能带上时随握手头一起发。
fn spawn_stream(
    app: AppHandle,
    base: String,
    cookie: Option<String>,
    stop: Arc<AtomicBool>,
    mux: bool,
) {
    std::thread::spawn(move || {
        let path = if mux {
            "/api/events.mux"
        } else {
            "/api/events.host"
        };
        let url = format!("{}{}", ws_base(&base), path);
        let mut backoff_ms: u64 = 500;
        while !stop.load(Ordering::SeqCst) {
            // 握手请求：有会话 cookie 就注入 Cookie 头（无 cookie 时保持原样，
            // 兼容 0.1.1 及更早的无认证 dsh）。
            let connect_result = match cookie.as_deref() {
                Some(c) => {
                    use tungstenite::client::IntoClientRequest;
                    use tungstenite::http::HeaderValue;
                    match url.as_str().into_client_request() {
                        Ok(mut request) => match HeaderValue::from_str(c) {
                            Ok(value) => {
                                request
                                    .headers_mut()
                                    .insert(tungstenite::http::header::COOKIE, value);
                                tungstenite::connect(request)
                            }
                            Err(_) => tungstenite::connect(url.as_str()),
                        },
                        Err(_) => tungstenite::connect(url.as_str()),
                    }
                }
                None => tungstenite::connect(url.as_str()),
            };
            match connect_result {
                Ok((mut socket, _)) => {
                    backoff_ms = 500;
                    while !stop.load(Ordering::SeqCst) {
                        match socket.read() {
                            Ok(tungstenite::Message::Text(t)) => {
                                handle_stream_frame(&app, t.as_str())
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                }
                Err(_) => {}
            }
            // 指数退避（同时响应停止请求）。
            let mut waited_ms: u64 = 0;
            while waited_ms < backoff_ms && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
                waited_ms += 100;
            }
            if backoff_ms < 15_000 {
                backoff_ms = (backoff_ms * 2).min(15_000);
            }
        }
    });
}

/// 主循环：每 2s 探测服务器状态、维护两条下行流、拉取并推送会话状态。
fn orchestrator(app: AppHandle) {
    let mut mux_stop: Option<Arc<AtomicBool>> = None;
    let mut host_stop: Option<Arc<AtomicBool>> = None;
    let mut last_url: Option<String> = None;
    let mut last_phase: Option<String> = None;
    let mut last_style_text: Option<String> = None;
    // 上一轮轮询仍在运行的主会话（排除空白会话与子代理），用于完成/中断检测。
    let mut prev_running: RunningMap = RunningMap::new();

    loop {
        let st = app.state::<AppState>();
        if st.pet_stop.load(Ordering::SeqCst) {
            break;
        }
        let base = current_base(&st);
        let cookie = st.auth_cookie.lock().unwrap().clone();
        drop(st);

        // 桌宠样式契约热更新：文件内容变化 → 广播并应用位置。
        let style_text = std::fs::read_to_string(crate::pet_style::style_path()).ok();
        if style_text != last_style_text {
            last_style_text = style_text;
            let style = crate::pet_style::load();
            crate::pet_style::apply_position(&app, style.position.as_ref());
            crate::pet_style::broadcast_style(&app, &style);
        }

        match base {
            Some(u) => {
                if last_url.as_deref() != Some(u.as_str()) {
                    if let Some(s) = mux_stop.take() {
                        s.store(true, Ordering::SeqCst);
                    }
                    if let Some(s) = host_stop.take() {
                        s.store(true, Ordering::SeqCst);
                    }
                    let mux_s = Arc::new(AtomicBool::new(false));
                    let host_s = Arc::new(AtomicBool::new(false));
                    spawn_stream(
                        app.clone(),
                        u.clone(),
                        cookie.clone(),
                        Arc::clone(&mux_s),
                        true,
                    );
                    spawn_stream(
                        app.clone(),
                        u.clone(),
                        cookie.clone(),
                        Arc::clone(&host_s),
                        false,
                    );
                    mux_stop = Some(mux_s);
                    host_stop = Some(host_s);
                    last_url = Some(u.clone());
                }
                match poll_state(&u, cookie.as_deref()) {
                    Ok(state) => {
                        // 完成检测：上一轮在跑的主会话全部结束（结束 / 被阻塞 /
                        // 从列表消失）→ 生成待查看通知，用户打开主界面后清除。
                        let current_running: RunningMap = state
                            .sessions
                            .iter()
                            .filter(|s| s.running && !s.blank && !s.is_subagent)
                            .map(|s| {
                                (
                                    s.session_id.clone(),
                                    (
                                        s.title.clone(),
                                        s.goal.as_ref().and_then(goal_objective),
                                        s.goal
                                            .as_ref()
                                            .and_then(|g| g.get("phase"))
                                            .and_then(|p| p.as_str())
                                            == Some("blocked"),
                                    ),
                                )
                            })
                            .collect();
                        if let Some(notice) = detect_completion(&prev_running, &current_running) {
                            // 流侧已通过 turn/end error 通知「任务失败」的会话，
                            // 轮询不再重复发「已完成」（避免失败被报成完成）。
                            let st = app.state::<AppState>();
                            let mut failed = st.pet_failed_sessions.lock().unwrap();
                            let had_failed = prev_running.keys().any(|k| failed.contains(k));
                            if had_failed {
                                for k in prev_running.keys() {
                                    failed.remove(k);
                                }
                                pet_log("pet notice suppressed: failed sent via turn/end");
                            } else {
                                // 主窗口聚焦时用户正看着 DSH，「任务完成」通知冗余 → 抑制；
                                // 被阻塞 / 被中断仍需提醒（可能不在场或需要处理）。
                                let main_open = app
                                    .state::<AppState>()
                                    .main_open
                                    .load(Ordering::SeqCst);
                                if notice.kind == "completed" && main_open {
                                    pet_log("pet notice suppressed: main window focused");
                                } else {
                                    pet_log(&format!("pet notice: {}", notice.kind));
                                    set_notice(&app, notice);
                                }
                            }
                        }
                        prev_running = current_running;
                        if last_phase.as_deref() != Some("running") {
                            pet_log(&format!("poll ok, base={u}"));
                            last_phase = Some("running".into());
                        }
                        emit_state(&app, state);
                    }
                    Err(e) => {
                        if last_phase.as_deref() != Some("error") {
                            pet_log(&format!("poll failed, base={u}, reason={e}"));
                            last_phase = Some("error".into());
                        }
                        emit_state(&app, PetState::failed(e));
                    }
                }
            }
            None => {
                if let Some(s) = mux_stop.take() {
                    s.store(true, Ordering::SeqCst);
                }
                if let Some(s) = host_stop.take() {
                    s.store(true, Ordering::SeqCst);
                }
                last_url = None;
                // 中断检测：服务器不可达，且中断前仍有主会话在跑。
                if let Some(notice) = detect_interruption(last_phase.as_deref(), !prev_running.is_empty()) {
                    pet_log(&format!("pet notice: {}", notice.kind));
                    set_notice(&app, notice);
                    prev_running.clear();
                    // 服务器断开：失败会话标记一并失效。
                    app.state::<AppState>().pet_failed_sessions.lock().unwrap().clear();
                }
                if last_phase.as_deref() != Some("stopped") {
                    pet_log("no reachable server (recorded url and configured port both unhealthy)");
                    last_phase = Some("stopped".into());
                }
                emit_state(&app, PetState::stopped());
            }
        }

        // 分片睡眠，及时响应退出。
        for _ in 0..(POLL_INTERVAL.as_millis() / 100) {
            let st = app.state::<AppState>();
            if st.pet_stop.load(Ordering::SeqCst) {
                break;
            }
            drop(st);
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    if let Some(s) = mux_stop {
        s.store(true, Ordering::SeqCst);
    }
    if let Some(s) = host_stop {
        s.store(true, Ordering::SeqCst);
    }
}

/// 启动桌宠读取器（应用生命周期内调用一次）。
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || orchestrator(app));
}

/// 显示 / 隐藏 pet 窗口。
pub fn apply_visibility(app: &AppHandle, enabled: bool) {
    if let Some(w) = app.get_webview_window("pet") {
        if enabled {
            let _ = w.show();
        } else {
            let _ = w.hide();
        }
    }
}

/// 持久化 `pet_enabled` 并应用窗口可见性。
pub fn set_enabled(app: &AppHandle, enabled: bool) -> Result<bool, String> {
    {
        let st = app.state::<AppState>();
        st.settings.lock().unwrap().pet_enabled = enabled;
    }
    let st = app.state::<AppState>();
    let s = st.settings.lock().unwrap().clone();
    state::save_settings(app, &s)?;
    apply_visibility(app, enabled);
    Ok(enabled)
}

/// 回答一条审批（允许一次 / 拒绝）。`rpc_id` 是 `approval/requested` 帧的 rpcId。
fn respond_approval(
    base: &str,
    cookie: Option<&str>,
    rpc_id: &str,
    session_id: &str,
    approval_id: &str,
    outcome: &str,
) -> Result<bool, String> {
    let outcome = if outcome == "rejected" {
        "rejected"
    } else {
        "allowed-once"
    };
    let body = json!({
        "type": "client-response",
        "rpcId": rpc_id,
        "result": {
            "ok": true,
            "value": {
                "sessionId": session_id,
                "approvalId": approval_id,
                "outcome": outcome,
            }
        }
    });
    let url = format!("{base}/api/respond");
    let mut request = ureq::post(&url).set("Content-Type", "application/json");
    if let Some(cookie) = cookie {
        request = request.set("Cookie", cookie);
    }
    let resp = request
        .send_string(&body.to_string())
        .map_err(|e| format!("respond: {e}"))?;
    let text = resp.into_string().map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(v.get("accepted").and_then(|x| x.as_bool()).unwrap_or(false))
}

#[tauri::command]
pub fn pet_status(st: State<'_, AppState>) -> Option<Value> {
    st.pet_snapshot.lock().unwrap().clone()
}

#[tauri::command]
pub fn pet_open_session(app: AppHandle, session_id: Option<String>) {
    crate::show_main(&app);
    let _ = app.emit_to("main", "pet-open-session", session_id);
}

#[tauri::command]
pub fn pet_respond(
    st: State<'_, AppState>,
    rpc_id: String,
    session_id: String,
    approval_id: String,
    outcome: String,
) -> Result<bool, String> {
    let base = current_base(&st).ok_or("服务器未运行")?;
    let cookie = st.auth_cookie.lock().unwrap().clone();
    respond_approval(&base, cookie.as_deref(), &rpc_id, &session_id, &approval_id, &outcome)
}

#[tauri::command]
pub fn pet_set_enabled(app: AppHandle, enabled: bool) -> Result<bool, String> {
    set_enabled(&app, enabled)
}

/// 切换桌宠可见性（与托盘菜单一致）：按 Rust 当前状态翻转并返回新值。
/// 前端用它做右键菜单切换，避免依赖可能与后端错位的前端缓存值。
#[tauri::command]
pub fn pet_toggle(app: AppHandle) -> Result<bool, String> {
    let st = app.state::<AppState>();
    let enabled = !st.settings.lock().unwrap().pet_enabled;
    set_enabled(&app, enabled)
}

/// 桌宠右键菜单：显示并聚焦主窗口（与托盘"打开面板"一致）。
#[tauri::command]
pub fn show_main_window(app: AppHandle) {
    crate::show_main(&app);
}

/// 桌宠右键菜单：退出应用（与托盘"退出"一致；按「服务跟随鲸仔程序停止」
/// 设置决定是否先停服，见 commands::quit_app）。
#[tauri::command]
pub fn quit_app(app: AppHandle) {
    crate::commands::quit_app(&app);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(
        id: &str,
        title: Option<&str>,
        goal: Option<&str>,
        blocked: bool,
    ) -> (String, (Option<String>, Option<String>, bool)) {
        (
            id.to_string(),
            (
                title.map(String::from),
                goal.map(String::from),
                blocked,
            ),
        )
    }

    #[test]
    fn no_completion_when_nothing_was_running() {
        assert_eq!(detect_completion(&RunningMap::new(), &RunningMap::new()), None);
    }

    #[test]
    fn no_completion_while_a_session_still_runs() {
        let prev = RunningMap::from([running("a", None, None, false)]);
        let cur = RunningMap::from([running("a", None, None, false)]);
        assert_eq!(detect_completion(&prev, &cur), None);
    }

    #[test]
    fn completion_prefers_goal_text() {
        let prev = RunningMap::from([
            running("a", Some("会话A"), None, false),
            running("b", None, Some("目标B"), false),
        ]);
        let n = detect_completion(&prev, &RunningMap::new()).unwrap();
        assert_eq!(n.kind, "completed");
        assert_eq!(n.goal.as_deref(), Some("目标B"));
    }

    #[test]
    fn completion_falls_back_to_title() {
        let prev = RunningMap::from([running("a", Some("会话A"), None, false)]);
        let n = detect_completion(&prev, &RunningMap::new()).unwrap();
        assert_eq!(n.kind, "completed");
        assert_eq!(n.title.as_deref(), Some("会话A"));
    }

    #[test]
    fn blocked_beats_completed() {
        let prev = RunningMap::from([
            running("a", None, Some("目标A"), false),
            running("b", None, Some("目标B"), true),
        ]);
        let n = detect_completion(&prev, &RunningMap::new()).unwrap();
        assert_eq!(n.kind, "blocked");
    }

    #[test]
    fn interruption_only_on_running_to_stopped_transition() {
        let n = detect_interruption(Some("running"), true).unwrap();
        assert_eq!(n.kind, "interrupted");
        // error 阶段（服务器在但列表拉取失败）同样可能有任务在跑。
        assert_eq!(
            detect_interruption(Some("error"), true).unwrap().kind,
            "interrupted"
        );
        assert_eq!(detect_interruption(Some("running"), false), None);
        assert_eq!(detect_interruption(Some("error"), false), None);
        assert_eq!(detect_interruption(Some("stopped"), true), None);
        assert_eq!(detect_interruption(None, true), None);
    }
}
