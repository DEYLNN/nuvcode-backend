use std::sync::Arc;

use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use futures::StreamExt;
use std::convert::Infallible;
use std::time::Instant;

use crate::error::GatewayError;
use crate::services::usage_tracking::{record_error, record_success, StreamRecorder};
use crate::state::AppState;
use crate::types::chat::ChatCompletionRequest;

/// Handle streaming chat completion.
///
/// Provider-agnostic usage tracking: each chunk folds into a
/// `StreamRecorder` (prompt/completion tokens, TTFT on first chunk).
/// After the producer stream is consumed, we persist one row.
/// Provider-API-key-id isn't surfaced yet (TODO).
pub(crate) async fn handle_streaming(
    state: &Arc<AppState>,
    gw_key: &crate::middleware::auth::GatewayKeyInfo,
    provider: &(dyn crate::providers::traits::Provider + Send + Sync),
    provider_id: &str,
    model: &str,
    provider_request: &ChatCompletionRequest,
    start: Instant,
) -> Result<axum::response::Response, GatewayError> {
    let provider_id = provider_id.to_string();
    let model = model.to_string();
    let gateway_key_id = gw_key.key_id.clone();
    let endpoint = "/v1/chat/completions".to_string();
    let state_for_track = state.clone();
    let endpoint_for_track = endpoint.clone();
    let provider_id_for_track = provider_id.clone();
    let model_for_track = model.clone();
    let gateway_key_id_for_track = gateway_key_id.clone();
    let started = start;

    let stream_result = provider.chat_completion_stream(provider_request.clone()).await;

    match stream_result {
        Ok(chat_result) => {
            let used_key_id_for_track: Option<String> = chat_result.used_key_id.clone();
            // Mirror non_streaming.rs: log each failed key attempt as its own error row.
            for failed in &chat_result.failed_keys {
                record_error(
                    &state.usage_tracker,
                    start,
                    &provider_id,
                    &model,
                    &gateway_key_id,
                    Some(&failed.key_id),
                    &endpoint,
                    failed.error.http_status().unwrap_or(503) as i64,
                    &failed.error.to_string(),
                )
                .await;
            }
            // `then` is FnMut — must capture by reference, not move.
            // Track whether we've already persisted the final usage row; the
            // chunk that carries `usage` is the terminal one — write immediately
            // so we never lose token counts to the previous 500 ms-race.
            let recorder_ref = Arc::new(tokio::sync::Mutex::new(StreamRecorder::default()));
            let saved_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let recorder_w = recorder_ref.clone();
            let saved_flag_w = saved_flag.clone();
            let start_for_record = start;
            let state_w = state.clone();
            let provider_id_w = provider_id.clone();
            let model_w = model.clone();
            let gateway_key_id_w = gateway_key_id.clone();
            let endpoint_w = endpoint.clone();
            let started_w = start;
            let key_id_w = used_key_id_for_track.clone();
            let stream = chat_result.stream.then(move |chunk| {
                let recorder_w = recorder_w.clone();
                let saved_flag_w = saved_flag_w.clone();
                let state_w = state_w.clone();
                let provider_id_w = provider_id_w.clone();
                let model_w = model_w.clone();
                let gateway_key_id_w = gateway_key_id_w.clone();
                let endpoint_w = endpoint_w.clone();
                let key_id_w = key_id_w.clone();
                async move {
                    match chunk {
                        Ok(chunk) => {
                            // Borrow option so we don't move `chunk`.
                            let usage = chunk.usage.as_ref();
                            let prompt = usage.map(|u| u.prompt_tokens as i64);
                            let completion = usage.map(|u| u.completion_tokens as i64);
                            let (ttft_ms, last_seen) = {
                                let mut r = recorder_w.lock().await;
                                r.record_chunk(start_for_record, prompt, completion);
                                (r.ttft_ms, (r.prompt_tokens, r.completion_tokens))
                            };
                            // If this chunk carries usage, flush immediately —
                            // it IS the final chunk. Atomic compare-and-swap so
                            // only the first chunk with usage fires the row write.
                            if usage.is_some()
                                && !saved_flag_w.swap(true, std::sync::atomic::Ordering::SeqCst)
                            {
                                let (p, c) = last_seen;
                                tokio::spawn(async move {
                                    record_success(
                                        &state_w.usage_tracker,
                                        started_w,
                                        &provider_id_w,
                                        &model_w,
                                        &gateway_key_id_w,
                                        key_id_w.as_deref(),
                                        &endpoint_w,
                                        crate::services::usage_tracking::CanonicalUsage {
                                            prompt_tokens: p,
                                            completion_tokens: c,
                                        },
                                        ttft_ms,
                                    )
                                    .await;
                                });
                            }
                            let json = serde_json::to_string(&chunk).unwrap_or_default();
                            Ok::<_, Infallible>(Event::default().data(json))
                        }
                        Err(e) => {
                            // Record the mid-stream failure exactly once —
                            // CAS against saved_flag so a request that already
                            // flushed a success row doesn't get a second one.
                            if !saved_flag_w.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                let state_e = state_w.clone();
                                let provider_e = provider_id_w.clone();
                                let model_e = model_w.clone();
                                let gw_e = gateway_key_id_w.clone();
                                let ep_e = endpoint_w.clone();
                                let key_e = key_id_w.clone();
                                let status_e = e.http_status().unwrap_or(502) as i64;
                                let msg_e = e.to_string();
                                tokio::spawn(async move {
                                    record_error(
                                        &state_e.usage_tracker,
                                        started_w,
                                        &provider_e,
                                        &model_e,
                                        &gw_e,
                                        key_e.as_deref(),
                                        &ep_e,
                                        status_e,
                                        &msg_e,
                                    )
                                    .await;
                                });
                            }
                            let err_json = serde_json::json!({
                                "error": {
                                    "message": e.to_string(),
                                    "type": "stream_error",
                                    "code": "stream_error"
                                }
                            });
                            Ok::<_, Infallible>(Event::default().data(err_json.to_string()))
                        }
                    }
                }
            });

            let done = futures::stream::once(async {
                Ok::<_, Infallible>(Event::default().data("[DONE]"))
            });

            let full_stream = stream.chain(done);

            let sse = Sse::new(full_stream)
                .keep_alive(axum::response::sse::KeepAlive::new()
                    .interval(std::time::Duration::from_secs(15))
                    .text("keep-alive"));

            let mut response = sse.into_response();

            response.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-cache, no-transform"),
            );
            response.headers_mut().insert(
                axum::http::header::CONNECTION,
                axum::http::HeaderValue::from_static("keep-alive"),
            );
            response.headers_mut().insert(
                "X-Accel-Buffering",
                axum::http::HeaderValue::from_static("no"),
            );

            Ok(response)
        }
        Err(e) => {
            record_error(
                &state.usage_tracker,
                start,
                &provider_id,
                &model,
                &gateway_key_id,
                e.provider_api_key_id(),
                &endpoint,
                e.http_status().unwrap_or(500) as i64,
                &e.to_string(),
            )
            .await;
            Err(e)
        }
    }
}
