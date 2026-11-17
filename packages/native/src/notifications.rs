//! System notifications in the macOS Notification Center, through GPUI's
//! `UNUserNotificationCenter` support (`App::show_system_notification`), and on
//! Linux via the D-Bus Desktop Notifications specification (`notify-rust`).
//!
//! GPUI asks for authorization the first time a notification is posted and
//! delivers nothing once the user has denied it. It also disables
//! notifications on macOS in a process without a bundle identifier, because
//! `UNUserNotificationCenter` aborts outside an app bundle; Solo checks the
//! same condition first and throws instead of posting nothing.
//!
//! On Linux, notifications are posted over the session bus to the running
//! desktop notification daemon. If no daemon owns `org.freedesktop.Notifications`,
//! posting fails immediately with an error rather than silently dropping.
//!
//! Responses (a click on the body or on an action button) reach JS as an
//! element-0 `notificationResponse` event whose `value` is
//! `{"tag":…,"actionId":…}`. The observer is installed during startup,
//! so a response that launched the app waits here until a renderer is
//! initialized, as opened URLs do (`open_urls.rs`).

#![cfg_attr(
    not(any(target_os = "macos", target_os = "linux", target_os = "freebsd")),
    allow(dead_code)
)]

use napi::{Error, Result};
use napi_derive::napi;

/// A button offered on a notification.
#[napi(object)]
pub struct NotificationAction {
    /// Reported back as the response's `actionId`.
    pub id: String,
    /// The button's label.
    pub label: String,
}

/// What `showNotification` posts.
#[napi(object)]
pub struct NotificationOptions {
    /// Identifies the notification: posting the same tag replaces it, and
    /// responses carry it back.
    pub tag: String,
    pub title: String,
    pub body: Option<String>,
    pub actions: Option<Vec<NotificationAction>>,
}

/// Element-0 event: the user clicked a notification or one of its buttons.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
const NOTIFICATION_RESPONSE_EVENT: &str = "notificationResponse";

/// Check the options and build GPUI's notification from them.
fn system_notification(options: NotificationOptions) -> Result<gpui::SystemNotification> {
    let NotificationOptions { tag, title, body, actions } = options;
    if tag.is_empty() {
        return Err(Error::from_reason("A notification needs a non-empty tag"));
    }
    if title.is_empty() {
        return Err(Error::from_reason("A notification needs a non-empty title"));
    }
    let actions = actions.unwrap_or_default();
    let mut ids = std::collections::HashSet::new();
    for action in &actions {
        if action.id.is_empty() || action.label.is_empty() {
            return Err(Error::from_reason(
                "A notification action needs a non-empty id and label",
            ));
        }
        if !ids.insert(action.id.as_str()) {
            return Err(Error::from_reason(format!(
                "Notification action ids must be unique; {:?} repeats",
                action.id
            )));
        }
    }
    Ok(gpui::SystemNotification {
        tag: tag.into(),
        title: title.into(),
        body: body.unwrap_or_default().into(),
        actions: actions
            .into_iter()
            .map(|action| gpui::SystemNotificationAction {
                id: action.id.into(),
                label: action.label.into(),
            })
            .collect(),
    })
}

#[cfg(target_os = "macos")]
thread_local! {
    /// Main thread only, where GPUI hands over responses and `init` runs.
    static PENDING: std::cell::RefCell<Vec<gpui::SystemNotificationResponse>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
static PENDING: std::sync::Mutex<Vec<gpui::SystemNotificationResponse>> =
    std::sync::Mutex::new(Vec::new());

/// Report notification responses to the current app renderer for the life
/// of the process. Installed once at startup, so it is in place
/// when a click on a notification launches the app.
#[cfg(target_os = "macos")]
pub(crate) fn observe(cx: &mut gpui::App) {
    cx.on_system_notification_response(|response, _cx| {
        PENDING.with(|pending| pending.borrow_mut().push(response));
        deliver_pending();
    });
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) fn observe(cx: &mut gpui::App) {
    cx.on_system_notification_response(|response, _cx| {
        PENDING.lock().unwrap().push(response);
        deliver_pending();
    });
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "freebsd")))]
pub(crate) fn observe(_cx: &mut gpui::App) {}

/// Hand any held responses to the app renderer, if one is initialized.
/// Called on arrival and after `init`, including a `solo dev` reload's.
#[cfg(target_os = "macos")]
pub(crate) fn deliver_pending() {
    let Some(callback) = crate::renderer::app_event_callback() else {
        return;
    };
    let responses = PENDING.with(|pending| std::mem::take(&mut *pending.borrow_mut()));
    deliver_responses(&callback, responses);
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) fn deliver_pending() {
    let Some(callback) = crate::renderer::app_event_callback() else {
        return;
    };
    let responses = std::mem::take(&mut *PENDING.lock().unwrap());
    deliver_responses(&callback, responses);
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "freebsd")))]
pub(crate) fn deliver_pending() {}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
fn deliver_responses(
    callback: &crate::renderer::EventCallback,
    responses: Vec<gpui::SystemNotificationResponse>,
) {
    for response in responses {
        let value = serde_json::json!({
            "tag": response.tag.as_ref(),
            "actionId": response.action_id.as_ref().map(|id| id.as_ref()),
        })
        .to_string();
        crate::renderer::emit_event_full(
            &Some(callback.clone()),
            0,
            NOTIFICATION_RESPONSE_EVENT,
            |payload| payload.value = Some(value),
        );
    }
}

/// macOS delivers notifications only to an app bundle; GPUI turns them off
/// without one, using the same test.
#[cfg(target_os = "macos")]
fn require_app_bundle(action: &str) -> Result<()> {
    if objc2_foundation::NSBundle::mainBundle()
        .bundleIdentifier()
        .is_none()
    {
        return Err(Error::from_reason(format!(
            "{action} needs a packaged .app with a CFBundleIdentifier: macOS delivers notifications only to app bundles"
        )));
    }
    Ok(())
}

/// Check that a desktop notification daemon owns `org.freedesktop.Notifications`
/// on the D-Bus session bus.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn check_notification_daemon() -> Result<()> {
    notify_rust::get_server_information().map_err(|error| {
        Error::from_reason(format!(
            "No notification daemon available on the session bus: {error}"
        ))
    })?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "freebsd")))]
fn unsupported(action: &str) -> Error {
    Error::from_reason(format!("{action} is supported only on macOS and Linux"))
}

/// Post a notification to the Notification Center. Posting a tag that is
/// already shown replaces that notification. The first post asks the user for
/// permission; once denied, nothing is shown.
#[napi]
pub fn show_notification(options: NotificationOptions) -> Result<()> {
    let notification = system_notification(options)?;
    #[cfg(target_os = "macos")]
    {
        require_app_bundle("showNotification")?;
        crate::application::with_app("showNotification", move |cx| {
            cx.show_system_notification(notification)
        })
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        check_notification_daemon()?;
        crate::renderer::with_ui_app("showNotification", move |cx| {
            cx.show_system_notification(notification)
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "freebsd")))]
    {
        let _ = notification;
        Err(unsupported("showNotification"))
    }
}

/// Remove the notification posted with `tag`, shown or still pending.
#[napi]
pub fn dismiss_notification(tag: String) -> Result<()> {
    if tag.is_empty() {
        return Err(Error::from_reason("A notification needs a non-empty tag"));
    }
    #[cfg(target_os = "macos")]
    {
        require_app_bundle("dismissNotification")?;
        crate::application::with_app("dismissNotification", move |cx| {
            cx.dismiss_system_notification(&tag)
        })
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        check_notification_daemon()?;
        crate::renderer::with_ui_app("dismissNotification", move |cx| {
            cx.dismiss_system_notification(&tag)
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "freebsd")))]
    {
        Err(unsupported("dismissNotification"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(actions: &[(&str, &str)]) -> NotificationOptions {
        NotificationOptions {
            tag: "download-1".into(),
            title: "Download finished".into(),
            body: None,
            actions: Some(
                actions
                    .iter()
                    .map(|(id, label)| NotificationAction {
                        id: id.to_string(),
                        label: label.to_string(),
                    })
                    .collect(),
            ),
        }
    }

    fn reason(options: NotificationOptions) -> String {
        match system_notification(options) {
            Ok(_) => panic!("expected an error"),
            Err(error) => error.reason.clone(),
        }
    }

    #[test]
    fn builds_the_gpui_notification() {
        let notification = system_notification(options(&[("open", "Open"), ("show", "Show in Finder")]))
            .expect("valid options");
        assert_eq!(notification.tag.as_ref(), "download-1");
        assert_eq!(notification.title.as_ref(), "Download finished");
        assert_eq!(notification.body.as_ref(), "");
        let ids: Vec<&str> = notification.actions.iter().map(|a| a.id.as_ref()).collect();
        assert_eq!(ids, ["open", "show"]);
        assert_eq!(notification.actions[1].label.as_ref(), "Show in Finder");
    }

    #[test]
    fn rejects_unusable_options() {
        let mut empty_tag = options(&[]);
        empty_tag.tag.clear();
        assert!(reason(empty_tag).contains("non-empty tag"));
        let mut empty_title = options(&[]);
        empty_title.title.clear();
        assert!(reason(empty_title).contains("non-empty title"));
        assert!(reason(options(&[("", "Open")])).contains("non-empty id and label"));
        assert!(reason(options(&[("open", "")])).contains("non-empty id and label"));
        assert!(reason(options(&[("open", "Open"), ("open", "Again")])).contains("\"open\" repeats"));
    }
}
