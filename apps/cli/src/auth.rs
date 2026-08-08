use crate::{
    client::{ApiError, ClientIdentity, HeaderProvider},
    storage::*,
};
use async_trait::async_trait;
use chrono::Utc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct CredentialManager {
    api: String,
    home: PathBuf,
    http: Client,
    refresh: Arc<Mutex<()>>,
}
impl CredentialManager {
    pub fn new(api: impl Into<String>, home: impl Into<PathBuf>) -> Self {
        Self {
            api: api.into(),
            home: home.into(),
            http: Client::new(),
            refresh: Arc::new(Mutex::new(())),
        }
    }
    pub async fn logout(&self) -> Result<(), ApiError> {
        let creds = load_credentials_for(&self.home, &self.api);
        if creds.access_token.is_some() {
            let _ = self
                .post::<serde_json::Value>("/auth/logout", None::<&()>, self.headers().await?)
                .await?;
        }
        clear_credentials_for(&self.home, &self.api);
        Ok(())
    }
    pub fn clear_local_credentials(&self) {
        clear_credentials_for(&self.home, &self.api);
    }
    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: Option<&impl Serialize>,
        headers: HashMap<String, String>,
    ) -> Result<T, ApiError> {
        let mut b = self
            .http
            .post(format!("{}{path}", self.api))
            .header("content-type", "application/json");
        for (k, v) in headers {
            b = b.header(k, v)
        }
        if let Some(v) = body {
            b = b.json(v)
        }
        let r = b.send().await.map_err(|e| ApiError {
            status: 0,
            code: "network_error".into(),
            message: e.to_string(),
        })?;
        let status = r.status();
        if !status.is_success() {
            let v = r.json::<serde_json::Value>().await.unwrap_or_default();
            return Err(ApiError {
                status: status.as_u16(),
                code: v
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("auth_error")
                    .into(),
                message: v
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Authentication failed")
                    .into(),
            });
        }
        r.json().await.map_err(|e| ApiError {
            status: 0,
            code: "invalid_response".into(),
            message: e.to_string(),
        })
    }
}
#[async_trait]
impl HeaderProvider for CredentialManager {
    async fn headers(&self) -> Result<HashMap<String, String>, ApiError> {
        let mut creds = load_credentials_for(&self.home, &self.api);
        if let (Some(access), Some(refresh)) =
            (creds.access_token.clone(), creds.refresh_token.clone())
        {
            let expiry = creds.expires_at.or_else(|| access_token_expiry(&access));
            if expiry.is_some_and(|e| e <= Utc::now().timestamp() + 60) {
                let _guard = self.refresh.lock().await;
                creds = load_credentials_for(&self.home, &self.api);
                let expiry = creds
                    .expires_at
                    .or_else(|| creds.access_token.as_deref().and_then(access_token_expiry));
                if expiry.is_some_and(|e| e <= Utc::now().timestamp() + 60) {
                    #[derive(Serialize)]
                    struct Body<'a> {
                        #[serde(rename = "refreshToken")]
                        refresh_token: &'a str,
                    }
                    let tokens: TokenResponse = self
                        .post(
                            "/auth/refresh",
                            Some(&Body {
                                refresh_token: &refresh,
                            }),
                            HashMap::new(),
                        )
                        .await?;
                    creds = credentials(&tokens);
                    save_credentials_for(&self.home, &self.api, &creds).map_err(io_error)?;
                }
            }
        }
        Ok(auth_headers(&creds))
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Device {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}
#[derive(Debug, Deserialize)]
struct User {
    email: String,
    first_name: Option<String>,
    last_name: Option<String>,
    profile_picture_url: Option<String>,
}
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    user: User,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceAuthorizationInfo {
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    pub expires_in: u64,
    pub interval: u64,
}

pub struct DeviceAuthorization {
    manager: CredentialManager,
    home: PathBuf,
    device: Device,
    identity: ClientIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceAuthorizationState {
    Polling,
    SlowDown,
}

impl DeviceAuthorization {
    pub fn info(&self) -> DeviceAuthorizationInfo {
        DeviceAuthorizationInfo {
            verification_uri: self.device.verification_uri.clone(),
            verification_uri_complete: self.device.verification_uri_complete.clone(),
            user_code: self.device.user_code.clone(),
            expires_in: self.device.expires_in,
            interval: self.device.interval.unwrap_or(5),
        }
    }

    pub async fn complete(self, cancel: CancellationToken) -> Result<(), ApiError> {
        self.complete_with(cancel, |_| {}).await
    }

    pub async fn complete_with(
        self,
        cancel: CancellationToken,
        mut on_state: impl FnMut(DeviceAuthorizationState) + Send,
    ) -> Result<(), ApiError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(self.device.expires_in);
        let mut interval = Duration::from_secs(self.device.interval.unwrap_or(5));
        let tokens = loop {
            tokio::select! {
                _ = cancel.cancelled() => return Err(ApiError { status: 0, code: "aborted".into(), message: "Login cancelled".into() }),
                _ = tokio::time::sleep(interval) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ApiError {
                    status: 400,
                    code: "expired_token".into(),
                    message: "The device authorization expired".into(),
                });
            }
            #[derive(Serialize)]
            struct Body<'a> {
                #[serde(rename = "deviceCode")]
                device_code: &'a str,
            }
            on_state(DeviceAuthorizationState::Polling);
            match self
                .manager
                .post::<TokenResponse>(
                    "/auth/device/token",
                    Some(&Body {
                        device_code: &self.device.device_code,
                    }),
                    HashMap::new(),
                )
                .await
            {
                Ok(value) => break value,
                Err(error) if error.code == "authorization_pending" => continue,
                Err(error) if error.code == "slow_down" => {
                    on_state(DeviceAuthorizationState::SlowDown);
                    interval += Duration::from_secs(5);
                    continue;
                }
                Err(error) => return Err(error),
            }
        };
        let creds = credentials(&tokens);
        let mut headers = HashMap::new();
        headers.insert(
            "authorization".into(),
            format!("Bearer {}", tokens.access_token),
        );
        headers.extend(self.identity.headers());
        let name = [
            tokens.user.first_name.as_deref(),
            tokens.user.last_name.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
        let body = serde_json::json!({
            "email": tokens.user.email,
            "name": if name.is_empty() { None } else { Some(name) },
            "avatarUrl": tokens.user.profile_picture_url
        });
        let _: serde_json::Value = self
            .manager
            .post("/auth/session", Some(&body), headers)
            .await?;
        save_credentials_for(&self.home, &self.manager.api, &creds).map_err(io_error)?;
        Ok(())
    }
}

pub async fn begin_device_login(
    api: &str,
    home: &std::path::Path,
    identity: ClientIdentity,
) -> Result<DeviceAuthorization, ApiError> {
    let manager = CredentialManager::new(api, home);
    let device = manager
        .post("/auth/device", None::<&()>, HashMap::new())
        .await?;
    Ok(DeviceAuthorization {
        manager,
        home: home.to_path_buf(),
        device,
        identity,
    })
}
fn credentials(t: &TokenResponse) -> Credentials {
    Credentials {
        access_token: Some(t.access_token.clone()),
        refresh_token: Some(t.refresh_token.clone()),
        expires_at: access_token_expiry(&t.access_token),
        dev_user_id: None,
    }
}
fn auth_headers(c: &Credentials) -> HashMap<String, String> {
    let mut h = HashMap::new();
    if let Some(v) = &c.access_token {
        h.insert("authorization".into(), format!("Bearer {v}"));
    } else if let Some(v) = &c.dev_user_id {
        h.insert("x-user-id".into(), v.clone());
    }
    h
}
fn io_error(e: std::io::Error) -> ApiError {
    ApiError {
        status: 0,
        code: "credentials_io".into(),
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[tokio::test]
    async fn device_login_begin_returns_structured_progress() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            let body = serde_json::json!({
                "device_code": "private-device-code",
                "user_code": "RONIN-42",
                "verification_uri": "https://api.workos.com/device",
                "verification_uri_complete": "https://api.workos.com/device?code=RONIN-42",
                "expires_in": 600,
                "interval": 3
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            ).unwrap();
        });
        let home = tempfile::tempdir().unwrap();
        let login = begin_device_login(
            &format!("http://{address}"),
            home.path(),
            ClientIdentity::desktop("test"),
        )
        .await
        .unwrap();
        let info = login.info();
        assert_eq!(info.user_code, "RONIN-42");
        assert_eq!(info.interval, 3);
        assert!(!serde_json::to_string(&info)
            .unwrap()
            .contains("private-device-code"));
    }
}
