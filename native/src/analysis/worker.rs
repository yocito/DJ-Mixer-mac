//! 優先度付きの解析キュー（djmix/analysis/worker.py）。
//!
//! 優先度: 0=デッキに読み込んだ曲（最優先） / 1=ミックスキューに入れた曲 / 2=ユーザーが明示的に指示した曲。
//! 同じ曲を高い優先度で頼み直すと、その優先度で処理される。
use super::{cache, cutpoints, pipeline, sections};
use crate::{db, util};
use parking_lot::{Condvar, Mutex};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

pub const PRIO_DECK: i64 = 0;
pub const PRIO_QUEUE: i64 = 1;
pub const PRIO_MANUAL: i64 = 2;

#[derive(Default)]
struct Inner {
    q: BinaryHeap<Reverse<(i64, u64, i64)>>, // (優先度, 順番, 曲)
    want: HashMap<i64, i64>,
    seq: u64,
    running: Option<i64>,
    recent: Vec<Value>,
    claimed: HashSet<i64>,
    started: bool,
}

#[derive(Clone, Default)]
pub struct AnalysisWorker {
    inner: Arc<Mutex<Inner>>,
    cv: Arc<Condvar>,
}

/// 解析の後にすること（おまかせ用の要約・区間の候補の作り直し・パートの量）。worker と bulk で共通。
pub fn after_analysis(tid: i64, path: &str) {
    sections::forget(tid);
    cutpoints::forget(tid);
    if let (Some(a), Some(feats)) = (cache::read(tid), cache::bars_array(tid)) {
        if let Err(e) = crate::stubs::desc::update(tid, &a, &feats, None) {
            log::warn!("desc update failed for {tid}: {e:#}");
        }
    }
    request_stems(tid, path, false);
}

/// パートの量（音源分離）を裏で求め、終わったらおまかせ用の要約も作り直す。
pub fn request_stems(tid: i64, path: &str, audio: bool) -> &'static str {
    if !crate::stubs::stems::available() {
        return "unavailable";
    }
    crate::stubs::stems::request(
        tid,
        path.to_string(),
        Some(Box::new(|tid, arr| {
            if let (Some(a), Some(feats)) = (cache::read(tid), cache::bars_array(tid)) {
                let _ = crate::stubs::desc::update(tid, &a, &feats, Some(arr));
            }
        })),
        audio,
    )
}

impl AnalysisWorker {
    pub fn new() -> AnalysisWorker {
        AnalysisWorker::default()
    }

    fn start(&self) {
        let mut g = self.inner.lock();
        if g.started {
            return;
        }
        g.started = true;
        drop(g);
        let me = self.clone();
        std::thread::Builder::new().name("analysis".into()).spawn(move || me.run()).ok();
    }

    /// 解析を依頼する。戻り値: 'done'（解析済み）/ 'queued' / 'running'。
    pub fn request(&self, track_id: i64, priority: i64, force: bool) -> &'static str {
        if !force && cache::read(track_id).is_some() {
            let _ = db::set_status(track_id, "done", &[]);
            return "done";
        }
        {
            let mut g = self.inner.lock();
            if g.running == Some(track_id) || g.claimed.contains(&track_id) {
                return "running";
            }
            if let Some(&cur) = g.want.get(&track_id) {
                if cur <= priority {
                    return "queued";
                }
            }
            g.want.insert(track_id, priority);
            g.seq += 1;
            let seq = g.seq;
            g.q.push(Reverse((priority, seq, track_id)));
        }
        let _ = db::set_status(track_id, "queued", &[]);
        self.cv.notify_one();
        self.start();
        "queued"
    }

    /// まとめて解析がこの曲を預かる。こちらで解析中・依頼待ちなら false。
    pub fn claim(&self, track_id: i64) -> bool {
        let mut g = self.inner.lock();
        if g.running == Some(track_id) || g.want.contains_key(&track_id) || g.claimed.contains(&track_id) {
            return false;
        }
        g.claimed.insert(track_id);
        true
    }

    pub fn release(&self, track_id: i64) {
        self.inner.lock().claimed.remove(&track_id);
    }

    /// まとめて解析で終わった曲も、画面の「直近に終わった曲」に載せる。
    pub fn notify(&self, mut result: Value) {
        result["t"] = json!(util::now());
        let mut g = self.inner.lock();
        g.recent.push(result);
        let n = g.recent.len();
        if n > 200 {
            g.recent.drain(0..n - 200);
        }
    }

    pub fn status(&self) -> Value {
        let g = self.inner.lock();
        let n = g.recent.len();
        json!({"pending": g.want.len(), "running": g.running, "recent": g.recent[n.saturating_sub(50)..]})
    }

    fn run(&self) {
        loop {
            let tid = {
                let mut g = self.inner.lock();
                loop {
                    if let Some(Reverse((prio, _, tid))) = g.q.pop() {
                        if g.want.get(&tid) != Some(&prio) {
                            continue; // より高い優先度で積み直された古い要素
                        }
                        g.want.remove(&tid);
                        g.running = Some(tid);
                        break tid;
                    }
                    self.cv.wait(&mut g);
                }
            };
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.analyze(tid)));
            if r.is_err() {
                log::error!("analysis panicked for {tid}");
                let _ = db::set_status(tid, "failed", &[("analysis_error", SqlValue::Text("解析中に内部エラーが起きました".into()))]);
            }
            self.inner.lock().running = None;
        }
    }

    fn analyze(&self, tid: i64) {
        let Some(path) = crate::library::query::track_path(tid) else { return };
        let _ = db::set_status(tid, "running", &[]);
        let t0 = util::now();
        let result = match pipeline::analyze_file(&path).and_then(|(a, wave, feats, dprob)| {
            cache::write(tid, &a, &wave, &feats, &dprob)?;
            Ok(a)
        }) {
            Ok(a) => {
                after_analysis(tid, &path);
                let _ = db::set_status(tid, "done", &done_cols(&a));
                log::info!(
                    "analyzed {} in {:.1}s: {:.1} BPM, meter {}, class {}",
                    path,
                    util::now() - t0,
                    a.bpm,
                    a.meter,
                    a.beat_class
                );
                json!({"id": tid, "status": "done"})
            }
            Err(e) => {
                log::error!("analysis failed: {path}: {e:#}");
                let msg = util::truncate_chars(&format!("{e:#}"), 500);
                let _ = db::set_status(tid, "failed", &[("analysis_error", SqlValue::Text(msg.clone()))]);
                json!({"id": tid, "status": "failed", "error": util::truncate_chars(&msg, 200)})
            }
        };
        self.notify(result);
    }
}

/// 解析が終わった曲の DB の列
pub fn done_cols(a: &super::types::Analysis) -> Vec<(&'static str, SqlValue)> {
    vec![
        ("analysis_version", SqlValue::Integer(cache::ANALYSIS_VERSION as i64)),
        ("analysis_error", SqlValue::Null),
        ("bpm", SqlValue::Real(a.bpm)),
        ("camelot", a.key.camelot.clone().map(SqlValue::Text).unwrap_or(SqlValue::Null)),
        ("lufs", SqlValue::Real(a.lufs)),
        ("beat_class", SqlValue::Text(a.beat_class.clone())),
    ]
}
