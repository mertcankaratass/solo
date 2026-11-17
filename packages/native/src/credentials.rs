//! Secure credential storage for Solo applications.
//!
//! - Linux: backed by the Secret Service API (GNOME Keyring, KWallet) over D-Bus
//!   via the `oo7` crate (Issue #128). Every item is labeled with the app's identifier
//!   and keyed with `solo-app` and `url` attributes so that separate Solo apps and
//!   Zed credentials never collide or overwrite each other.
//! - macOS: backed by the macOS Keychain (Issue #43).
//!
//! All operations run asynchronously off the UI thread via napi `AsyncTask`
//! so the desktop stays responsive. Secrets are never logged or included in error
//! messages.

use napi::bindgen_prelude::{AsyncTask, Result, Task};
use napi::Error;
use napi_derive::napi;

/// A stored credential record. Secrets are never printed in Debug logs.
#[derive(Clone)]
#[napi(object)]
pub struct CredentialResult {
    pub username: String,
    pub secret: String,
}

impl std::fmt::Debug for CredentialResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialResult")
            .field("username", &self.username)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

fn validate_url(url: &str) -> Result<()> {
    if url.trim().is_empty() {
        return Err(Error::from_reason("Credential URL must not be empty"));
    }
    Ok(())
}

fn validate_username(username: &str) -> Result<()> {
    if username.trim().is_empty() {
        return Err(Error::from_reason("Credential username must not be empty"));
    }
    Ok(())
}

fn resolve_app_id(app_id: Option<String>) -> Result<String> {
    app_id
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| Error::from_reason("Credential operations require an app identifier"))
}

// ---------------------------------------------------------------------------
// Linux / FreeBSD (Secret Service via oo7)
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod linux {
    use super::*;

    fn map_oo7_error(err: oo7::Error, url: &str) -> Error {
        match &err {
            oo7::Error::Dismissed | oo7::Error::Cancelled => {
                Error::from_reason("Keyring unlock prompt was dismissed")
            }
            oo7::Error::NoService => {
                Error::from_reason("No Secret Service provider available on the session bus")
            }
            oo7::Error::ZBus(zbus_err) => {
                let msg = zbus_err.to_string();
                if msg.contains("ServiceUnknown")
                    || msg.contains("org.freedesktop.secrets")
                    || msg.contains("No such interface")
                {
                    Error::from_reason("No Secret Service provider available on the session bus")
                } else {
                    Error::from_reason(format!("Keyring operation failed for {url}: {err}"))
                }
            }
            _ => {
                let msg = err.to_string();
                if msg.contains("ServiceUnknown")
                    || msg.contains("org.freedesktop.secrets")
                    || msg.contains("No such interface")
                    || msg.contains("no secret service")
                {
                    Error::from_reason("No Secret Service provider available on the session bus")
                } else if msg.contains("dismissed")
                    || msg.contains("cancelled")
                    || msg.contains("canceled")
                    || msg.contains("Prompt")
                {
                    Error::from_reason("Keyring unlock prompt was dismissed")
                } else {
                    Error::from_reason(format!("Keyring operation failed for {url}: {msg}"))
                }
            }
        }
    }

    pub fn write(url: &str, username: &str, secret: &[u8], app_id: &str) -> Result<()> {
        futures::executor::block_on(async {
            let keyring = oo7::Keyring::new()
                .await
                .map_err(|err| map_oo7_error(err, url))?;

            keyring
                .unlock()
                .await
                .map_err(|err| map_oo7_error(err, url))?;

            // Replace any existing item matching (solo-app, url) regardless of old username
            let search_attributes = [
                ("solo-app", app_id),
                ("url", url),
            ];
            if let Ok(existing) = keyring.search_items(&search_attributes).await {
                for item in existing {
                    let _ = item.delete().await;
                }
            }

            let label = format!("{app_id} ({url})");
            let attributes = [
                ("solo-app", app_id),
                ("url", url),
                ("username", username),
            ];

            keyring
                .create_item(&label, &attributes, secret, false)
                .await
                .map_err(|err| map_oo7_error(err, url))?;

            Ok(())
        })
    }

    pub fn read(url: &str, app_id: &str) -> Result<Option<CredentialResult>> {
        futures::executor::block_on(async {
            let keyring = oo7::Keyring::new()
                .await
                .map_err(|err| map_oo7_error(err, url))?;

            keyring
                .unlock()
                .await
                .map_err(|err| map_oo7_error(err, url))?;

            let attributes = [
                ("solo-app", app_id),
                ("url", url),
            ];

            let items = keyring
                .search_items(&attributes)
                .await
                .map_err(|err| map_oo7_error(err, url))?;

            for item in items {
                let attrs = match item.attributes().await {
                    Ok(a) => a,
                    Err(_) => continue,
                };

                if attrs.get("solo-app").map(|s| s.as_str()) != Some(app_id) {
                    continue;
                }
                if attrs.get("url").map(|s| s.as_str()) != Some(url) {
                    continue;
                }

                let username = match attrs.get("username") {
                    Some(u) => u.clone(),
                    None => continue,
                };

                if let Err(err) = item.unlock().await {
                    return Err(map_oo7_error(err, url));
                }

                let secret_bytes = item
                    .secret()
                    .await
                    .map_err(|err| map_oo7_error(err, url))?;

                let secret = String::from_utf8(secret_bytes.to_vec())
                    .map_err(|_| Error::from_reason("Stored credential secret is not valid UTF-8"))?;

                return Ok(Some(CredentialResult { username, secret }));
            }

            Ok(None)
        })
    }

    pub fn delete(url: &str, app_id: &str) -> Result<bool> {
        futures::executor::block_on(async {
            let keyring = oo7::Keyring::new()
                .await
                .map_err(|err| map_oo7_error(err, url))?;

            keyring
                .unlock()
                .await
                .map_err(|err| map_oo7_error(err, url))?;

            let attributes = [
                ("solo-app", app_id),
                ("url", url),
            ];

            let items = keyring
                .search_items(&attributes)
                .await
                .map_err(|err| map_oo7_error(err, url))?;

            let mut deleted_any = false;
            for item in items {
                let attrs = match item.attributes().await {
                    Ok(a) => a,
                    Err(_) => continue,
                };

                if attrs.get("solo-app").map(|s| s.as_str()) != Some(app_id) {
                    continue;
                }
                if attrs.get("url").map(|s| s.as_str()) != Some(url) {
                    continue;
                }

                item.delete().await.map_err(|err| map_oo7_error(err, url))?;
                deleted_any = true;
            }

            Ok(deleted_any)
        })
    }
}

// ---------------------------------------------------------------------------
// Tasks for libuv worker pool
// ---------------------------------------------------------------------------

pub struct WriteCredentialsTask {
    url: String,
    username: String,
    secret: String,
    app_id: String,
}

#[napi]
impl Task for WriteCredentialsTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> Result<Self::Output> {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            linux::write(&self.url, &self.username, self.secret.as_bytes(), &self.app_id)
        }
        #[cfg(target_os = "macos")]
        {
            gpui_apple::keychain::write_credentials(&self.url, &self.username, self.secret.as_bytes())
                .map_err(|err| Error::from_reason(format!("Keychain write failed for {}: {err}", self.url)))
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
        {
            Err(Error::from_reason(
                "Secure credential storage is supported only on Linux and macOS",
            ))
        }
    }

    fn resolve(&mut self, _env: napi::Env, _output: Self::Output) -> Result<Self::JsValue> {
        Ok(())
    }
}

pub struct ReadCredentialsTask {
    url: String,
    app_id: String,
}

#[napi]
impl Task for ReadCredentialsTask {
    type Output = Option<CredentialResult>;
    type JsValue = Option<CredentialResult>;

    fn compute(&mut self) -> Result<Self::Output> {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            linux::read(&self.url, &self.app_id)
        }
        #[cfg(target_os = "macos")]
        {
            let result = gpui_apple::keychain::read_credentials(&self.url)
                .map_err(|err| Error::from_reason(format!("Keychain read failed for {}: {err}", self.url)))?;
            Ok(result.and_then(|(username, bytes)| {
                String::from_utf8(bytes).ok().map(|secret| CredentialResult {
                    username,
                    secret,
                })
            }))
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
        {
            Err(Error::from_reason(
                "Secure credential storage is supported only on Linux and macOS",
            ))
        }
    }

    fn resolve(&mut self, _env: napi::Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

pub struct DeleteCredentialsTask {
    url: String,
    app_id: String,
}

#[napi]
impl Task for DeleteCredentialsTask {
    type Output = bool;
    type JsValue = bool;

    fn compute(&mut self) -> Result<Self::Output> {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            linux::delete(&self.url, &self.app_id)
        }
        #[cfg(target_os = "macos")]
        {
            match gpui_apple::keychain::delete_credentials(&self.url) {
                Ok(()) => Ok(true),
                Err(gpui_apple::keychain::KeychainError::Security { status, .. }) if status == -25300 => Ok(false),
                Err(err) => Err(Error::from_reason(format!("Keychain delete failed for {}: {err}", self.url))),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
        {
            Err(Error::from_reason(
                "Secure credential storage is supported only on Linux and macOS",
            ))
        }
    }

    fn resolve(&mut self, _env: napi::Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// Napi exports
// ---------------------------------------------------------------------------

/// Store a secret in the operating system's secure credential store.
/// Rejects with an Error if the OS rejects the write, or off Linux/macOS.
#[napi]
pub fn write_credentials(
    url: String,
    username: String,
    secret: String,
    app_id: Option<String>,
) -> Result<AsyncTask<WriteCredentialsTask>> {
    validate_url(&url)?;
    validate_username(&username)?;
    let app_id = resolve_app_id(app_id)?;
    Ok(AsyncTask::new(WriteCredentialsTask {
        url,
        username,
        secret,
        app_id,
    }))
}

/// Retrieve a secret from the operating system's secure credential store.
/// Returns null when no credential matches `url`.
#[napi]
pub fn read_credentials(
    url: String,
    app_id: Option<String>,
) -> Result<AsyncTask<ReadCredentialsTask>> {
    validate_url(&url)?;
    let app_id = resolve_app_id(app_id)?;
    Ok(AsyncTask::new(ReadCredentialsTask { url, app_id }))
}

/// Remove a secret from the operating system's secure credential store.
/// Returns true if an item was removed, false if no matching credential existed.
#[napi]
pub fn delete_credentials(
    url: String,
    app_id: Option<String>,
) -> Result<AsyncTask<DeleteCredentialsTask>> {
    validate_url(&url)?;
    let app_id = resolve_app_id(app_id)?;
    Ok(AsyncTask::new(DeleteCredentialsTask { url, app_id }))
}
