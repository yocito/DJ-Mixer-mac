//! HTTP サーバー（djmix/server.py）。画面（frontend/）の配信と API を 127.0.0.1 で提供する。
//! エンドポイントと JSON の形は Python 版と同じ（画面をそのまま使うため）。
use crate::analysis::bulk::BulkAnalyzer;
use crate::analysis::worker::{request_stems, AnalysisWorker, PRIO_MANUAL};
use crate::analysis::{cache, cutpoints, sections, stems};
use crate::audio::pcm::PcmStore;
use crate::library::query::{self, SearchOpts};
use crate::library::scanner::Scanner;
use crate::library::{itunes, roots};
use crate::midi::MidiBridge;
use crate::planner::genres;
use crate::queues::{self, QueueItem, CURRENT};
use crate::{config, db, edits, mixes, prefs, settings, util};
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub type DialogFn = Arc<dyn Fn() -> Option<Option<String>> + Send + Sync>;

/// アプリの状態（Python 版のモジュール変数 scanner・worker・bulk・pcm_store・midi）
pub struct AppState {
    pub scanner: Scanner,
    pub worker: AnalysisWorker,
    pub bulk: BulkAnalyzer,
    pub pcm: PcmStore,
    pub midi: MidiBridge,
    /// フォルダ選択（専用ウィンドウのときだけ。None なら使えない）。戻り値 None = 開けない、Some(None) = キャンセル
    pub dialog: parking_lot::Mutex<Option<DialogFn>>,
}

pub type St = State<Arc<AppState>>;

impl AppState {
    pub fn new() -> Arc<AppState> {
        let worker = AnalysisWorker::new();
        Arc::new(AppState {
            scanner: Scanner::new(roots::configured()),
            bulk: BulkAnalyzer::new(worker.clone()),
            worker,
            pcm: PcmStore::default(),
            midi: MidiBridge::new(),
            dialog: parking_lot::Mutex::new(None),
        })
    }

    /// 起動時の処理（Python 版の lifespan）
    pub fn startup(self: &Arc<Self>) {
        let _ = db::tx(|c| {
            // 解析アルゴリズムを更新した（ANALYSIS_VERSION が変わった）曲は未解析に戻す
            c.execute(
                "UPDATE tracks SET analysis_status = 'none' WHERE analysis_status = 'done' AND \
                 (analysis_version IS NULL OR analysis_version != ?)",
                [cache::ANALYSIS_VERSION],
            )?;
            // 前回の終了時に解析待ち・解析中だった曲も未解析に戻す（キューは画面側が依頼し直す）
            c.execute("UPDATE tracks SET analysis_status = 'none' WHERE analysis_status IN ('queued', 'running')", [])?;
            Ok(())
        });
        let _ = roots::backfill();
        let _ = self.bulk.recover();
        if std::env::var("DJMIX_NO_ITUNES").as_deref() != Ok("1") {
            itunes::start_watch(300);
        }
        if std::env::var("DJMIX_NO_SCAN").as_deref() != Ok("1") {
            self.scanner.start();
        }
        if std::env::var("DJMIX_NO_MIDI").as_deref() != Ok("1") {
            self.midi.start();
        }
        // おまかせ用の要約が無い解析済みの曲の要約を作る
        std::thread::Builder::new()
            .name("desc-backfill".into())
            .spawn(|| {
                if let Err(e) = crate::stubs::desc::backfill() {
                    log::warn!("desc backfill: {e:#}");
                }
            })
            .ok();
    }

    pub fn shutdown(&self) {
        self.bulk.shutdown();
    }
}

// ---------------------------------------------------------------- エラー（FastAPI の HTTPException と同じ {"detail": …}）
pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"detail": self.1}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        log::error!("api error: {e:#}");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, util::truncate_chars(&format!("{e:#}"), 300))
    }
}

pub type ApiResult<T = Json<Value>> = Result<T, ApiError>;

pub fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}
pub fn not_found(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, msg.into())
}

/// 同期の処理（DB・ファイル）をスレッドで実行する
pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> ApiResult<T> + Send + 'static) -> ApiResult<T> {
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) => Err(ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("内部エラー: {e}"))),
    }
}

// ---------------------------------------------------------------- クエリ文字列
#[derive(Default, Debug)]
pub struct Q(pub Vec<(String, String)>);

impl Q {
    pub fn parse(raw: Option<String>) -> Q {
        Q(form_urlencoded::parse(raw.unwrap_or_default().as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
    }
    pub fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().rev().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }
    pub fn all(&self, k: &str) -> Vec<String> {
        self.0.iter().filter(|(a, _)| a == k).map(|(_, v)| v.clone()).collect()
    }
    pub fn str(&self, k: &str) -> String {
        self.get(k).unwrap_or("").to_string()
    }
    pub fn opt_str(&self, k: &str) -> Option<String> {
        self.get(k).map(|s| s.to_string())
    }
    pub fn i64(&self, k: &str) -> Option<i64> {
        self.get(k).and_then(|v| v.trim().parse::<i64>().ok().or_else(|| v.trim().parse::<f64>().ok().map(|f| f as i64)))
    }
    pub fn f64(&self, k: &str) -> Option<f64> {
        self.get(k).and_then(|v| v.trim().parse().ok())
    }
    pub fn bool(&self, k: &str) -> Option<bool> {
        self.get(k).map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
    }
    pub fn list(&self, k: &str) -> Vec<String> {
        self.str(k).split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    }
}

fn body_obj(v: Option<Json<Value>>) -> Map<String, Value> {
    v.and_then(|Json(v)| v.as_object().cloned()).unwrap_or_default()
}

// ---------------------------------------------------------------- 自分の画面からの要求だけを受け付ける
// 127.0.0.1 でだけ待ち受けるが、同じ Mac のブラウザで開いた別のサイトから、ファイルの書き出し・Finder の起動・WebSocket を
// 要求されるおそれがある（CSRF・WebSocket の乗っ取り・DNS rebinding）。Host がこの Mac の名前でないものと、Origin が自分の画面でないものを断る。
const LOCAL_HOSTS: [&str; 4] = ["127.0.0.1", "localhost", "[::1]", "testserver"];

fn host_only(v: &str) -> String {
    let v = v.trim().to_lowercase();
    if v.starts_with('[') {
        return match v.find(']') {
            Some(i) => v[..=i].to_string(),
            None => v,
        };
    }
    v.split(':').next().unwrap_or("").to_string()
}

pub async fn local_only(req: Request, next: Next) -> Response {
    let h = req.headers();
    let host_raw = h.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let origin = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let mut bad = !LOCAL_HOSTS.contains(&host_only(&host_raw).as_str());
    if !bad {
        if let Some(o) = &origin {
            bad = o.trim().to_lowercase().trim_end_matches('/') != format!("http://{}", host_raw.trim().to_lowercase());
        }
    }
    if bad {
        log::warn!("rejected a request from outside the app: host={host_raw} origin={origin:?} path={}", req.uri().path());
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    let is_static = !req.uri().path().starts_with("/api/");
    let mut resp = next.run(req).await;
    if is_static {
        // ローカルアプリなので画面ファイルはキャッシュさせない（更新後に古い JS が使われ続けるのを防ぐ）
        resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        resp.headers_mut().remove(header::ETAG);
        resp.headers_mut().remove(header::LAST_MODIFIED);
    }
    resp
}

// ---------------------------------------------------------------- 画面ファイル
async fn static_file(uri: axum::http::Uri) -> Response {
    let root = &config::paths().frontend_dir;
    let rel = uri.path().trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let rel = percent_encoding::percent_decode_str(rel).decode_utf8_lossy().into_owned();
    if rel.split('/').any(|p| p == ".." || p.starts_with('.')) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut p = root.join(&rel);
    if p.is_dir() {
        p = p.join("index.html");
    }
    match tokio::fs::read(&p).await {
        Ok(b) => {
            let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            // .js は ES Module として読ませるので text/javascript を明示する
            let ct = match ext.as_str() {
                "js" | "mjs" => "text/javascript; charset=utf-8".to_string(),
                "css" => "text/css; charset=utf-8".to_string(),
                "html" => "text/html; charset=utf-8".to_string(),
                "wasm" => "application/wasm".to_string(),
                "json" => "application/json".to_string(),
                "svg" => "image/svg+xml".to_string(),
                _ => mime_guess::from_path(&p).first_or_octet_stream().to_string(),
            };
            ([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "no-store".to_string())], b).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, Json(json!({"detail": "Not Found"}))).into_response(),
    }
}

// ---------------------------------------------------------------- ルート
pub fn router(state: Arc<AppState>) -> Router {
    let r = Router::new()
        .route("/api/health", get(|| async { Json(json!({"ok": true, "version": config::VERSION})) }))
        .route("/api/diagnostics", post(diagnostics))
        .route("/api/library", get(library))
        .route("/api/itunes", get(|| async { blocking(|| Ok(Json(itunes::status()))).await }))
        .route("/api/itunes/import", post(itunes_import))
        .route("/api/itunes/unlink", post(itunes_unlink))
        .route("/api/itunes/playlists", get(|| async { blocking(|| Ok(Json(json!({"items": itunes::playlists()?})))).await }))
        .route("/api/library/facets", get(library_facets))
        .route("/api/library/folders", get(library_folders))
        .route("/api/library/scan", get(scan_status))
        .route("/api/library/rescan", post(rescan))
        .route("/api/queue", get(get_queue).put(put_queue))
        .route("/api/mixes", get(mixes_list).post(mixes_create))
        .route("/api/mixes/played", post(mixes_played))
        .route("/api/mixes/after-export", post(mixes_after_export))
        .route("/api/mixes/:id", get(mixes_get).patch(mixes_rename).delete(mixes_delete))
        .route("/api/mixes/:id/duplicate", post(mixes_duplicate))
        .route("/api/mixes/:id/open", post(mixes_open))
        .route("/api/tracks/:id/rating", post(track_rating))
        .route("/api/plays", get(plays_list).post(plays_add))
        .route("/api/queues", get(|| async { blocking(|| Ok(Json(json!({"queues": queues::names()?})))).await }))
        .route("/api/tracks/:id/analyze", post(analyze))
        .route("/api/tracks", get(tracks))
        .route("/api/tracks/:id/analysis", get(track_analysis))
        .route("/api/tracks/:id/edits", get(get_edits).patch(patch_edits))
        .route("/api/tracks/:id/sections", get(track_sections))
        .route("/api/tracks/:id/cutpoints", get(track_cutpoints))
        .route("/api/tracks/:id/wave", get(track_wave))
        .route("/api/tracks/:id/pcm", get(track_pcm))
        .route("/api/tracks/:id/stems", get(track_stems))
        .route("/api/tracks/:id/stems-audio", get(track_stems_audio))
        .route("/api/tracks/:id/stems-pcm", get(track_stems_pcm))
        .route("/api/analysis/status", get(analysis_status))
        .route("/api/bulk", get(bulk_status))
        .route("/api/bulk/start", post(bulk_start))
        .route("/api/bulk/failed", get(bulk_failed))
        .route("/api/bulk/retry-failed", post(bulk_retry_failed))
        .route("/api/bulk/pause", post(bulk_pause))
        .route("/api/bulk/playing", post(bulk_playing))
        .route("/api/genre-groups", get(genre_groups))
        .route("/api/midi/ports", get(midi_ports))
        .route("/api/dev/midi-inject", post(midi_inject))
        .route("/api/mappings", get(list_mappings))
        .route("/api/mappings/:name", get(get_mapping).put(put_mapping))
        .route("/api/settings", get(|| async { blocking(|| Ok(Json(Value::Object(settings::load())))).await }).put(put_settings))
        .route("/api/stats", get(stats))
        .route("/api/reveal", post(reveal))
        .route("/api/dialog/folder", post(dialog_folder))
        .route("/api/log", post(client_log))
        .route("/ws", get(ws_midi));
    let r = crate::server_ext::routes(r);
    r.fallback(get(static_file)).layer(axum::middleware::from_fn(local_only)).with_state(state)
}

async fn diagnostics(Json(d): Json<Value>) -> Json<Value> {
    let checks = d.get("checks").and_then(|c| c.as_object()).cloned().unwrap_or_default();
    let ok = checks.values().filter(|v| v.is_boolean()).all(|v| v.as_bool() == Some(true));
    log::info!(
        "frontend diagnostics {}: {} | {}",
        if ok { "OK" } else { "NG" },
        Value::Object(checks),
        d.get("userAgent").and_then(|v| v.as_str()).unwrap_or("")
    );
    Json(json!({"ok": ok}))
}

// ---- ライブラリ ----
async fn library(RawQuery(raw): RawQuery) -> ApiResult {
    let q = Q::parse(raw);
    let o = SearchOpts {
        q: q.str("q"),
        folder: q.opt_str("folder"),
        analyzed: q.bool("analyzed"),
        sort: q.get("sort").unwrap_or("path").to_string(),
        offset: q.i64("offset").unwrap_or(0).max(0),
        limit: q.i64("limit").unwrap_or(200).clamp(0, 1000),
        desc: q.bool("desc").unwrap_or(false),
        root: q.opt_str("root"),
        artist: q.opt_str("artist"),
        album: q.opt_str("album"),
        playlist: q.i64("playlist"),
        artist_q: q.str("artist_q"),
        album_q: q.str("album_q"),
        bpm_min: q.f64("bpm_min"),
        bpm_max: q.f64("bpm_max"),
        keys: q.list("keys"),
        groups: q.list("groups"),
        rating_min: q.i64("rating_min"),
    };
    blocking(move || Ok(Json(query::search(&o)?))).await
}

async fn itunes_import() -> ApiResult {
    blocking(|| {
        settings::save_one("itunes_enabled", json!(true))?;
        itunes::import_xml(None)?;
        Ok(Json(itunes::status()))
    })
    .await
}

async fn itunes_unlink() -> ApiResult {
    blocking(|| {
        itunes::unlink()?;
        Ok(Json(itunes::status()))
    })
    .await
}

async fn library_facets(RawQuery(raw): RawQuery) -> ApiResult {
    let q = Q::parse(raw);
    let kind = q.get("kind").unwrap_or("artist").to_string();
    if kind != "artist" && kind != "album" {
        return Err(bad("kind は artist か album"));
    }
    let artist = q.opt_str("artist");
    blocking(move || Ok(Json(json!({"items": query::facets(&kind, artist.as_deref())?})))).await
}

async fn library_folders() -> ApiResult {
    blocking(|| Ok(Json(json!({"counts": query::counts()?, "folders": query::folders()?, "roots": roots::configured()})))).await
}

async fn scan_status(State(s): St) -> Json<Value> {
    let mut v = s.scanner.status();
    v["root"] = json!(s.scanner.root().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default());
    v["roots"] = json!(s.scanner.roots().iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>());
    Json(v)
}

async fn rescan(State(s): St) -> Json<Value> {
    Json(json!({"started": s.scanner.start()}))
}

// ---- ミックスキュー ----
async fn get_queue(RawQuery(raw): RawQuery) -> ApiResult {
    let q = Q::parse(raw);
    let name = q.get("name").unwrap_or(CURRENT).to_string();
    blocking(move || Ok(Json(json!({"name": name, "items": queues::load(&name)?})))).await
}

async fn put_queue(RawQuery(raw): RawQuery, Json(body): Json<Value>) -> ApiResult {
    let q = Q::parse(raw);
    let name = q.get("name").unwrap_or(CURRENT).to_string();
    if name.trim().is_empty() {
        return Err(bad("name is empty"));
    }
    let items = queues::items_from_json(&body["items"]);
    blocking(move || {
        queues::save(&items, &name)?;
        Ok(Json(json!({"ok": true, "count": items.len()})))
    })
    .await
}

// ---- ミックス・好み・履歴 ----
async fn mixes_list(RawQuery(raw): RawQuery) -> ApiResult {
    let sort = Q::parse(raw).get("sort").unwrap_or("new").to_string();
    blocking(move || Ok(Json(json!({"mixes": mixes::list_mixes(&sort)?, "current": settings::load().get("current_mix")})))).await
}

async fn mixes_get(Path(id): Path<i64>) -> ApiResult {
    blocking(move || mixes::get(id)?.map(Json).ok_or_else(|| not_found("ミックスがありません"))).await
}

async fn mixes_create(body: Option<Json<Value>>) -> ApiResult {
    let b = body_obj(body);
    blocking(move || {
        let name = b
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d %H%M ミックス").to_string());
        let kind = b.get("kind").and_then(|v| v.as_str()).unwrap_or("hand").to_string();
        let m = mixes::save(&queues::load(CURRENT)?, &name, &kind)?;
        settings::save_one("current_mix", m["id"].clone())?;
        Ok(Json(m))
    })
    .await
}

async fn mixes_rename(Path(id): Path<i64>, body: Option<Json<Value>>) -> ApiResult {
    let b = body_obj(body);
    blocking(move || {
        let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        mixes::rename(id, &name)?.map(Json).ok_or_else(|| not_found("ミックスがありません"))
    })
    .await
}

async fn mixes_duplicate(Path(id): Path<i64>) -> ApiResult {
    blocking(move || mixes::duplicate(id)?.map(Json).ok_or_else(|| not_found("ミックスがありません"))).await
}

async fn mixes_delete(Path(id): Path<i64>) -> ApiResult {
    blocking(move || {
        let ok = mixes::delete(id)?;
        if settings::load().get("current_mix").and_then(|v| v.as_i64()) == Some(id) {
            settings::save_one("current_mix", Value::Null)?;
        }
        Ok(Json(json!({"ok": ok})))
    })
    .await
}

async fn mixes_open(Path(id): Path<i64>, body: Option<Json<Value>>) -> ApiResult {
    let b = body_obj(body);
    blocking(move || {
        let Some(items) = mixes::get_items(id)? else { return Err(not_found("ミックスがありません")) };
        let append = b.get("mode").and_then(|v| v.as_str()) == Some("append");
        let mut cur: Vec<QueueItem> = if append { queues::load(CURRENT)? } else { vec![] };
        cur.extend(items);
        queues::save(&cur, CURRENT)?;
        settings::save_one("current_mix", if append { Value::Null } else { json!(id) })?;
        Ok(Json(json!({"name": CURRENT, "items": queues::load(CURRENT)?, "mix": id})))
    })
    .await
}

async fn mixes_played() -> ApiResult {
    blocking(|| {
        let mid = settings::load().get("current_mix").and_then(|v| v.as_i64());
        if mid.is_some() && mixes::is_current(mid)? {
            mixes::record_play(mid.unwrap())?;
            return Ok(Json(json!({"mix": mid})));
        }
        Ok(Json(json!({"mix": null})))
    })
    .await
}

async fn mixes_after_export(body: Option<Json<Value>>) -> ApiResult {
    let b = body_obj(body);
    blocking(move || {
        let m = mixes::record_export(&b, settings::load().get("current_mix").and_then(|v| v.as_i64()))?;
        settings::save_one("current_mix", m["id"].clone())?;
        Ok(Json(m))
    })
    .await
}

async fn track_rating(Path(id): Path<i64>, body: Option<Json<Value>>) -> ApiResult {
    let b = body_obj(body);
    blocking(move || {
        let r = b.get("rating").and_then(|v| v.as_f64()).unwrap_or(0.0) as i64;
        Ok(Json(json!({"track_id": id, "rating": prefs::set_rating(id, r)?})))
    })
    .await
}

async fn plays_add(body: Option<Json<Value>>) -> ApiResult {
    let b = body_obj(body);
    blocking(move || {
        let src = b.get("source").and_then(|v| v.as_str()).filter(|s| prefs::SOURCES.contains(s)).unwrap_or("deck").to_string();
        let ids: Vec<i64> = b
            .get("track_ids")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64))).collect())
            .unwrap_or_default();
        Ok(Json(json!({"recorded": prefs::record_plays(&ids, &src, None)?})))
    })
    .await
}

async fn plays_list(RawQuery(raw): RawQuery) -> ApiResult {
    let limit = Q::parse(raw).i64("limit").unwrap_or(50).clamp(1, 500);
    blocking(move || Ok(Json(json!({"items": prefs::history(limit)?})))).await
}

// ---- 解析 ----
async fn analyze(State(s): St, Path(id): Path<i64>, RawQuery(raw): RawQuery) -> ApiResult {
    let q = Q::parse(raw);
    let prio = q.i64("priority").unwrap_or(PRIO_MANUAL);
    let force = q.bool("force").unwrap_or(false);
    blocking(move || {
        if query::get_tracks(&[id])?.is_empty() {
            return Err(not_found("track not found"));
        }
        Ok(Json(json!({"status": s.worker.request(id, prio, force)})))
    })
    .await
}

async fn tracks(RawQuery(raw): RawQuery) -> ApiResult {
    let ids: Vec<i64> = Q::parse(raw).str("ids").split(',').filter_map(|x| x.trim().parse().ok()).take(500).collect();
    blocking(move || {
        let m = query::get_tracks(&ids)?;
        // Python 版と同じく DB の順（id の順）
        let mut keys: Vec<i64> = m.keys().copied().collect();
        keys.sort();
        let items: Vec<Value> = keys.iter().filter_map(|i| m.get(i).cloned().map(Value::Object)).collect();
        Ok(Json(json!({"items": items})))
    })
    .await
}

async fn track_analysis(Path(id): Path<i64>) -> ApiResult {
    blocking(move || {
        let a = cache::read(id).ok_or_else(|| not_found("not analyzed"))?;
        Ok(Json(edits::apply_value(id, &a)?))
    })
    .await
}

async fn patch_edits(Path(id): Path<i64>, Json(body): Json<Value>) -> ApiResult {
    blocking(move || {
        if query::get_tracks(&[id])?.is_empty() {
            return Err(not_found("track not found"));
        }
        let patch = body.as_object().cloned().unwrap_or_default();
        edits::update(id, &patch)?;
        match cache::read(id) {
            Some(a) => Ok(Json(edits::apply_value(id, &a)?)),
            None => Ok(Json(json!({"edits": edits::get(id)?}))),
        }
    })
    .await
}

async fn get_edits(Path(id): Path<i64>) -> ApiResult {
    blocking(move || Ok(Json(Value::Object(edits::get(id)?)))).await
}

async fn track_sections(Path(id): Path<i64>) -> ApiResult {
    blocking(move || {
        let s = sections::for_track(id).ok_or_else(|| not_found("not analyzed"))?;
        Ok(Json(json!({"sections": *s})))
    })
    .await
}

async fn track_cutpoints(Path(id): Path<i64>) -> ApiResult {
    blocking(move || {
        let r = cutpoints::for_track(id).ok_or_else(|| not_found("not analyzed"))?;
        Ok(Json(serde_json::to_value(&*r).map_err(anyhow::Error::from)?))
    })
    .await
}

async fn track_wave(Path(id): Path<i64>) -> Result<Response, ApiError> {
    let b = blocking(move || cache::wave_bytes(id).ok_or_else(|| not_found("not analyzed"))).await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], b).into_response())
}

fn pcm_headers(total: usize, frames: Option<usize>) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert("X-Total-Frames", HeaderValue::from_str(&total.to_string()).unwrap());
    h.insert("X-Sample-Rate", HeaderValue::from_static("44100"));
    if let Some(f) = frames {
        h.insert("X-Frames", HeaderValue::from_str(&f.to_string()).unwrap());
        h.insert("Access-Control-Expose-Headers", HeaderValue::from_static("X-Total-Frames, X-Frames, X-Sample-Rate"));
    } else {
        h.insert("Access-Control-Expose-Headers", HeaderValue::from_static("X-Total-Frames, X-Sample-Rate"));
    }
    h
}

async fn track_pcm(State(s): St, Path(id): Path<i64>, RawQuery(raw): RawQuery) -> Result<Response, ApiError> {
    let q = Q::parse(raw);
    let offset = q.i64("offset").unwrap_or(0).max(0) as usize;
    let frames = q.i64("frames").unwrap_or(441000).clamp(0, 44100 * 60) as usize;
    let (body, total) = blocking(move || {
        let path = query::track_path(id).ok_or_else(|| not_found("track not found"))?;
        s.pcm.chunk(id, &path, offset, frames).map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))
    })
    .await?;
    Ok((pcm_headers(total, None), body).into_response())
}

/// 小節ごとのパートの量。まだなら裏で求め始めて status='pending' を返す（画面は少し待って聞き直す）。
async fn track_stems(Path(id): Path<i64>) -> ApiResult {
    blocking(move || {
        if let Some(arr) = stems::read(id) {
            let bars: Vec<Vec<f64>> = arr.iter().map(|r| r.iter().map(|v| util::round_f32(*v, 3)).collect()).collect();
            return Ok(Json(json!({"status": "done", "sources": stems::SOURCES, "bars": bars, "vocal_on": stems::VOCAL_ON})));
        }
        let (path, done) = track_done(id)?;
        if !done {
            return Err(not_found("解析されていません"));
        }
        if request_stems(id, &path, false) == "unavailable" {
            return Ok(Json(json!({"status": "unavailable"})));
        }
        Ok(Json(json!({"status": "pending"})))
    })
    .await
}

fn track_done(id: i64) -> ApiResult<(String, bool)> {
    let r: Option<(String, String)> = db::with(|c| {
        Ok(c.query_row("SELECT path, analysis_status FROM tracks WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?))).ok())
    })?;
    match r {
        Some((p, s)) => Ok((p, s == "done")),
        None => Err(not_found("解析されていません")),
    }
}

/// 分離した音の状態。まだなら（request=1 のとき）裏で分離を始めて status='pending'。
async fn track_stems_audio(Path(id): Path<i64>, RawQuery(raw): RawQuery) -> ApiResult {
    let req = Q::parse(raw).i64("request").unwrap_or(1);
    blocking(move || {
        if crate::stubs::stems::audio_ready(id) {
            return Ok(Json(json!({"status": "done"})));
        }
        let (path, done) = track_done(id)?;
        if !done {
            return Err(not_found("解析されていません"));
        }
        if req == 0 {
            return Ok(Json(json!({"status": "none"})));
        }
        if request_stems(id, &path, true) == "unavailable" {
            return Ok(Json(json!({"status": "unavailable"})));
        }
        Ok(Json(json!({"status": "pending"})))
    })
    .await
}

/// 分離した音の一部。planar float32 × 8ch（ボーカル L・R、ドラム L・R、ベース L・R、その他 L・R の順に frames 個ずつ）。
async fn track_stems_pcm(Path(id): Path<i64>, RawQuery(raw): RawQuery) -> Result<Response, ApiError> {
    let q = Q::parse(raw);
    let offset = q.i64("offset").unwrap_or(0).max(0) as usize;
    let frames = q.i64("frames").unwrap_or(441000).clamp(0, 44100 * 120) as usize;
    let (body, total, n) = blocking(move || {
        if !crate::stubs::stems::audio_ready(id) {
            return Err(not_found("分離した音がまだありません"));
        }
        let (y, total) = crate::stubs::stems::read_audio(id, offset, frames)?;
        let n = y.first().map(|c| c.len()).unwrap_or(0);
        let mut out = Vec::with_capacity(n * 8 * 4);
        for ch in &y {
            for v in ch {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        Ok((out, total, n))
    })
    .await?;
    Ok((pcm_headers(total, Some(n)), body).into_response())
}

async fn analysis_status(State(s): St) -> Json<Value> {
    Json(s.worker.status())
}

// ---- まとめて解析 ----
async fn bulk_status(State(s): St) -> ApiResult {
    blocking(move || Ok(Json(s.bulk.status()))).await
}

async fn bulk_start(State(s): St, body: Option<Json<Value>>) -> ApiResult {
    let b = body_obj(body);
    blocking(move || {
        let names = genres::names();
        let groups: Vec<String> = b
            .get("groups")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).filter(|g| names.contains(g)).map(String::from).collect())
            .unwrap_or_default();
        let rest = b.get("rest").and_then(|v| v.as_bool()).unwrap_or(true);
        let st = b.get("stems").and_then(|v| v.as_bool()).unwrap_or(false);
        s.bulk.start(groups, rest, st);
        Ok(Json(s.bulk.status()))
    })
    .await
}

async fn bulk_failed(State(s): St) -> ApiResult {
    blocking(move || Ok(Json(json!({"items": s.bulk.failed_list()?})))).await
}

async fn bulk_retry_failed(State(s): St) -> ApiResult {
    blocking(move || {
        let n = s.bulk.retry_failed()?;
        let running = s.bulk.status()["running"].as_bool().unwrap_or(false);
        if n > 0 && !running {
            s.bulk.start(vec![], true, false);
        }
        let mut v = s.bulk.status();
        v["reset"] = json!(n);
        Ok(Json(v))
    })
    .await
}

async fn bulk_pause(State(s): St) -> ApiResult {
    s.bulk.pause();
    blocking(move || Ok(Json(s.bulk.status()))).await
}

async fn bulk_playing(State(s): St, body: Option<Json<Value>>) -> Json<Value> {
    let b = body_obj(body);
    s.bulk.set_playing(b.get("playing").and_then(|v| v.as_bool()).unwrap_or(false));
    Json(json!({"ok": true}))
}

/// ジャンルのまとまりごとの曲数（全体・解析済み・未解析）。
async fn genre_groups() -> ApiResult {
    blocking(|| {
        use crate::analysis::bulk::{MAX_SEC, MIN_SEC};
        let names = genres::names();
        let mut counts: Vec<(String, i64, i64, i64)> = names.iter().map(|n| (n.to_string(), 0, 0, 0)).collect();
        let (rw, rargs) = roots::where_sql("");
        db::with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT genre, analysis_status FROM tracks WHERE duration BETWEEN {MIN_SEC} AND {MAX_SEC} AND {rw}"
            ))?;
            for (g, s) in st
                .query_map(rusqlite::params_from_iter(rargs.iter()), |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)))?
                .flatten()
            {
                let grp = genres::group_of(g.as_deref());
                if let Some(e) = counts.iter_mut().find(|e| e.0 == grp) {
                    e.1 += 1;
                    e.2 += (s == "done") as i64;
                    e.3 += (s == "none") as i64;
                }
            }
            Ok(())
        })?;
        let groups: Vec<Value> =
            counts.into_iter().map(|(n, t, a, l)| json!({"name": n, "total": t, "analyzed": a, "left": l})).collect();
        Ok(Json(json!({"groups": groups})))
    })
    .await
}

// ---- MIDI ----
async fn ws_midi(State(s): St, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| midi_socket(s, socket))
}

async fn midi_socket(s: Arc<AppState>, mut socket: WebSocket) {
    let mut rx = s.midi.tx.subscribe();
    loop {
        tokio::select! {
            m = rx.recv() => {
                match m {
                    Ok(v) => { if socket.send(Message::Text(v.to_string())).await.is_err() { break; } }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            if v["type"] == "midi_out" {
                                let data: Vec<i64> = v["data"].as_array().map(|a| a.iter().filter_map(|x| x.as_i64()).collect()).unwrap_or_default();
                                s.midi.send(v["port"].as_str().unwrap_or(""), &data);
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
}

async fn midi_ports(State(s): St) -> Json<Value> {
    Json(s.midi.ports())
}

async fn midi_inject(State(s): St, Json(b): Json<Value>) -> Json<Value> {
    let data: Vec<i64> = b["data"].as_array().map(|a| a.iter().filter_map(|x| x.as_i64()).collect()).unwrap_or_default();
    s.midi.inject(b["port"].as_str().unwrap_or("virtual"), &data);
    Json(json!({"ok": true}))
}

fn mapping_name_ok(name: &str) -> bool {
    static RX: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"^[\w\- ぁ-んァ-ン一-龥]{1,60}$").unwrap());
    RX.is_match(name)
}

async fn list_mappings() -> ApiResult {
    blocking(|| {
        let d = &config::paths().mappings_dir;
        std::fs::create_dir_all(d).map_err(anyhow::Error::from)?;
        let mut names: Vec<String> = std::fs::read_dir(d)
            .map_err(anyhow::Error::from)?
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                (p.extension().map(|x| x == "json").unwrap_or(false)).then(|| p.file_stem().unwrap().to_string_lossy().into_owned())
            })
            .collect();
        names.sort();
        Ok(Json(json!({"mappings": names})))
    })
    .await
}

async fn get_mapping(Path(name): Path<String>) -> ApiResult {
    blocking(move || {
        let p = config::paths().mappings_dir.join(format!("{name}.json"));
        if !mapping_name_ok(&name) || !p.exists() {
            return Ok(Json(json!({"name": name, "entries": []})));
        }
        let v: Value = serde_json::from_slice(&std::fs::read(p).map_err(anyhow::Error::from)?).map_err(anyhow::Error::from)?;
        Ok(Json(v))
    })
    .await
}

async fn put_mapping(Path(name): Path<String>, Json(body): Json<Value>) -> ApiResult {
    if !mapping_name_ok(&name) {
        return Err(bad("invalid name"));
    }
    blocking(move || {
        let d = &config::paths().mappings_dir;
        std::fs::create_dir_all(d).map_err(anyhow::Error::from)?;
        let mut m = body.as_object().cloned().unwrap_or_default();
        m.insert("name".into(), json!(name));
        std::fs::write(d.join(format!("{name}.json")), serde_json::to_vec_pretty(&Value::Object(m)).map_err(anyhow::Error::from)?)
            .map_err(anyhow::Error::from)?;
        Ok(Json(json!({"ok": true})))
    })
    .await
}

// ---- 設定 ----
async fn put_settings(State(s): St, Json(patch): Json<Value>) -> ApiResult {
    blocking(move || {
        let mut patch = patch.as_object().cloned().unwrap_or_default();
        let before = roots::configured();
        if let Some(mr) = patch.remove("music_root") {
            // Phase 1 の 1 つだけの指定 → 音楽フォルダの一覧にする
            patch.insert("music_roots".into(), json!([mr]));
        }
        if let Some(v) = patch.get("music_roots").cloned() {
            let list: Vec<String> = v
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string()))
                        .map(|x| expand_home(x.trim()))
                        .filter(|x| !x.is_empty())
                        .map(|x| roots::normpath(&x))
                        .collect()
                })
                .unwrap_or_default();
            let known: Vec<String> = before.iter().map(|r| roots::norm(r)).collect();
            for r in &list {
                // 新しく足すフォルダは、今あるものだけ（外付けドライブが外れているだけの既存のフォルダはそのまま）
                if !known.contains(&roots::norm(r)) && !std::path::Path::new(r).is_dir() {
                    return Err(bad(format!("フォルダが見つかりません: {r}")));
                }
            }
            patch.insert("music_roots".into(), json!(list));
        }
        let cur = settings::save(&patch)?;
        let after = roots::configured();
        if after.iter().map(|r| roots::norm(r)).collect::<Vec<_>>() != before.iter().map(|r| roots::norm(r)).collect::<Vec<_>>() {
            s.scanner.set_roots(after);
            s.scanner.start();
        }
        Ok(Json(Value::Object(cur)))
    })
    .await
}

fn expand_home(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = std::env::var_os("HOME") {
            return format!("{}/{rest}", h.to_string_lossy());
        }
    }
    p.to_string()
}

async fn stats(State(s): St) -> ApiResult {
    blocking(move || {
        let (rw, rargs) = roots::where_sql("");
        let (n, a, f): (i64, Option<i64>, Option<i64>) = db::with(|c| {
            Ok(c.query_row(
                &format!("SELECT COUNT(*), SUM(analysis_status='done'), SUM(analysis_status='failed') FROM tracks WHERE {rw}"),
                rusqlite::params_from_iter(rargs.iter()),
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })?;
        Ok(Json(json!({"tracks": n, "analyzed": a.unwrap_or(0), "failed": f.unwrap_or(0),
                       "music_root": s.scanner.root().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                       "roots": roots_status()?})))
    })
    .await
}

/// 音楽フォルダごとの状態: 設定にあるもの（見つかるか・曲数）と、外したが曲が残っているもの。
fn roots_status() -> anyhow::Result<Vec<Value>> {
    let counts: HashMap<String, i64> = db::with(|c| {
        let mut st = c.prepare("SELECT root, COUNT(*) FROM tracks WHERE root IS NOT NULL GROUP BY root")?;
        let m = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.flatten().collect();
        Ok(m)
    })?;
    let conf = roots::configured();
    let mut out: Vec<Value> = conf
        .iter()
        .map(|r| json!({"path": r, "active": true, "exists": std::path::Path::new(r).is_dir(), "tracks": counts.get(r).copied().unwrap_or(0)}))
        .collect();
    let confn: Vec<String> = conf.iter().map(|r| roots::norm(r)).collect();
    let mut rest: Vec<(&String, &i64)> = counts.iter().filter(|(r, _)| !confn.contains(&roots::norm(r))).collect();
    rest.sort();
    for (r, n) in rest {
        out.push(json!({"path": r, "active": false, "exists": std::path::Path::new(r).is_dir(), "tracks": n}));
    }
    Ok(out)
}

/// 書き出したファイル・保存先のフォルダを Finder で開く（ファイルならそのフォルダを開いて選んだ状態に）。
pub fn reveal_path(path: &str) -> Result<&'static str, ApiError> {
    let p = expand_home(path.trim());
    if p.contains("://") || p.starts_with("//") || p.starts_with("\\\\") {
        return Err(bad("ネットワーク上のフォルダは開けません"));
    }
    let mut pb = std::path::PathBuf::from(&p);
    if pb.is_relative() {
        pb = std::env::current_dir().unwrap_or_default().join(pb);
    }
    let open = |args: &[&std::ffi::OsStr]| {
        let prog = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        std::process::Command::new(prog).args(args).spawn().map(|_| ())
    };
    if pb.is_file() {
        if cfg!(target_os = "macos") {
            open(&["-R".as_ref(), pb.as_os_str()]).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        } else {
            open(&[pb.parent().unwrap_or(&pb).as_os_str()]).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        return Ok("file");
    }
    while !pb.is_dir() {
        match pb.parent() {
            Some(par) if par != pb => pb = par.to_path_buf(),
            _ => return Err(not_found("フォルダが見つかりません")),
        }
    }
    open(&[pb.as_os_str()]).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok("folder")
}

async fn reveal(Json(b): Json<Value>) -> ApiResult {
    let p = b["path"].as_str().unwrap_or("").trim().to_string();
    if p.is_empty() {
        return Err(bad("パスが空です"));
    }
    blocking(move || Ok(Json(json!({"opened": reveal_path(&p)?})))).await
}

/// 専用ウィンドウで開いているときは、Mac のフォルダ選択を開く（ブラウザで開いているときは使えない）。
async fn dialog_folder(State(s): St) -> ApiResult {
    let f = s.dialog.lock().clone();
    let Some(f) = f else {
        return Err(ApiError(StatusCode::NOT_IMPLEMENTED, "専用ウィンドウでないため、フォルダ選択は開けません。パスを入力してください".into()));
    };
    let r = tokio::task::spawn_blocking(move || f()).await.ok().flatten();
    match r {
        None => Err(ApiError(StatusCode::NOT_IMPLEMENTED, "フォルダ選択を開けませんでした".into())),
        Some(p) => Ok(Json(json!({"path": p}))),
    }
}

/// 画面側のエラーを djmix.log に残す（専用ウィンドウでは開発者ツールが見えないため）。
async fn client_log(Json(b): Json<Value>) -> Json<Value> {
    let level = b["level"].as_str().unwrap_or("error");
    let msg = util::truncate_chars(b["message"].as_str().unwrap_or(""), 500);
    let detail = util::truncate_chars(b["detail"].as_str().unwrap_or(""), 2000);
    if level == "error" {
        log::error!("frontend {level}: {msg} {detail}");
    } else {
        log::info!("frontend {level}: {msg} {detail}");
    }
    Json(json!({"ok": true}))
}

/// テストなどで使う: 生のバイト列の応答
pub fn bytes_response(b: Vec<u8>) -> Response {
    Response::builder().header(header::CONTENT_TYPE, "application/octet-stream").body(Body::from(Bytes::from(b))).unwrap()
}
