//! 在 SDK 解析前提取流内错误事实，避免业务错误被降为解析失败或静默结束。

use std::sync::Mutex;

use genai::chat::{ChatFrameSink, FrameCtx, RawFrameRef};
use serde::Deserialize;
use serde_json::Value;

use crate::ProviderError;

#[derive(Default)]
pub(crate) struct StreamErrorCapture(Mutex<CapturedState>);

#[derive(Default)]
struct CapturedState {
    error: Option<ProviderError>,
    response_id: Option<String>,
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: Option<Value>,
    response: Option<ResponseError>,
    #[serde(rename = "type")]
    kind: Option<String>,
    code: Option<Value>,
    message: Option<String>,
    request_id: Option<String>,
}

#[derive(Deserialize)]
struct ResponseError {
    id: Option<String>,
    error: Option<Value>,
}

impl StreamErrorCapture {
    pub(crate) fn capture(&self, data: &str) {
        // 不缓存原帧；正常 output 字段由 serde 跳过。异常巨帧仍走适配器的大小限制。
        if data.len() > 64 * 1024 {
            return;
        }
        let Ok(envelope) = serde_json::from_str::<ErrorEnvelope>(data) else {
            return;
        };
        let Ok(mut state) = self.0.lock() else {
            return;
        };
        // 响应身份通常只出现在 created 帧；只保留有界、脱敏的身份，不缓存完整响应。
        if state.response_id.is_none() {
            state.response_id = envelope.response.as_ref().and_then(|response| {
                response
                    .id
                    .as_ref()
                    .filter(|id| id.len() <= 512)
                    .map(|id| crate::sanitize_provider_error(id))
            });
        }
        let error = envelope
            .error
            .or_else(|| envelope.response.and_then(|response| response.error))
            .or_else(|| {
                (envelope.kind.as_deref() == Some("error")).then(|| {
                    serde_json::json!({
                        "code": envelope.code, "message": envelope.message,
                    })
                })
            });
        let Some(error) = error else {
            return;
        };
        let value = serde_json::json!({"error": error, "request_id": envelope.request_id});
        if state.error.is_none() {
            let error = crate::provider_error_from_value(&value, Some(200), &Default::default());
            let mut metadata = error.metadata().cloned().unwrap_or_default();
            metadata.upstream_response_id = state.response_id.clone();
            state.error = Some(error.with_metadata(metadata));
        }
    }

    pub(crate) fn error(&self) -> Option<ProviderError> {
        self.0.lock().ok().and_then(|state| state.error.clone())
    }
}

impl ChatFrameSink for StreamErrorCapture {
    fn on_frame(&self, _: &FrameCtx, frame: RawFrameRef<'_>) {
        self.capture(frame.data);
    }
}
