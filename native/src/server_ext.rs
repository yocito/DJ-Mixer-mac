//! おまかせ・書き出しの API（djmix/server.py の /api/auto/*・/api/export/*・/ws/record）。
use crate::export::{render, writer};
use crate::server::{blocking, ApiError, ApiResult, AppState, St};
use crate::settings;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::sync::Arc;

pub fn routes(r: Router<Arc<AppState>>) -> Router<Arc<AppState>> {
    r.route("/api/export/render", post(export_render))
        .route("/api/export/status", get(export_status))
        .route("/api/export/segments", get(|| async { Json(json!({"segments": []})) }))
        .route("/api/export/segments/:id", post(export_segment_audio))
        .route("/api/export/cancel", post(export_cancel))
        .route("/api/export/default-dir", get(|| async { Json(json!({"folder": writer::default_dir().to_string_lossy()})) }))
        .route("/ws/record", get(ws_record))
}

/// 高速で書き出す。{items, settings, format, folder, name, tracklist}。画面は engine: 'browser' を送るが、
/// Mac 版はストレッチもこちら（ネイティブの Signalsmith）で計算するので使わない。
async fn export_render(Json(body): Json<Value>) -> ApiResult {
    blocking(move || {
        let items: Vec<Map<String, Value>> =
            body["items"].as_array().map(|a| a.iter().filter_map(|x| x.as_object().cloned()).collect()).unwrap_or_default();
        if items.is_empty() {
            return Err(crate::server::bad("ミックスキューが空です"));
        }
        let fmt = writer::normalize_format(body["format"].as_str());
        let mut st = settings::load();
        if let Some(o) = body["settings"].as_object() {
            for (k, v) in o {
                st.insert(k.clone(), v.clone());
            }
        }
        let out = writer::target_path(body["folder"].as_str().filter(|s| !s.is_empty()), body["name"].as_str(), fmt)?;
        let tl = body["tracklist"].as_bool().unwrap_or(true);
        let job = render::start(items, st, out.clone(), fmt, tl).map_err(|e| ApiError(StatusCode::CONFLICT, e.to_string()))?;
        let mut v = job.status();
        v["path"] = json!(out.to_string_lossy());
        Ok(Json(v))
    })
    .await
}

async fn export_status() -> Json<Value> {
    Json(match render::current() {
        Some(j) => j.status(),
        None => json!({"progress": 0, "done": false, "error": null, "result": null}),
    })
}

async fn export_segment_audio(Path(_id): Path<i64>) -> ApiResult {
    Err(ApiError(StatusCode::CONFLICT, "受け取れる区間がありません".into()))
}

async fn export_cancel() -> Json<Value> {
    render::cancel();
    Json(json!({"ok": true}))
}

// ---- 再生しながら録音する ----
async fn ws_record(_s: St, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(record_socket)
}

/// 画面から: 最初に {folder, name, format, sampleRate} → 16bit ステレオの PCM（バイナリ）を続けて →
/// 最後に {end: true, tracklist: [...]}。応答: {ready, path} → {ok, path, tracklist, seconds} または {error}。
async fn record_socket(mut ws: WebSocket) {
    let mut sink: Option<writer::WavSink> = None;
    let r: anyhow::Result<()> = async {
        let head: Value = match ws.recv().await {
            Some(Ok(Message::Text(t))) => serde_json::from_str(&t)?,
            _ => return Ok(()),
        };
        let fmt = writer::normalize_format(head["format"].as_str());
        let out = writer::target_path(head["folder"].as_str().filter(|s| !s.is_empty()), head["name"].as_str(), fmt)?;
        let sr = head["sampleRate"].as_u64().unwrap_or(44100) as u32;
        sink = Some(writer::WavSink::new(&out, fmt, sr)?);
        ws.send(Message::Text(json!({"ready": true, "path": out.to_string_lossy()}).to_string())).await?;
        while let Some(msg) = ws.recv().await {
            match msg? {
                Message::Binary(b) => {
                    if let Some(s) = sink.as_mut() {
                        s.write(&b)?;
                    }
                }
                Message::Text(t) => {
                    let end: Value = serde_json::from_str(&t)?;
                    if end["end"].as_bool().unwrap_or(false) {
                        let s = sink.take().unwrap();
                        let tl = end["tracklist"].as_array().cloned();
                        let res = tokio::task::spawn_blocking(move || s.close(tl.as_deref())).await??;
                        let mut v = json!({"ok": true});
                        if let (Some(o), Some(r)) = (v.as_object_mut(), res.as_object()) {
                            o.extend(r.clone());
                        }
                        ws.send(Message::Text(v.to_string())).await?;
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = r {
        log::error!("recording failed: {e:#}");
        let _ = ws.send(Message::Text(json!({"error": crate::util::truncate_chars(&e.to_string(), 300)}).to_string())).await;
    }
    if let Some(s) = sink.take() {
        // 途中で切れた: そこまでの分を保存する
        let _ = tokio::task::spawn_blocking(move || s.close(None)).await;
    }
}
