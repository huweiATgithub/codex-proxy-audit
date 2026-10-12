//! Optional request metadata from the host's public detail API, never from host SQL.
use std::time::Duration;

use gateway_plugin_sdk::{
    call::middleware::http::Version,
    client::{HostClient, HttpBody, HttpFrame, HttpRequest},
};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::time::{Instant, timeout, timeout_at};

const HOST_WAIT: Duration = Duration::from_secs(5);
const CLOSE_WAIT: Duration = Duration::from_millis(100);
const MAX_HOST_RESPONSE_BYTES: usize = 512 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("host request metadata unavailable or invalid")]
pub(crate) struct RequestMetadataError;

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RequestMetadata {
    user_agent: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
}

#[derive(Deserialize)]
struct HostEnvelope {
    code: u32,
    message: String,
    data: HostRequestDetail,
}

// This is a projection of the public response: unrelated host fields may evolve.
// Nullable fields must still exist, so malformed responses cannot look like an
// existing record with missing metadata.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostRequestDetail {
    id: String,
    request_id: String,
    #[serde(deserialize_with = "nullable_string")]
    client_api_key_id: Option<String>,
    #[serde(deserialize_with = "nullable_string")]
    user_agent: Option<String>,
    #[serde(deserialize_with = "nullable_string")]
    requested_model: Option<String>,
    #[serde(deserialize_with = "nullable_string")]
    reasoning_effort: Option<String>,
}

fn nullable_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

impl RequestMetadata {
    pub(crate) async fn fetch(
        host: &HostClient,
        request_id: Option<&str>,
        key_id: &str,
    ) -> Result<Option<Self>, RequestMetadataError> {
        let Some(request_id) = request_id else {
            return Ok(None);
        };
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("id", request_id)
            .finish();
        let deadline = Instant::now() + HOST_WAIT;
        let mut response = timeout_at(
            deadline,
            host.dispatch_http(HttpRequest {
                settings: serde_json::Value::Null,
                method: "GET".into(),
                uri: format!("/api/admin/usage/records/detail?{query}"),
                version: Version::Http11,
                headers: Vec::new(),
                timeout_ms: Some(HOST_WAIT.as_millis() as u64),
                body: HttpBody::empty(),
            }),
        )
        .await
        .map_err(|_| RequestMetadataError)?
        .map_err(|_| RequestMetadataError)?;

        let result = if response.status == 404 {
            Ok(None)
        } else if response.status != 200 {
            Err(RequestMetadataError)
        } else {
            match timeout_at(deadline, read_body(&mut response.body)).await {
                Ok(Ok(bytes)) => Self::parse(&bytes, request_id, key_id).map(Some),
                _ => Err(RequestMetadataError),
            }
        };
        // EOF releases the handle in the SDK. Early status/size/read failures
        // explicitly close it while the parent management call is still alive.
        timeout(CLOSE_WAIT, response.body.close())
            .await
            .map_err(|_| RequestMetadataError)?
            .map_err(|_| RequestMetadataError)?;
        result
    }

    fn parse(bytes: &[u8], request_id: &str, key_id: &str) -> Result<Self, RequestMetadataError> {
        let envelope: HostEnvelope =
            serde_json::from_slice(bytes).map_err(|_| RequestMetadataError)?;
        if envelope.code != 200
            || envelope.message.is_empty()
            || envelope.data.id != request_id
            || envelope.data.request_id != request_id
            || envelope.data.client_api_key_id.as_deref() != Some(key_id)
        {
            return Err(RequestMetadataError);
        }
        let detail = envelope.data;
        Ok(Self {
            user_agent: bounded(detail.user_agent, 8192),
            model: bounded(detail.requested_model, 256),
            reasoning_effort: bounded(detail.reasoning_effort, 80),
        })
    }
}

async fn read_body(body: &mut HttpBody) -> Result<Vec<u8>, RequestMetadataError> {
    let mut bytes = Vec::new();
    while let Some(frame) = body.read().await.map_err(|_| RequestMetadataError)? {
        if let HttpFrame::Data(data) = frame {
            if data.len() > MAX_HOST_RESPONSE_BYTES.saturating_sub(bytes.len()) {
                return Err(RequestMetadataError);
            }
            bytes.extend(data);
        }
    }
    Ok(bytes)
}

fn bounded(value: Option<String>, limit: usize) -> Option<String> {
    value.filter(|value| value.chars().count() <= limit)
}

#[cfg(test)]
mod tests;
