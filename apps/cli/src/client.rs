use async_trait::async_trait;
use futures::{stream, StreamExt};
use reqwest::{Client, Response};
use ronin_agent_core::*;
use serde::{de::DeserializeOwned, Deserialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct ClientIdentity {
    pub surface: String,
    pub version: String,
    pub platform: String,
    pub arch: String,
}

impl ClientIdentity {
    pub fn cli() -> Self {
        Self {
            surface: "cli".into(),
            version: VERSION.into(),
            platform: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        }
    }

    pub fn desktop(version: impl Into<String>) -> Self {
        Self {
            surface: "desktop".into(),
            version: version.into(),
            platform: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        }
    }
}

impl ClientIdentity {
    pub(crate) fn headers(&self) -> HashMap<String, String> {
        let mut headers = HashMap::from([
            ("x-ronin-client-surface".into(), self.surface.clone()),
            ("x-ronin-client-version".into(), self.version.clone()),
            ("x-ronin-client-platform".into(), self.platform.clone()),
            ("x-ronin-client-arch".into(), self.arch.clone()),
        ]);
        if self.surface == "cli" {
            headers.extend([
                ("x-ronin-cli-version".into(), self.version.clone()),
                ("x-ronin-cli-platform".into(), self.platform.clone()),
                ("x-ronin-cli-arch".into(), self.arch.clone()),
            ]);
        }
        headers
    }
}

#[derive(Debug, Error, Clone)]
#[error("API error ({code}): {message}")]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
}
#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: Option<String>,
    message: Option<serde_json::Value>,
}

#[derive(Clone)]
pub struct RoninApiClient {
    base: String,
    http: Client,
    headers: Arc<dyn HeaderProvider>,
    identity: ClientIdentity,
}
#[async_trait]
pub trait HeaderProvider: Send + Sync {
    async fn headers(&self) -> Result<HashMap<String, String>, ApiError>;
}
impl RoninApiClient {
    pub fn new(base: impl Into<String>, headers: Arc<dyn HeaderProvider>) -> Self {
        Self::new_with_identity(base, headers, ClientIdentity::cli())
    }
    pub fn new_with_identity(
        base: impl Into<String>,
        headers: Arc<dyn HeaderProvider>,
        identity: ClientIdentity,
    ) -> Self {
        Self {
            base: base.into().trim_end_matches('/').into(),
            http: Client::new(),
            headers,
            identity,
        }
    }
    async fn apply(&self, b: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder, ApiError> {
        let mut b = b;
        for (k, v) in self.identity.headers() {
            b = b.header(k, v)
        }
        for (k, v) in self.headers.headers().await? {
            b = b.header(k, v)
        }
        Ok(b)
    }
    async fn json<T: DeserializeOwned>(
        &self,
        path: &str,
        method: reqwest::Method,
        body: Option<&impl serde::Serialize>,
    ) -> Result<T, ApiError> {
        let mut b = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("content-type", "application/json");
        if let Some(v) = body {
            b = b.json(v)
        }
        let response = self.apply(b).await?.send().await.map_err(network)?;
        decode(response).await
    }
    pub async fn health(&self) -> bool {
        match self
            .apply(self.http.get(format!("{}/health", self.base)))
            .await
        {
            Ok(v) => v.send().await.is_ok_and(|r| r.status().is_success()),
            Err(_) => false,
        }
    }
    pub async fn models(&self) -> Result<Vec<ModelSummary>, ApiError> {
        self.json("/models", reqwest::Method::GET, None::<&()>)
            .await
    }
    pub async fn balance(&self) -> Result<BalanceSummary, ApiError> {
        self.json("/credits/balance", reqwest::Method::GET, None::<&()>)
            .await
    }

    pub async fn compatibility(&self) -> Result<(), ApiError> {
        let _: serde_json::Value = self
            .json("/agent/compatibility", reqwest::Method::GET, None::<&()>)
            .await?;
        Ok(())
    }
    pub async fn upsert_agent_session(
        &self,
        metadata: &AgentSessionMetadata,
    ) -> Result<AgentSessionRecord, ApiError> {
        self.json("/agent/sessions", reqwest::Method::POST, Some(metadata))
            .await
    }
    pub async fn agent_sessions(
        &self,
        limit: u8,
        before: Option<&str>,
    ) -> Result<AgentSessionList, ApiError> {
        let mut path = format!("/agent/sessions?limit={}", limit.clamp(1, 100));
        if let Some(before) = before {
            path.push_str("&before=");
            path.push_str(&urlencoding::encode(before));
        }
        self.json(&path, reqwest::Method::GET, None::<&()>).await
    }
    pub async fn delete_agent_session(&self, session_id: &str) -> Result<(), ApiError> {
        let _: serde_json::Value = self
            .json(
                &format!("/agent/sessions/{}", urlencoding::encode(session_id)),
                reqwest::Method::DELETE,
                None::<&()>,
            )
            .await?;
        Ok(())
    }
    async fn stream(
        &self,
        id: String,
        cancel: &CancellationToken,
    ) -> Result<CompletionStream, AgentLoopError> {
        let (client_id, client, cancel) = (id.clone(), self.clone(), cancel.clone());
        let (rx_tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            let mut last = "0".to_string();
            for attempt in 0..5 {
                if cancel.is_cancelled() {
                    return;
                }
                let url = format!(
                    "{}/generations/{}/stream?lastEventId={}",
                    client.base,
                    client_id,
                    urlencoding::encode(&last)
                );
                let builder = match client
                    .apply(client.http.get(url).header("accept", "text/event-stream"))
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = rx_tx.send(Err(loop_error(e))).await;
                        return;
                    }
                };
                let response = match builder.send().await {
                    Ok(v) => v,
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt))).await;
                        continue;
                    }
                };
                if !response.status().is_success() {
                    let error = response_error(response).await;
                    let _ = rx_tx.send(Err(loop_error(error))).await;
                    return;
                }
                let mut chunks = response.bytes_stream();
                let mut buffer = String::new();
                let mut terminal = false;
                while let Some(chunk) = chunks.next().await {
                    if cancel.is_cancelled() {
                        return;
                    }
                    let bytes = match chunk {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(end) = buffer.find("\n\n").or_else(|| buffer.find("\r\n\r\n")) {
                        let frame = buffer[..end].replace("\r\n", "\n");
                        let skip = if buffer[end..].starts_with("\r\n\r\n") {
                            4
                        } else {
                            2
                        };
                        buffer.drain(..end + skip);
                        let mut data = String::new();
                        for line in frame.lines() {
                            if let Some(v) = line.strip_prefix("id:") {
                                last = v.trim().into()
                            }
                            if let Some(v) = line.strip_prefix("data:") {
                                if !data.is_empty() {
                                    data.push('\n')
                                }
                                data.push_str(v.trim_start())
                            }
                        }
                        if data.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<GenerationStreamEvent>(&data) {
                            Ok(event) => {
                                terminal = matches!(
                                    event,
                                    GenerationStreamEvent::Done { .. }
                                        | GenerationStreamEvent::Error { .. }
                                );
                                if rx_tx.send(Ok(event)).await.is_err() {
                                    return;
                                }
                                if terminal {
                                    return;
                                }
                            }
                            Err(_) => continue,
                        }
                    }
                }
                if terminal {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt))).await;
            }
            let _ = rx_tx
                .send(Err(AgentLoopError::new(
                    "stream_disconnected",
                    "Lost the generation stream and could not reconnect",
                    true,
                )))
                .await;
        });
        Ok(CompletionStream {
            generation_id: id,
            events: Box::pin(stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|v| (v, rx))
            })),
        })
    }
}

#[async_trait]
impl CompletionClient for RoninApiClient {
    async fn create_completion(
        &self,
        request: AgentCompletionRequest,
        cancel: &CancellationToken,
    ) -> Result<CompletionStream, AgentLoopError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Created {
            generation_id: String,
        }
        let value: Created = self
            .json("/agent/completions", reqwest::Method::POST, Some(&request))
            .await
            .map_err(loop_error)?;
        self.stream(value.generation_id, cancel).await
    }
    async fn resume_completion(
        &self,
        id: &str,
        cancel: &CancellationToken,
    ) -> Result<CompletionStream, AgentLoopError> {
        self.stream(id.into(), cancel).await
    }
    async fn stop_generation(&self, id: &str) -> Result<(), AgentLoopError> {
        let _: serde_json::Value = self
            .json(
                &format!("/generations/{id}/stop"),
                reqwest::Method::POST,
                None::<&()>,
            )
            .await
            .map_err(loop_error)?;
        Ok(())
    }
}

async fn decode<T: DeserializeOwned>(r: Response) -> Result<T, ApiError> {
    if !r.status().is_success() {
        return Err(response_error(r).await);
    }
    let body = r.bytes().await.map_err(network)?;
    serde_json::from_slice(&body).map_err(|e| ApiError {
        status: 0,
        code: "invalid_response".into(),
        message: e.to_string(),
    })
}
async fn response_error(r: Response) -> ApiError {
    let status = r.status();
    let fallback = status
        .canonical_reason()
        .unwrap_or("HTTP error")
        .to_string();
    let body = r.json::<ErrorBody>().await.ok();
    let message = body
        .as_ref()
        .and_then(|b| b.message.as_ref())
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or(fallback);
    ApiError {
        status: status.as_u16(),
        code: body
            .and_then(|b| b.code)
            .unwrap_or_else(|| format!("http_{}", status.as_u16())),
        message,
    }
}
fn network(e: reqwest::Error) -> ApiError {
    ApiError {
        status: 0,
        code: "network_error".into(),
        message: e.to_string(),
    }
}
fn loop_error(e: ApiError) -> AgentLoopError {
    AgentLoopError::new(e.code, e.message, e.status == 0 || e.status >= 500)
}

pub struct StaticHeaders(pub HashMap<String, String>);
#[async_trait]
impl HeaderProvider for StaticHeaders {
    async fn headers(&self) -> Result<HashMap<String, String>, ApiError> {
        Ok(self.0.clone())
    }
}
