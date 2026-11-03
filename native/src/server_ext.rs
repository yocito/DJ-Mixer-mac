//! おまかせ・書き出しの API（djmix/server.py の /api/auto/*・/api/export/*・/ws/record）。
use crate::export::{render, writer};
use crate::planner::engine::{self, SegSpec};
use crate::query_util::*;
use crate::server::{bad, blocking, ApiError, ApiResult, AppState, St, Q};
use axum::extract::RawQuery;
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
    r.route("/api/auto/plan", post(auto_plan))
        .route("/api/auto/next", get(auto_next))
        .route("/api/auto/albums", get(auto_albums))
        .route("/api/auto/album-tracks", get(auto_album_tracks))
        .route("/api/auto/rejoin", post(auto_rejoin))
        .route("/api/auto/segments", get(auto_segments))
        .route("/api/auto/facets", get(|| async { blocking(|| Ok(Json(engine::candidate_facets()))).await }))
        .route("/api/export/render", post(export_render))
        .route("/api/export/status", get(export_status))
        .route("/api/export/segments", get(|| async { Json(json!({"segments": []})) }))
        .route("/api/export/segments/:id", post(export_segment_audio))
        .route("/api/export/cancel", post(export_cancel))
        .route("/api/export/default-dir", get(|| async { Json(json!({"folder": writer::default_dir().to_string_lossy()})) }))
        .route("/ws/record", get(ws_record))
}

// ---- おまかせ ----
const POSITIONS: [&str; 6] = ["auto", "intro", "head", "peak", "tail", "full"];

fn plan_err(e: engine::PlanError) -> ApiError {
    bad(e.0)
}

fn join_mode(b: &Value) -> &'static str {
    match b["join_mode"].as_str() {
        Some("mix") => "mix",
        Some("cut") => "cut",
        _ => "auto",
    }
}

/// 流れ: 自分で描いた線（2 点以上）なら custom、知らない値は up
fn flow_of(b: &Value, curve: &[f64]) -> String {
    let f = b["flow"].as_str().unwrap_or("");
    if f == "custom" && curve.len() >= 2 {
        "custom".into()
    } else if engine::FLOWS.contains(&f) {
        f.into()
    } else {
        "up".into()
    }
}

/// mode=library: {seed, groups, minutes, flow, avoid} → 3 案。mode=queue: {ids} → [おまかせの並び, 今の並び]。mode=albums: アルバムで選ぶ。
async fn auto_plan(Json(b): Json<Value>) -> ApiResult {
    blocking(move || {
        let jm = join_mode(&b);
        let prefs = crate::settings::load();
        if b["mode"] == "queue" {
            return engine::plan_queue(&ints(&b["ids"]), jm).map(Json).map_err(plan_err);
        }
        let curve: Vec<f64> = floats(&b["curve"]).into_iter().take(200).collect();
        let flow = flow_of(&b, &curve);
        let tl = num(&b["track_len"]).filter(|x| *x != 0.0);
        let pos = b["track_pos"].as_str().filter(|p| POSITIONS.contains(p)).unwrap_or("auto").to_string();
        let minutes = num(&b["minutes"]).unwrap_or(40.0).clamp(5.0, 600.0);
        let avoid = ints(&b["avoid"]);
        let cv = (!curve.is_empty()).then_some(curve.as_slice());
        if b["mode"] == "albums" {
            let albums: Vec<String> = b["albums"].as_array().map(|a| a.iter().map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string())).collect()).unwrap_or_default();
            let seed = num(&b["seed"]).filter(|x| *x != 0.0).map(|x| x as i64);
            let use_all = truthy(&b["use_all"]);
            return engine::plan_albums(&albums, minutes, &flow, seed, use_all, &avoid, cv, Some(&prefs), tl, &pos, jm)
                .map(Json)
                .map_err(plan_err);
        }
        let Some(seed) = num(&b["seed"]).map(|x| x as i64) else { return Err(bad("'seed'")) };
        let groups: Vec<String> = b["groups"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        let pls = ints(&b["playlists"]);
        engine::plan_library(seed, &groups, minutes, &flow, &avoid, cv, Some(&prefs), (!pls.is_empty()).then_some(pls.as_slice()), tl, &pos, jm)
            .map(Json)
            .map_err(plan_err)
    })
    .await
}

/// 次の曲の候補。prev の次に合う曲、next もあれば差し替え（前後の相性の平均）。artist（いくつでも）で絞る。
async fn auto_next(RawQuery(raw): RawQuery) -> ApiResult {
    let q = Q::parse(raw);
    blocking(move || {
        let ex: Vec<i64> = q.str("exclude").split(',').filter_map(|x| x.trim().parse().ok()).collect();
        let n = q.i64("n").unwrap_or(8).clamp(1, 20) as usize;
        let prefs = crate::settings::load();
        let items = engine::next_candidates(q.i64("prev"), q.i64("next"), &q.list("groups"), &ex, n, Some(&prefs), &q.all("artist"));
        Ok(Json(json!({"items": items})))
    })
    .await
}

async fn auto_albums(RawQuery(raw): RawQuery) -> ApiResult {
    let q = Q::parse(raw).str("q").trim().to_string();
    blocking(move || Ok(Json(json!({"items": crate::library::query::album_list(&q)?})))).await
}

async fn auto_album_tracks(RawQuery(raw): RawQuery) -> ApiResult {
    let albums = Q::parse(raw).all("album");
    blocking(move || Ok(Json(json!({"items": crate::library::query::album_tracks(&albums)?})))).await
}

/// 結果の曲ごとの区間を変えて、同じ並びで繋ぎを計算し直す。{tracks: [{id, len, pos, t0, t1, jm}]}
async fn auto_rejoin(Json(b): Json<Value>) -> ApiResult {
    blocking(move || {
        let spec: Vec<SegSpec> = b["tracks"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| {
                        let pos = t["pos"].as_str().filter(|p| POSITIONS.contains(p) || *p == "custom").unwrap_or("auto");
                        Some(SegSpec {
                            id: num(&t["id"])? as i64,
                            len: num(&t["len"]).filter(|x| *x != 0.0),
                            pos: Some(pos.to_string()),
                            t0: num(&t["t0"]),
                            t1: num(&t["t1"]),
                            jm: t["jm"].as_str().filter(|m| *m == "cut" || *m == "mix").map(String::from),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let prefs = crate::settings::load();
        engine::rejoin(&spec, Some(&prefs), join_mode(&b)).map(Json).map_err(plan_err)
    })
    .await
}

/// 1 曲の区間の候補。len は 1 曲あたりの長さ（秒）。
async fn auto_segments(RawQuery(raw): RawQuery) -> ApiResult {
    let q = Q::parse(raw);
    let Some(id) = q.i64("id") else { return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, "id がありません".into())) };
    let len = q.f64("len").filter(|x| *x != 0.0);
    blocking(move || Ok(Json(json!({"items": engine::segment_choices(id, len)})))).await
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
