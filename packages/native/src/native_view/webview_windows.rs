//! Windows Edge WebView2 host for Solo's `<webview>` element.
//!
//! A WebView2 controller is a native Win32 child, attached directly to GPUI's
//! HWND via `CreateCoreWebView2Controller` and positioned using GPUI's layout bounds.
//!
//! GPUI owns layout; WebView2 owns rendering and input.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::channel::oneshot;
use webview2_com::take_pwstr;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2Controller,
    ICoreWebView2NavigationCompletedEventArgs, ICoreWebView2NavigationStartingEventArgs,
    ICoreWebView2NewWindowRequestedEventArgs, ICoreWebView2Settings2,
    ICoreWebView2SourceChangedEventArgs, ICoreWebView2WebMessageReceivedEventArgs,
    COREWEBVIEW2_WEB_ERROR_STATUS, COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    ExecuteScriptCompletedHandler, NavigationCompletedEventHandler,
    NavigationStartingEventHandler, NewWindowRequestedEventHandler,
    SourceChangedEventHandler, WebMessageReceivedEventHandler,
};
use windows::core::Interface;

use super::{EvaluateReceiver, NativeViewFrame, NativeViewInstance, ReadyReceiver};
use crate::renderer::{emit_event_full, EventCallback};

// ── Event names ──────────────────────────────────────────────────────

const EVENT_NAVIGATION: &str = "navigation";
const EVENT_NAVIGATION_REQUEST: &str = "navigationRequest";
const EVENT_LOAD: &str = "load";
const EVENT_LOAD_ERROR: &str = "loadError";

const PROP_URL: &str = "url";
const PROP_HTML: &str = "html";
const PROP_BASE_URL: &str = "baseUrl";
const PROP_USER_AGENT: &str = "userAgent";

type EvaluateSender = oneshot::Sender<anyhow::Result<String>>;
type ReadySender = oneshot::Sender<anyhow::Result<u64>>;

static NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);

fn new_nonce() -> String {
    let id = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:024x}{id:08x}")
}

/// Wrap an expression so WebView2 returns its JSON without needing top-level await.
/// Delivers the result through `window.chrome.webview.postMessage` as `{ id, nonce, ok, value }`.
fn evaluation_script(script: &str, id: u64, nonce: &str) -> String {
    format!(
        "(async () => {{ try {{ const value = await ({script}); if (value === undefined) return \"null\"; const seen = new WeakSet(); return JSON.stringify(value, (_key, item) => {{ if (typeof item === \"number\" && !Number.isFinite(item)) throw new Error(\"unsupported non-finite number\"); if (typeof item === \"undefined\" || typeof item === \"function\" || typeof item === \"symbol\" || typeof item === \"bigint\") throw new Error(\"unsupported JavaScript result\"); if (item !== null && typeof item === \"object\") {{ if (seen.has(item)) throw new Error(\"unsupported cyclic result\"); seen.add(item); }} return item; }}); }} catch (error) {{ throw error; }} }})().then((value) => window.chrome.webview.postMessage(JSON.stringify({{ id: {id}, nonce: \"{nonce}\", ok: true, value }})), (error) => window.chrome.webview.postMessage(JSON.stringify({{ id: {id}, nonce: \"{nonce}\", ok: false, value: String(error && error.message ? error.message : error) }})));"
    )
}

fn without_fragment(url: &str) -> &str {
    url.split_once('#').map_or(url, |(without, _)| without)
}

fn emit(
    callback: &Option<EventCallback>,
    element_id: u64,
    event_type: &str,
    value: Option<String>,
) {
    let value = value.unwrap_or_default();
    emit_event_full(callback, element_id, event_type, move |payload| {
        payload.value = Some(value);
    });
}

fn emit_navigation_request(
    callback: &Option<EventCallback>,
    element_id: u64,
    navigation_id: u64,
    value: Option<String>,
    same_document: bool,
    new_window: bool,
) {
    emit_event_full(
        callback,
        element_id,
        EVENT_NAVIGATION_REQUEST,
        move |payload| {
            payload.navigation_id = Some(navigation_id as f64);
            payload.value = value.clone();
            payload.navigation_url = value;
            payload.is_same_document = Some(same_document);
            payload.is_new_window = Some(new_window);
        },
    );
}

#[derive(Default)]
struct NavigationState {
    intercept: bool,
    controlled_load: bool,
    next_id: u64,
    pending: HashMap<u64, String>,
}

#[derive(Default)]
struct ReadinessState {
    generation: u64,
    ready: bool,
    waiters: HashMap<u64, Vec<ReadySender>>,
}

struct WebViewState {
    element_id: u64,
    callback: Option<EventCallback>,
    controller: Option<ICoreWebView2Controller>,
    webview: Option<ICoreWebView2>,
    hwnd: Option<windows::Win32::Foundation::HWND>,
    last_frame: Option<NativeViewFrame>,
    pending_url: Option<String>,
    pending_html: Option<(String, Option<String>)>,
    pending_user_agent: Option<String>,
    last_url: Option<String>,
    last_html: Option<String>,
    last_base_url: Option<String>,
    last_user_agent: Option<String>,
    current_url: Option<String>,
    navigation: NavigationState,
    evaluations: HashMap<u64, EvaluateSender>,
    readiness: ReadinessState,
    next_evaluation_id: u64,
    eval_nonce: String,
    visible: bool,
    mounted: bool,
    destroyed: bool,
    initialization_failed: bool,
    token_navigation_starting: i64,
    token_navigation_completed: i64,
    token_new_window: i64,
    token_web_message: i64,
    token_source_changed: i64,
}

impl WebViewState {
    fn new(element_id: u64, callback: Option<EventCallback>) -> Self {
        Self {
            element_id,
            callback,
            controller: None,
            webview: None,
            hwnd: None,
            last_frame: None,
            pending_url: None,
            pending_html: None,
            pending_user_agent: None,
            last_url: None,
            last_html: None,
            last_base_url: None,
            last_user_agent: None,
            current_url: None,
            navigation: NavigationState {
                intercept: false,
                controlled_load: false,
                next_id: 1,
                pending: HashMap::new(),
            },
            evaluations: HashMap::new(),
            readiness: ReadinessState {
                generation: 1,
                ready: false,
                waiters: HashMap::new(),
            },
            next_evaluation_id: 1,
            eval_nonce: new_nonce(),
            visible: true,
            mounted: false,
            destroyed: false,
            initialization_failed: false,
            token_navigation_starting: 0,
            token_navigation_completed: 0,
            token_new_window: 0,
            token_web_message: 0,
            token_source_changed: 0,
        }
    }

    fn handle_runtime_failure(&mut self, message: &str) {
        self.initialization_failed = true;
        log::error!("webview (element {}): {}", self.element_id, message);
        let msg = message.to_string();
        emit(&self.callback, self.element_id, EVENT_LOAD_ERROR, Some(msg.clone()));
        for (_, sender) in self.evaluations.drain() {
            let _ = sender.send(Err(anyhow::anyhow!("{msg}")));
        }
        for (_, senders) in std::mem::take(&mut self.readiness.waiters) {
            for sender in senders {
                let _ = sender.send(Err(anyhow::anyhow!("{msg}")));
            }
        }
    }

    fn attach_controller(
        &mut self,
        state_rc: Rc<RefCell<WebViewState>>,
        controller: ICoreWebView2Controller,
    ) {
        let webview = match unsafe { controller.CoreWebView2() } {
            Ok(wv) => wv,
            Err(err) => {
                self.handle_runtime_failure(&format!(
                    "Failed to obtain CoreWebView2 interface: {err}"
                ));
                let _ = unsafe { controller.Close() };
                return;
            }
        };

        if let Ok(settings) = unsafe { webview.Settings() } {
            let _ = unsafe { settings.SetIsScriptEnabled(true) };
            let _ = unsafe { settings.SetAreDefaultScriptDialogsEnabled(true) };
            let _ = unsafe { settings.SetIsWebMessageEnabled(true) };
        }

        let _ = unsafe { controller.SetIsVisible(self.visible) };
        if let Some(frame) = self.last_frame {
            self.apply_bounds(&controller, frame);
        }

        if let Some(ua) = &self.pending_user_agent {
            let _ = self.apply_user_agent(&webview, Some(ua));
        }

        // 1. NavigationStarting
        {
            let state = state_rc.clone();
            let mut token = 0i64;
            let handler = NavigationStartingEventHandler::create(Box::new(move |_sender, args| {
                let mut guard = state.borrow_mut();
                if guard.destroyed {
                    return Ok(());
                }
                if let Some(args) = args {
                    guard.on_navigation_starting(&args);
                }
                Ok(())
            }));
            if unsafe { webview.add_NavigationStarting(&handler, &mut token) }.is_ok() {
                self.token_navigation_starting = token;
            }
        }

        // 2. NavigationCompleted
        {
            let state = state_rc.clone();
            let mut token = 0i64;
            let handler = NavigationCompletedEventHandler::create(Box::new(move |_sender, args| {
                let mut guard = state.borrow_mut();
                if guard.destroyed {
                    return Ok(());
                }
                if let Some(args) = args {
                    guard.on_navigation_completed(&args);
                }
                Ok(())
            }));
            if unsafe { webview.add_NavigationCompleted(&handler, &mut token) }.is_ok() {
                self.token_navigation_completed = token;
            }
        }

        // 3. NewWindowRequested
        {
            let state = state_rc.clone();
            let mut token = 0i64;
            let handler = NewWindowRequestedEventHandler::create(Box::new(move |_sender, args| {
                let mut guard = state.borrow_mut();
                if guard.destroyed {
                    return Ok(());
                }
                if let Some(args) = args {
                    guard.on_new_window_requested(&args);
                }
                Ok(())
            }));
            if unsafe { webview.add_NewWindowRequested(&handler, &mut token) }.is_ok() {
                self.token_new_window = token;
            }
        }

        // 4. WebMessageReceived
        {
            let state = state_rc.clone();
            let mut token = 0i64;
            let handler = WebMessageReceivedEventHandler::create(Box::new(move |_sender, args| {
                let mut guard = state.borrow_mut();
                if guard.destroyed {
                    return Ok(());
                }
                if let Some(args) = args {
                    guard.on_web_message_received(&args);
                }
                Ok(())
            }));
            if unsafe { webview.add_WebMessageReceived(&handler, &mut token) }.is_ok() {
                self.token_web_message = token;
            }
        }

        // 5. SourceChanged
        {
            let state = state_rc.clone();
            let mut token = 0i64;
            let handler = SourceChangedEventHandler::create(Box::new(move |_sender, args| {
                let mut guard = state.borrow_mut();
                if guard.destroyed {
                    return Ok(());
                }
                if let Some(args) = args {
                    guard.on_source_changed(&args);
                }
                Ok(())
            }));
            if unsafe { webview.add_SourceChanged(&handler, &mut token) }.is_ok() {
                self.token_source_changed = token;
            }
        }

        self.controller = Some(controller);
        self.webview = Some(webview);

        if let Some(url) = self.pending_url.take() {
            self.load_url_inner(&url);
        } else if let Some((html, base_url)) = self.pending_html.take() {
            self.load_html_inner(&html, base_url.as_deref());
        } else {
            // No initial url or html: view is mounted and ready for evaluation
            self.mark_ready();
        }
    }

    fn apply_bounds(&self, controller: &ICoreWebView2Controller, frame: NativeViewFrame) {
        let rect = windows::Win32::Foundation::RECT {
            left: frame.x as i32,
            top: frame.y as i32,
            right: (frame.x + frame.width) as i32,
            bottom: (frame.y + frame.height) as i32,
        };
        let _ = unsafe { controller.SetBounds(rect) };
    }

    fn apply_user_agent(
        &self,
        webview: &ICoreWebView2,
        user_agent: Option<&str>,
    ) -> windows::core::Result<()> {
        if let Ok(settings) = unsafe { webview.Settings() } {
            if let Ok(settings2) = settings.cast::<ICoreWebView2Settings2>() {
                match user_agent {
                    Some(ua) => {
                        let hstring = windows::core::HSTRING::from(ua);
                        unsafe { settings2.SetUserAgent(&hstring) }?;
                    }
                    None => {
                        unsafe { settings2.SetUserAgent(windows::core::PCWSTR::null()) }?;
                    }
                }
            }
        }
        Ok(())
    }

    fn query_current_url(&self) -> Option<String> {
        let webview = self.webview.as_ref()?;
        let mut uri_pwstr = windows::core::PWSTR::null();
        if unsafe { webview.Source(&mut uri_pwstr) }.is_ok() && !uri_pwstr.is_null() {
            Some(take_pwstr(uri_pwstr))
        } else {
            None
        }
    }

    fn begin_document(&mut self) {
        for (_, sender) in self.evaluations.drain() {
            let _ = sender.send(Err(anyhow::anyhow!("WebView document changed")));
        }
        self.readiness.generation = self.readiness.generation.wrapping_add(1).max(1);
        self.readiness.ready = false;
        for (_, senders) in std::mem::take(&mut self.readiness.waiters) {
            for sender in senders {
                let _ = sender.send(Err(anyhow::anyhow!("WebView document changed")));
            }
        }
    }

    fn mark_ready(&mut self) -> bool {
        self.readiness.ready = true;
        let waiters = self.readiness.waiters.remove(&self.readiness.generation);
        if let Some(waiters) = waiters {
            for sender in waiters {
                let _ = sender.send(Ok(self.readiness.generation));
            }
        }
        true
    }

    fn mark_failed(&mut self, reason: &str) -> bool {
        self.readiness.ready = false;
        let waiters = self.readiness.waiters.remove(&self.readiness.generation);
        if let Some(waiters) = waiters {
            for sender in waiters {
                let _ = sender.send(Err(anyhow::anyhow!("{reason}")));
            }
        }
        true
    }

    fn on_source_changed(&mut self, args: &ICoreWebView2SourceChangedEventArgs) {
        let mut is_new = windows::core::BOOL::default();
        let is_new_doc = unsafe { args.IsNewDocument(&mut is_new) }.is_ok() && is_new.as_bool();
        if !is_new_doc {
            let current_url = self.query_current_url();
            let navigation_id = self.navigation.next_id;
            self.navigation.next_id = self.navigation.next_id.wrapping_add(1).max(1);
            emit_navigation_request(
                &self.callback,
                self.element_id,
                navigation_id,
                current_url.clone(),
                true,
                false,
            );
            emit(&self.callback, self.element_id, EVENT_NAVIGATION, current_url);
        }
    }

    fn on_navigation_starting(&mut self, args: &ICoreWebView2NavigationStartingEventArgs) {
        let mut uri_pwstr = windows::core::PWSTR::null();
        let requested_url = if unsafe { args.Uri(&mut uri_pwstr) }.is_ok() && !uri_pwstr.is_null() {
            Some(take_pwstr(uri_pwstr))
        } else {
            None
        };

        let mut user_initiated = windows::core::BOOL::default();
        let is_user_initiated =
            unsafe { args.IsUserInitiated(&mut user_initiated) }.is_ok() && user_initiated.as_bool();

        let intercept = self.navigation.intercept;
        let controlled_load = self.navigation.controlled_load;
        self.navigation.controlled_load = false;
        let navigation_id = self.navigation.next_id;
        self.navigation.next_id = self.navigation.next_id.wrapping_add(1).max(1);

        let mut cancelled = false;
        if controlled_load {
            let _ = unsafe { args.SetCancel(false) };
        } else if !is_user_initiated {
            self.begin_document();
            let _ = unsafe { args.SetCancel(false) };
        } else if intercept {
            let _ = unsafe { args.SetCancel(true) };
            cancelled = true;
            if let Some(url) = requested_url.clone() {
                self.navigation.pending.insert(navigation_id, url);
            }
            emit_navigation_request(
                &self.callback,
                self.element_id,
                navigation_id,
                requested_url.clone(),
                false,
                false,
            );
        } else {
            self.begin_document();
            let _ = unsafe { args.SetCancel(false) };
        }

        if !cancelled {
            emit(&self.callback, self.element_id, EVENT_NAVIGATION, requested_url);
        }
    }

    fn on_navigation_completed(&mut self, args: &ICoreWebView2NavigationCompletedEventArgs) {
        let mut is_success = windows::core::BOOL::default();
        let success = unsafe { args.IsSuccess(&mut is_success) }.is_ok() && is_success.as_bool();

        let current_url = self.query_current_url();
        if let Some(url) = &current_url {
            self.current_url = Some(url.clone());
        }

        if success {
            if self.mark_ready() {
                emit(&self.callback, self.element_id, EVENT_LOAD, current_url);
            }
        } else {
            let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
            let _ = unsafe { args.WebErrorStatus(&mut status) };
            // Intercepted navigations cancelled with SetCancel(true) report status OPERATION_CANCELED (value 14).
            // Do not emit loadError or mark failed for cancelled navigations.
            if status == COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED || status.0 == 14 {
                return;
            }
            let reason = format!("Navigation failed with status {}", status.0);
            if self.mark_failed(&reason) {
                let failure = match current_url {
                    Some(u) => format!("{u} — {reason}"),
                    None => reason,
                };
                emit(&self.callback, self.element_id, EVENT_LOAD_ERROR, Some(failure));
            }
        }
    }

    fn on_new_window_requested(&mut self, args: &ICoreWebView2NewWindowRequestedEventArgs) {
        let mut uri_pwstr = windows::core::PWSTR::null();
        let requested_url = if unsafe { args.Uri(&mut uri_pwstr) }.is_ok() && !uri_pwstr.is_null() {
            Some(take_pwstr(uri_pwstr))
        } else {
            None
        };
        let _ = unsafe { args.SetHandled(true) };
        let navigation_id = self.navigation.next_id;
        self.navigation.next_id = self.navigation.next_id.wrapping_add(1).max(1);
        emit_navigation_request(
            &self.callback,
            self.element_id,
            navigation_id,
            requested_url,
            false,
            true,
        );
    }

    fn on_web_message_received(&mut self, args: &ICoreWebView2WebMessageReceivedEventArgs) {
        let mut message_pwstr = windows::core::PWSTR::null();
        if unsafe { args.TryGetWebMessageAsString(&mut message_pwstr) }.is_err()
            || message_pwstr.is_null()
        {
            return;
        }
        let raw = take_pwstr(message_pwstr);
        #[derive(serde::Deserialize)]
        struct EvalEnvelope {
            id: u64,
            nonce: String,
            ok: bool,
            value: String,
        }
        let Ok(envelope) = serde_json::from_str::<EvalEnvelope>(&raw) else {
            return;
        };
        if envelope.nonce != self.eval_nonce {
            return;
        }
        if let Some(sender) = self.evaluations.remove(&envelope.id) {
            let result = if envelope.ok {
                Ok(envelope.value)
            } else {
                Err(anyhow::anyhow!(envelope.value))
            };
            let _ = sender.send(result);
        }
    }

    fn load_url_inner(&mut self, url: &str) {
        if url::Url::parse(url).is_err() {
            let msg = format!("{url} — invalid URL");
            self.mark_failed(&msg);
            emit(&self.callback, self.element_id, EVENT_LOAD_ERROR, Some(msg));
            return;
        }
        let Some(webview) = self.webview.clone() else {
            self.pending_url = Some(url.to_string());
            self.pending_html = None;
            self.begin_document();
            return;
        };
        self.begin_document();
        self.navigation.controlled_load = true;
        let hstring = windows::core::HSTRING::from(url);
        let _ = unsafe { webview.Navigate(&hstring) };
    }

    fn load_html_inner(&mut self, html: &str, base_url: Option<&str>) {
        if let Some(base) = base_url {
            if url::Url::parse(base).is_err() {
                let msg = format!("{base} — invalid base URL");
                self.mark_failed(&msg);
                emit(&self.callback, self.element_id, EVENT_LOAD_ERROR, Some(msg));
                return;
            }
        }
        let Some(webview) = self.webview.clone() else {
            self.pending_html = Some((html.to_string(), base_url.map(str::to_string)));
            self.pending_url = None;
            self.begin_document();
            return;
        };
        self.begin_document();
        self.navigation.controlled_load = true;

        let content = match base_url {
            Some(base) if !html.to_ascii_lowercase().contains("<base ") => {
                let escaped_base = base
                    .replace('&', "&amp;")
                    .replace('"', "&quot;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;");
                let base_tag = format!("<base href=\"{escaped_base}\">");
                let lower = html.to_ascii_lowercase();
                if let Some(pos) = lower.find("<head") {
                    if let Some(close_rel) = html[pos..].find('>') {
                        let insert_idx = pos + close_rel + 1;
                        format!("{}{}{}", &html[..insert_idx], base_tag, &html[insert_idx..])
                    } else {
                        format!("{base_tag}{html}")
                    }
                } else if let Some(pos) = lower.find("<!doctype") {
                    if let Some(close_rel) = html[pos..].find('>') {
                        let insert_idx = pos + close_rel + 1;
                        format!("{}{}{}", &html[..insert_idx], base_tag, &html[insert_idx..])
                    } else {
                        format!("{base_tag}{html}")
                    }
                } else {
                    format!("{base_tag}{html}")
                }
            }
            _ => html.to_string(),
        };

        let hstring = windows::core::HSTRING::from(content.as_str());
        let _ = unsafe { webview.NavigateToString(&hstring) };
    }

    fn destroy(&mut self) {
        if self.destroyed {
            return;
        }
        self.destroyed = true;
        if let Some(wv) = &self.webview {
            if self.token_navigation_starting != 0 {
                let _ = unsafe { wv.remove_NavigationStarting(self.token_navigation_starting) };
            }
            if self.token_navigation_completed != 0 {
                let _ = unsafe { wv.remove_NavigationCompleted(self.token_navigation_completed) };
            }
            if self.token_new_window != 0 {
                let _ = unsafe { wv.remove_NewWindowRequested(self.token_new_window) };
            }
            if self.token_web_message != 0 {
                let _ = unsafe { wv.remove_WebMessageReceived(self.token_web_message) };
            }
            if self.token_source_changed != 0 {
                let _ = unsafe { wv.remove_SourceChanged(self.token_source_changed) };
            }
        }
        if let Some(controller) = self.controller.take() {
            let _ = unsafe { controller.Close() };
        }
        self.webview = None;
        for (_, sender) in self.evaluations.drain() {
            let _ = sender.send(Err(anyhow::anyhow!("WebView was unmounted")));
        }
        for (_, senders) in std::mem::take(&mut self.readiness.waiters) {
            for sender in senders {
                let _ = sender.send(Err(anyhow::anyhow!("WebView was unmounted")));
            }
        }
    }
}

// ── The native view instance ─────────────────────────────────────────

pub struct WindowsWebView {
    state: Rc<RefCell<WebViewState>>,
}

impl WindowsWebView {
    pub fn new(element_id: u64, callback: Option<EventCallback>) -> Self {
        Self {
            state: Rc::new(RefCell::new(WebViewState::new(element_id, callback))),
        }
    }
}

impl NativeViewInstance for WindowsWebView {
    fn mount(&mut self, host: *mut c_void) {
        let mut guard = self.state.borrow_mut();
        if guard.mounted || guard.destroyed || host.is_null() {
            return;
        }
        guard.mounted = true;
        let hwnd = windows::Win32::Foundation::HWND(host);
        guard.hwnd = Some(hwnd);

        let app_id = crate::renderer::current_app_id();
        let user_data = std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(app_id)
            .join("webview2");
        let user_data_hstring = windows::core::HSTRING::from(user_data.to_string_lossy().as_ref());

        let state_env = self.state.clone();
        let env_handler =
            CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |res, env| {
                let mut guard = state_env.borrow_mut();
                if guard.destroyed {
                    return Ok(());
                }
                let env = match res.and_then(|()| {
                    env.ok_or_else(|| {
                        windows::core::Error::from(windows::core::HRESULT(0x80004005u32 as i32))
                    })
                }) {
                    Ok(env) => env,
                    Err(err) => {
                        guard.handle_runtime_failure(&format!(
                            "WebView2 runtime failed to initialize: {err}. Install the Microsoft Edge WebView2 Evergreen Runtime: https://go.microsoft.com/fwlink/p/?LinkId=2124703"
                        ));
                        return Ok(());
                    }
                };

                let hwnd = guard.hwnd.expect("hwnd present");
                let state_ctrl = state_env.clone();
                let ctrl_handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
                    move |res, controller| {
                        let mut guard = state_ctrl.borrow_mut();
                        if guard.destroyed {
                            if let Some(c) = controller {
                                let _ = unsafe { c.Close() };
                            }
                            return Ok(());
                        }
                        let controller = match res.and_then(|()| {
                            controller.ok_or_else(|| {
                                windows::core::Error::from(windows::core::HRESULT(
                                    0x80004005u32 as i32,
                                ))
                            })
                        }) {
                            Ok(c) => c,
                            Err(err) => {
                                guard.handle_runtime_failure(&format!(
                                    "WebView2 controller failed to initialize: {err}"
                                ));
                                return Ok(());
                            }
                        };
                        guard.attach_controller(state_ctrl.clone(), controller);
                        Ok(())
                    },
                ));

                let create_res = unsafe { env.CreateCoreWebView2Controller(hwnd, &ctrl_handler) };
                if let Err(err) = create_res {
                    guard.handle_runtime_failure(&format!(
                        "Failed to request WebView2 controller: {err}"
                    ));
                }
                Ok(())
            }));

        let create_env_res = unsafe {
            CreateCoreWebView2EnvironmentWithOptions(
                windows::core::PCWSTR::null(),
                &user_data_hstring,
                None,
                &env_handler,
            )
        };
        if let Err(err) = create_env_res {
            guard.handle_runtime_failure(&format!(
                "WebView2 runtime is not installed or failed to start: {err}. Install the Microsoft Edge WebView2 Evergreen Runtime: https://go.microsoft.com/fwlink/p/?LinkId=2124703"
            ));
        }
    }

    fn update_frame(&mut self, frame: NativeViewFrame) {
        let mut guard = self.state.borrow_mut();
        if guard.destroyed {
            return;
        }
        guard.last_frame = Some(frame);
        let visible = frame.is_renderable();
        if guard.visible != visible {
            guard.visible = visible;
            if let Some(controller) = &guard.controller {
                let _ = unsafe { controller.SetIsVisible(visible) };
            }
        }
        if let Some(controller) = guard.controller.clone() {
            guard.apply_bounds(&controller, frame);
        }
    }

    fn set_content(&mut self, key: &str, value: Option<&str>) {
        let mut guard = self.state.borrow_mut();
        if guard.destroyed {
            return;
        }
        match key {
            PROP_URL => {
                if value.is_some() && guard.last_html.is_some() {
                    emit(
                        &guard.callback,
                        guard.element_id,
                        EVENT_LOAD_ERROR,
                        Some("WebView url and html are mutually exclusive".to_string()),
                    );
                    return;
                }
                if guard.last_url.as_deref() == value {
                    return;
                }
                guard.begin_document();
                guard.last_url = value.map(str::to_string);
                if let Some(url) = value {
                    guard.load_url_inner(url);
                }
            }
            PROP_HTML => {
                if value.is_some() && guard.last_url.is_some() {
                    emit(
                        &guard.callback,
                        guard.element_id,
                        EVENT_LOAD_ERROR,
                        Some("WebView url and html are mutually exclusive".to_string()),
                    );
                    return;
                }
                if guard.last_html.as_deref() == value {
                    return;
                }
                guard.begin_document();
                guard.last_html = value.map(str::to_string);
                if let Some(html) = value {
                    let base_url = guard.last_base_url.clone();
                    guard.load_html_inner(html, base_url.as_deref());
                }
            }
            PROP_BASE_URL => {
                if value.is_some() && guard.last_url.is_some() {
                    emit(
                        &guard.callback,
                        guard.element_id,
                        EVENT_LOAD_ERROR,
                        Some("WebView baseUrl requires html and cannot be used with url".to_string()),
                    );
                    return;
                }
                if guard.last_base_url.as_deref() == value {
                    return;
                }
                guard.begin_document();
                guard.last_base_url = value.map(str::to_string);
                if let Some(html) = guard.last_html.clone() {
                    guard.load_html_inner(&html, value);
                }
            }
            PROP_USER_AGENT => {
                if guard.last_user_agent.as_deref() == value {
                    return;
                }
                guard.last_user_agent = value.map(str::to_string);
                guard.pending_user_agent = value.map(str::to_string);
                if let Some(webview) = guard.webview.clone() {
                    let _ = guard.apply_user_agent(&webview, value);
                }
            }
            _ => {}
        }
    }

    fn set_webview_source(
        &mut self,
        url: Option<&str>,
        html: Option<&str>,
        base_url: Option<&str>,
    ) {
        let mut guard = self.state.borrow_mut();
        if guard.destroyed {
            return;
        }
        if url.is_some() && html.is_some() {
            emit(
                &guard.callback,
                guard.element_id,
                EVENT_LOAD_ERROR,
                Some("WebView url and html are mutually exclusive".to_string()),
            );
            return;
        }
        if base_url.is_some() && html.is_none() {
            emit(
                &guard.callback,
                guard.element_id,
                EVENT_LOAD_ERROR,
                Some("WebView baseUrl requires html".to_string()),
            );
            return;
        }
        if guard.last_url.as_deref() == url
            && guard.last_html.as_deref() == html
            && guard.last_base_url.as_deref() == base_url
        {
            return;
        }

        guard.begin_document();
        guard.last_url = url.map(str::to_string);
        guard.last_html = html.map(str::to_string);
        guard.last_base_url = base_url.map(str::to_string);

        if let Some(html) = html {
            guard.load_html_inner(html, base_url);
        } else if let Some(url) = url {
            guard.load_url_inner(url);
        } else {
            guard.load_html_inner("", None);
        }
    }

    fn evaluate_javascript(&mut self, script: &str) -> anyhow::Result<EvaluateReceiver> {
        let mut guard = self.state.borrow_mut();
        if guard.destroyed {
            anyhow::bail!("WebView is unmounted");
        }
        if guard.initialization_failed {
            anyhow::bail!("WebView runtime failed to initialize");
        }
        let Some(webview) = guard.webview.clone() else {
            anyhow::bail!("WebView {} is not mounted", guard.element_id);
        };
        let (sender, receiver) = oneshot::channel();
        let eval_id = guard.next_evaluation_id;
        guard.next_evaluation_id = guard.next_evaluation_id.wrapping_add(1).max(1);
        guard.evaluations.insert(eval_id, sender);

        let wrapped = evaluation_script(script, eval_id, &guard.eval_nonce);
        let wrapped_hstring = windows::core::HSTRING::from(wrapped.as_str());

        let state_eval = self.state.clone();
        let handler = ExecuteScriptCompletedHandler::create(Box::new(move |res, _| {
            if let Err(err) = res {
                let mut guard = state_eval.borrow_mut();
                if let Some(sender) = guard.evaluations.remove(&eval_id) {
                    let _ = sender.send(Err(anyhow::anyhow!("Script execution failed: {err}")));
                }
            }
            Ok(())
        }));

        let eval_res = unsafe { webview.ExecuteScript(&wrapped_hstring, &handler) };
        if let Err(err) = eval_res {
            guard.evaluations.remove(&eval_id);
            anyhow::bail!("Failed to execute script: {err}");
        }
        Ok(receiver)
    }

    fn wait_for_ready(&mut self) -> anyhow::Result<ReadyReceiver> {
        let mut guard = self.state.borrow_mut();
        if guard.destroyed {
            anyhow::bail!("WebView is unmounted");
        }
        if guard.initialization_failed {
            anyhow::bail!("WebView runtime failed to initialize");
        }
        let (sender, receiver) = oneshot::channel();
        if guard.readiness.ready {
            let _ = sender.send(Ok(guard.readiness.generation));
        } else {
            let gen = guard.readiness.generation;
            guard.readiness.waiters.entry(gen).or_default().push(sender);
        }
        Ok(receiver)
    }

    fn set_navigation_interception(&mut self, enabled: bool) {
        let mut guard = self.state.borrow_mut();
        guard.navigation.intercept = enabled;
        if !enabled {
            guard.navigation.pending.clear();
        }
    }

    fn decide_navigation(&mut self, navigation_id: u64, allow: bool) -> bool {
        let mut guard = self.state.borrow_mut();
        let url = guard.navigation.pending.remove(&navigation_id);
        if let Some(url) = url {
            if allow {
                guard.navigation.controlled_load = true;
                guard.load_url_inner(&url);
            }
            true
        } else {
            false
        }
    }

    fn destroy(&mut self) {
        self.state.borrow_mut().destroy();
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_fragment_strips_hash() {
        assert_eq!(without_fragment("https://example.com/page#section"), "https://example.com/page");
        assert_eq!(without_fragment("https://example.com/page"), "https://example.com/page");
        assert_eq!(without_fragment("https://example.com/#top"), "https://example.com/");
    }

    #[test]
    fn evaluation_script_contains_id_and_nonce() {
        let script = evaluation_script("1 + 1", 42, "testnonce123");
        assert!(script.contains("42"));
        assert!(script.contains("testnonce123"));
        assert!(script.contains("window.chrome.webview.postMessage"));
        assert!(script.contains("1 + 1"));
    }

    #[test]
    fn new_nonce_generates_unique_values() {
        let n1 = new_nonce();
        let n2 = new_nonce();
        assert_ne!(n1, n2);
        assert_eq!(n1.len(), 32);
        assert_eq!(n2.len(), 32);
    }
}
