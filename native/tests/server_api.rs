//! API の結合試験（tests/test_server.py・test_library.py・test_mixes.py・test_pcm.py に相当）。
//! 一時フォルダにデータと曲（ffmpeg で作る）を置き、ルーターに直接要求を送る。
use axum::body::Body;
use axum::http::{Request, StatusCode};
use djmix::config::{self, Paths};
use djmix::server::{router, AppState};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

fn ffmpeg_tone(path: &Path, freq: u32, secs: u32, meta: &[(&str, &str)]) {
    let mut c = std::process::Command::new("ffmpeg");
    c.args(["-v", "error", "-y", "-f", "lavfi", "-i", &format!("sine=frequency={freq}:duration={secs}"), "-ac", "2"]);
    for (k, v) in meta {
        c.args(["-metadata", &format!("{k}={v}")]);
    }
    assert!(c.arg(path).status().unwrap().success());
}

struct T {
    app: axum::Router,
}

impl T {
    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (s, b, _) = self.raw(method, uri, body, &[]).await;
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    async fn raw(&self, method: &str, uri: &str, body: Option<Value>, headers: &[(&str, &str)]) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
        let mut req = Request::builder().method(method).uri(uri).header("host", "127.0.0.1:9");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let req = match body {
            Some(b) => req.header("content-type", "application/json").body(Body::from(b.to_string())).unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let h = resp.headers().clone();
        let b = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
        (status, b, h)
    }
}

async fn wait_scan(t: &T) {
    for _ in 0..200 {
        let (_, s) = t.call("GET", "/api/library/scan", None).await;
        if s["running"] == false && s["finished"].is_number() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("scan did not finish");
}

#[tokio::test(flavor = "multi_thread")]
async fn api_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let music: PathBuf = tmp.path().join("music");
    let music2: PathBuf = tmp.path().join("music2");
    std::fs::create_dir_all(music.join("Jazz")).unwrap();
    std::fs::create_dir_all(&music2).unwrap();
    ffmpeg_tone(&music.join("Jazz/a.mp3"), 440, 3, &[("title", "Alpha"), ("artist", "Zed"), ("album", "J1"), ("genre", "Jazz")]);
    ffmpeg_tone(&music.join("Jazz/b.mp3"), 330, 4, &[("title", "Beta"), ("artist", "Amy"), ("album", "J1"), ("genre", "Rock")]);
    ffmpeg_tone(&music.join("c.wav"), 220, 2, &[]);
    ffmpeg_tone(&music2.join("d.flac"), 550, 2, &[("title", "Delta")]);
    std::env::set_var("DJMIX_NO_ITUNES", "1");
    std::env::set_var("DJMIX_NO_MIDI", "1");
    std::env::set_var("DJMIX_MUSIC_ROOT", &music);
    config::init(Paths::new(data.clone(), config::default_resources_dir()));
    config::seed_mappings();
    let state: Arc<AppState> = AppState::new();
    state.startup();
    let t = T { app: router(state.clone()) };
    wait_scan(&t).await;

    // ---- 起動・画面ファイル（test_server.py）
    let (s, h) = t.call("GET", "/api/health", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["ok"], true);
    let (s, body, hd) = t.raw("GET", "/", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("<html"));
    assert_eq!(hd["cache-control"], "no-store");
    let (_, _, hd) = t.raw("GET", "/app.js", None, &[("if-none-match", "x")]).await;
    assert!(hd["content-type"].to_str().unwrap().starts_with("text/javascript"));
    // 他のサイトからの要求は断る（Origin・Host）
    let (s, _, _) = t.raw("POST", "/api/reveal", Some(json!({"path": "/"})), &[("origin", "http://evil.example")]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let req = Request::builder().uri("/api/health").header("host", "evil.example").body(Body::empty()).unwrap();
    assert_eq!(t.app.clone().oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);

    // ---- ライブラリ（test_library.py）
    let (_, lib) = t.call("GET", "/api/library", None).await;
    assert_eq!(lib["total"], 3);
    let titles: Vec<&str> = lib["items"].as_array().unwrap().iter().map(|x| x["title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"Alpha") && titles.contains(&"c"));
    let (_, q) = t.call("GET", "/api/library?q=alp", None).await;
    assert_eq!(q["total"], 1);
    let (_, g) = t.call("GET", "/api/library?groups=%E3%82%B8%E3%83%A3%E3%82%BA%E3%83%BB%E3%83%95%E3%83%A5%E3%83%BC%E3%82%B8%E3%83%A7%E3%83%B3", None).await;
    assert_eq!(g["total"], 1, "{g}");
    let (_, srt) = t.call("GET", "/api/library?sort=artist", None).await;
    let arts: Vec<Value> = srt["items"].as_array().unwrap().iter().map(|x| x["artist"].clone()).collect();
    assert_eq!(arts, vec![json!("Amy"), json!("Zed"), Value::Null], "値の無い曲は最後");
    let (_, f) = t.call("GET", "/api/library/facets?kind=album", None).await;
    assert_eq!(f["items"][0]["name"], "J1");
    assert_eq!(f["items"][0]["count"], 2);

    // ---- キュー（つなぎは次の曲とセット）
    let ids: Vec<i64> = lib["items"].as_array().unwrap().iter().map(|x| x["id"].as_i64().unwrap()).collect();
    let items = json!([{"track_id": ids[0], "join_tpl": "short", "join_bars": 8, "join_next": ids[1]}, {"track_id": ids[1]}, {"track_id": ids[2]}]);
    let (s, _) = t.call("PUT", "/api/queue", Some(json!({"items": items}))).await;
    assert_eq!(s, StatusCode::OK);
    let (_, qv) = t.call("GET", "/api/queue", None).await;
    assert_eq!(qv["items"][0]["join_tpl"], "short");
    assert_eq!(qv["items"][0]["track"]["id"], ids[0]);
    let swapped = json!([{"track_id": ids[0], "join_tpl": "short", "join_bars": 8, "join_next": ids[1]}, {"track_id": ids[2]}]);
    t.call("PUT", "/api/queue", Some(json!({"items": swapped}))).await;
    let (_, qv) = t.call("GET", "/api/queue", None).await;
    assert_eq!(qv["items"][0]["join_tpl"], Value::Null, "次の曲が変わったつなぎは標準に戻す");

    // ---- ミックス・★・履歴（test_mixes.py）
    let (_, m) = t.call("POST", "/api/mixes", Some(json!({"name": "M"}))).await;
    let mid = m["id"].as_i64().unwrap();
    let (_, m2) = t.call("POST", "/api/mixes", Some(json!({"name": "M"}))).await;
    assert_eq!(m2["name"], "M (2)");
    let (_, d) = t.call("POST", &format!("/api/mixes/{mid}/duplicate"), None).await;
    assert_eq!(d["name"], "M のコピー");
    let (_, r) = t.call("PATCH", &format!("/api/mixes/{mid}"), Some(json!({"name": "X"}))).await;
    assert_eq!(r["name"], "X");
    let (_, pl) = t.call("POST", "/api/mixes/played", None).await;
    assert_eq!(pl["mix"], m2["id"], "キューは最後に保存したミックスのまま");
    let (_, del) = t.call("DELETE", &format!("/api/mixes/{mid}"), None).await;
    assert_eq!(del["ok"], true);
    let (_, ex) = t.call("POST", "/api/mixes/after-export", Some(json!({"path": "/x/書き出し.wav", "format": "wav", "seconds": 10}))).await;
    assert_eq!(ex["exports"][0]["format"], "wav");
    let (_, rt) = t.call("POST", &format!("/api/tracks/{}/rating", ids[0]), Some(json!({"rating": 9}))).await;
    assert_eq!(rt["rating"], 5);
    let (_, rec) = t.call("POST", "/api/plays", Some(json!({"track_ids": [ids[1]], "source": "deck"}))).await;
    assert_eq!(rec["recorded"], 1);
    let (_, rec) = t.call("POST", "/api/plays", Some(json!({"track_ids": [ids[1]], "source": "deck"}))).await;
    assert_eq!(rec["recorded"], 0, "10 分以内の同じ記録は数えない");
    let (_, hist) = t.call("GET", "/api/plays", None).await;
    assert!(hist["items"].as_array().unwrap().len() >= 2);

    // ---- 再生用 PCM（test_pcm.py）: planar で、全体のデコードと同じ
    let (s, pcm, hd) = t.raw("GET", &format!("/api/tracks/{}/pcm?offset=100&frames=1000", ids[0]), None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(pcm.len(), 1000 * 2 * 4);
    let total: usize = hd["x-total-frames"].to_str().unwrap().parse().unwrap();
    let full = djmix::audio::decode::decode(music.join("Jazz/a.mp3")).unwrap();
    assert_eq!(total, full.frames());
    let l0 = f32::from_le_bytes(pcm[0..4].try_into().unwrap());
    let r0 = f32::from_le_bytes(pcm[4000..4004].try_into().unwrap());
    assert_eq!(l0, full.data[200]);
    assert_eq!(r0, full.data[201]);

    // ---- 音楽フォルダを増やす・外す（外した曲は消さずに隠す）
    let (s, _) = t.call("PUT", "/api/settings", Some(json!({"music_roots": ["/no/such/folder"]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let roots = json!([music.to_string_lossy(), music2.to_string_lossy()]);
    t.call("PUT", "/api/settings", Some(json!({"music_roots": roots}))).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    wait_scan(&t).await;
    let (_, lib) = t.call("GET", "/api/library", None).await;
    assert_eq!(lib["total"], 4);
    t.call("PUT", "/api/settings", Some(json!({"music_roots": [music2.to_string_lossy()]}))).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    wait_scan(&t).await;
    let (_, lib) = t.call("GET", "/api/library", None).await;
    assert_eq!(lib["total"], 1);
    let (_, st) = t.call("GET", "/api/stats", None).await;
    let hidden = st["roots"].as_array().unwrap().iter().find(|r| r["active"] == false).unwrap();
    assert_eq!(hidden["tracks"], 3);

    // ---- 走査は差分で、消えた曲は消す
    std::fs::remove_file(music2.join("d.flac")).unwrap();
    t.call("POST", "/api/library/rescan", None).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    wait_scan(&t).await;
    let (_, sc) = t.call("GET", "/api/library/scan", None).await;
    assert_eq!(sc["removed"], 1);

    // ---- 割り当て・設定
    let (_, maps) = t.call("GET", "/api/mappings", None).await;
    assert!(maps["mappings"].as_array().unwrap().iter().any(|m| m == "default"));
    let (s, _) = t.call("PUT", "/api/mappings/..%2Fx", Some(json!({"entries": []}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, set) = t.call("PUT", "/api/settings", Some(json!({"lufs_target": -40, "tempo_range": 0.33, "lib_view": {"sort": "bpm"}}))).await;
    assert_eq!(set["lufs_target"], -23.0);
    assert_eq!(set["tempo_range"], 0.08);
    assert_eq!(set["lib_view"]["sort"], "bpm");
    state.shutdown();
}
