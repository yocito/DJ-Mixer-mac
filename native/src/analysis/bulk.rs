//! ライブラリのまとめて解析（djmix/analysis/bulk.py）。
//!
//! Python 版は子プロセス（最大 4 本）で解析していたが、こちらはスレッドで解析する（Rust は GIL が無いので並列に動く）。
//! - 自動では始めない。start() で始め、pause() で止める（解析中の曲は終わるまで待つ）。アプリを閉じると止まり、次は未解析の曲から
//! - どちらかのデッキが再生中は、同時に解析する曲を 1 曲に減らす（set_playing）
//! - 範囲（ジャンルのまとまり）を指定すると、その曲を先に解析する。rest=false ならその範囲だけ
//! - デッキ・キューからの解析依頼（worker.rs）とは、同じ曲を同時に解析しないよう「預かり」で調整する
//! - 解析の間は Mac をスリープさせない（caffeinate）。解析のスレッドは優先度を下げる（QoS utility）
use super::worker::{after_analysis, done_cols, AnalysisWorker};
use super::{cache, pipeline};
use crate::library::roots::where_sql;
use crate::planner::genres::group_of;
use crate::{db, util};
use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const MIN_SEC: i64 = 30;
pub const MAX_SEC: i64 = 1200;
pub const POOL_LOST: &str = "A process in the process pool was terminated abruptly";

#[derive(Clone, Default)]
struct State {
    running: bool,
    groups: Vec<String>,
    rest: bool,
    stems: bool,
    done: i64,
    failed: i64,
    started: Option<f64>,
    rate: Option<f64>,
    in_flight: usize,
    error: Option<String>,
    restarts: i64,
    closing: bool,
}

#[derive(Clone)]
pub struct BulkAnalyzer {
    worker: AnalysisWorker,
    max_procs: usize,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    playing: Arc<AtomicBool>,
}

fn elig() -> (String, Vec<SqlValue>) {
    let (rw, args) = where_sql("");
    (format!("duration BETWEEN {MIN_SEC} AND {MAX_SEC} AND {rw}"), args)
}

/// 解析のスレッドの優先度を下げる（再生や画面の操作を邪魔しない）
fn lower_priority() {
    #[cfg(target_os = "macos")]
    unsafe {
        // QOS_CLASS_UTILITY = 0x11
        extern "C" {
            fn pthread_set_qos_class_self_np(qos: u32, rel: i32) -> i32;
        }
        pthread_set_qos_class_self_np(0x11, 0);
    }
    #[cfg(not(target_os = "macos"))]
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
}

/// 解析の間、Mac をスリープさせない（このプロセスが続く間だけ。終わったら止める）
struct KeepAwake(Option<std::process::Child>);

impl KeepAwake {
    fn new() -> KeepAwake {
        if cfg!(target_os = "macos") {
            let pid = std::process::id().to_string();
            KeepAwake(std::process::Command::new("/usr/bin/caffeinate").args(["-i", "-w", &pid]).spawn().ok())
        } else {
            KeepAwake(None)
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        if let Some(c) = self.0.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl BulkAnalyzer {
    pub fn new(worker: AnalysisWorker) -> BulkAnalyzer {
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        BulkAnalyzer {
            worker,
            max_procs: (n / 2).clamp(1, 4),
            state: Arc::new(Mutex::new(State { rest: true, ..Default::default() })),
            stop: Arc::new(AtomicBool::new(false)),
            playing: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn start(&self, groups: Vec<String>, rest: bool, stems: bool) -> bool {
        {
            let mut s = self.state.lock();
            if s.running {
                s.groups = groups; // 実行中なら範囲だけ差し替える（次に選ぶ曲から効く）
                s.rest = rest;
                return false;
            }
            self.stop.store(false, Ordering::SeqCst);
            *s = State { running: true, groups, rest, stems, started: Some(util::now()), ..Default::default() };
        }
        let me = self.clone();
        std::thread::Builder::new().name("bulk-analysis".into()).spawn(move || me.run()).ok();
        true
    }

    pub fn pause(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.state.lock().closing = true;
    }

    pub fn set_playing(&self, playing: bool) {
        self.playing.store(playing, Ordering::SeqCst);
    }

    pub fn status(&self) -> Value {
        let (el, args) = elig();
        let (total, analyzed, failed): (i64, i64, i64) = db::with(|c| {
            Ok(c.query_row(
                &format!("SELECT COUNT(*), SUM(analysis_status = 'done'), SUM(analysis_status = 'failed') FROM tracks WHERE {el}"),
                rusqlite::params_from_iter(args.iter()),
                |r| Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0), r.get::<_, Option<i64>>(2)?.unwrap_or(0))),
            )?)
        })
        .unwrap_or((0, 0, 0));
        let st = self.state.lock().clone();
        let left_all = total - analyzed - failed;
        let mut left = left_all;
        if st.running && !st.groups.is_empty() && !st.rest {
            let gs: HashSet<String> = st.groups.iter().cloned().collect();
            left = db::with(|c| {
                let mut s = c.prepare(&format!("SELECT genre FROM tracks WHERE analysis_status IN ('none', 'running') AND {el}"))?;
                let n = s
                    .query_map(rusqlite::params_from_iter(args.iter()), |r| r.get::<_, Option<String>>(0))?
                    .flatten()
                    .filter(|g| gs.contains(group_of(g.as_deref())))
                    .count() as i64;
                Ok(n)
            })
            .unwrap_or(left_all);
        }
        let playing = self.playing.load(Ordering::SeqCst);
        let procs = if playing { 1 } else { self.max_procs };
        json!({"running": st.running, "groups": st.groups, "rest": st.rest, "stems": st.stems, "done": st.done, "failed": st.failed,
               "started": st.started, "rate": st.rate, "in_flight": st.in_flight, "error": st.error, "restarts": st.restarts,
               "closing": st.closing, "total": total, "analyzed": analyzed, "failed_total": failed, "left": left,
               "left_all": left_all, "playing": playing, "procs": procs,
               "eta_sec": st.rate.filter(|r| *r > 0.0).map(|r| (left as f64 / r).round() as i64)})
    }

    /// 未解析の曲を n 曲。範囲（ジャンルのまとまり）の曲を先に。
    fn next_ids(&self, n: usize, skip: &HashSet<i64>) -> Vec<(i64, String)> {
        let (groups, rest) = {
            let s = self.state.lock();
            (s.groups.iter().cloned().collect::<HashSet<_>>(), s.rest)
        };
        let (el, args) = elig();
        let rows: Vec<(i64, String, Option<String>)> = db::with(|c| {
            let mut s =
                c.prepare(&format!("SELECT id, path, genre FROM tracks WHERE analysis_status = 'none' AND {el} ORDER BY folder, path"))?;
            let v = s.query_map(rusqlite::params_from_iter(args.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.flatten().collect();
            Ok(v)
        })
        .unwrap_or_default();
        let mut first = vec![];
        let mut later = vec![];
        for (id, path, genre) in rows {
            if skip.contains(&id) {
                continue;
            }
            if groups.is_empty() || groups.contains(group_of(genre.as_deref())) {
                first.push((id, path));
            } else {
                later.push((id, path));
            }
            if first.len() >= n {
                break;
            }
        }
        if rest {
            first.extend(later);
        }
        first.truncate(n);
        first
    }

    pub fn failed_list(&self) -> Result<Vec<Value>> {
        let (el, mut args) = elig();
        args.push(SqlValue::Integer(500));
        db::with(|c| {
            let rows = db::query_json(
                c,
                &format!(
                    "SELECT id, title, artist, album, path, analysis_error AS error FROM tracks WHERE analysis_status = 'failed' \
                     AND {el} ORDER BY artist COLLATE NOCASE, album COLLATE NOCASE, path LIMIT ?"
                ),
                &db::params_of(&args),
            )?;
            Ok(rows.into_iter().map(Value::Object).collect())
        })
    }

    pub fn retry_failed(&self) -> Result<usize> {
        let (el, args) = elig();
        db::tx(|c| {
            Ok(c.execute(
                &format!("UPDATE tracks SET analysis_status = 'none', analysis_error = NULL WHERE analysis_status = 'failed' AND {el}"),
                rusqlite::params_from_iter(args.iter()),
            )?)
        })
    }

    /// 起動時: 解析が途中で止まったせいで「失敗」になった曲（曲のせいではない）を未解析に戻す。
    pub fn recover(&self) -> Result<usize> {
        let n = db::tx(|c| {
            Ok(c.execute(
                "UPDATE tracks SET analysis_status = 'none', analysis_error = NULL WHERE analysis_status = 'failed' AND analysis_error LIKE ?",
                [format!("{POOL_LOST}%")],
            )?)
        })?;
        if n > 0 {
            log::info!("bulk analysis: {n} tracks failed by a lost worker were reset to unanalyzed");
        }
        Ok(n)
    }

    fn run(&self) {
        let _awake = KeepAwake::new();
        let pending: Arc<Mutex<HashSet<i64>>> = Arc::new(Mutex::new(HashSet::new()));
        let t_hist: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(vec![]));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let mut handles = vec![];
        loop {
            let stems = self.state.lock().stems;
            let limit = if self.playing.load(Ordering::SeqCst) {
                1
            } else if stems {
                self.max_procs.min(2)
            } else {
                self.max_procs
            };
            let mut started_any = false;
            if !self.stop.load(Ordering::SeqCst) {
                let busy = pending.lock().len();
                if limit > busy {
                    let skip = pending.lock().clone();
                    for (tid, path) in self.next_ids(limit - busy, &skip) {
                        if !self.worker.claim(tid) {
                            continue; // デッキ・キューの解析が先に扱っている
                        }
                        let _ = db::set_status(tid, "running", &[]);
                        pending.lock().insert(tid);
                        started_any = true;
                        let me = self.clone();
                        let pending = pending.clone();
                        let t_hist = t_hist.clone();
                        let tx = tx.clone();
                        handles.push(std::thread::spawn(move || {
                            lower_priority();
                            me.analyze_one(tid, &path, stems, &t_hist);
                            pending.lock().remove(&tid);
                            let _ = tx.send(());
                        }));
                    }
                }
            }
            let inflight = pending.lock().len();
            self.state.lock().in_flight = inflight;
            if inflight == 0 && !started_any {
                break; // 止めた、または解析する曲が残っていない
            }
            let _ = rx.recv_timeout(std::time::Duration::from_millis(500));
            handles.retain(|h| !h.is_finished());
        }
        for h in handles {
            let _ = h.join();
        }
        let mut s = self.state.lock();
        s.running = false;
        s.in_flight = 0;
        log::info!("bulk analysis ended: done {}, failed {}", s.done, s.failed);
    }

    fn analyze_one(&self, tid: i64, path: &str, with_stems: bool, t_hist: &Mutex<Vec<f64>>) {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<super::types::Analysis> {
            let (a, wave, feats, dprob) = pipeline::analyze_file(path)?;
            cache::write(tid, &a, &wave, &feats, &dprob)?;
            if with_stems && crate::analysis::stems::available() {
                if let Some(arr) = crate::analysis::stems::compute_for_track(tid, path)? {
                    let _ = arr;
                }
            }
            Ok(a)
        }));
        let result = match r {
            Ok(Ok(a)) => {
                after_analysis(tid, path);
                let _ = db::set_status(tid, "done", &done_cols(&a));
                self.state.lock().done += 1;
                json!({"id": tid, "status": "done"})
            }
            Ok(Err(e)) => {
                log::warn!("bulk analysis failed for {tid}: {e:#}");
                let msg = util::truncate_chars(&format!("{e:#}"), 500);
                let _ = db::set_status(tid, "failed", &[("analysis_error", SqlValue::Text(msg.clone()))]);
                self.state.lock().failed += 1;
                json!({"id": tid, "status": "failed", "error": util::truncate_chars(&msg, 200)})
            }
            Err(_) => {
                let msg = "解析中に内部エラーが起きました".to_string();
                let _ = db::set_status(tid, "failed", &[("analysis_error", SqlValue::Text(msg.clone()))]);
                self.state.lock().failed += 1;
                json!({"id": tid, "status": "failed", "error": msg})
            }
        };
        // 直近 2 分の速さ
        {
            let now = util::now();
            let mut h = t_hist.lock();
            h.push(now);
            h.retain(|t| *t > now - 120.0);
            let mut s = self.state.lock();
            let started = s.started.unwrap_or(now);
            let span = now - started.max(h[0] - 1.0);
            s.rate = if span > 20.0 { Some(util::round(h.len() as f64 / span, 3)) } else { None };
        }
        // 止めたときに途中だった曲は未解析に戻す（Python 版と同じ）。ここでは解析し終えているので不要
        self.worker.release(tid);
        self.worker.notify(result);
    }
}
