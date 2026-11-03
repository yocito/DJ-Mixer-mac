//! デッキの音（高速書き出しのストレッチ）。
//!
//! Python 版の既定は、画面の WebAssembly の Signalsmith Stretch を OfflineAudioContext で動かして区間ごとに計算していた
//! （python-stretch は呼び出しごとに独立して処理し、区切りで音量が落ちたため）。Mac 版は同じ Signalsmith の C++ 版
//! （signalsmith-stretch クレート。再生と同じ preset_default）を、デッキ・曲ごとに区切らずに流し続けて計算する:
//!
//! - 刻み k → k+1 の間に、入力位置 plan.pos[k] → plan.pos[k+1] の間のサンプルをちょうど入れ、出力は 512 サンプル
//!   （入力位置は整数に丸めた累計なので、ずれが溜まらない）
//! - ストレッチの遅れの補正: 入力は input_latency だけ先まで入れ、出力は output_latency だけ後で取り出す。
//!   これで「最終の出力の時刻 t に鳴る音 = 入力位置 pos(t)」になる（2 つのデッキの拍が 1ms 以内でそろう。tests/export_render.rs）
//! - 鳴り始めは、位置を手前へ延ばして（PRE_STEPS 刻み）から計算し、その分は捨てる（立ち上がりがフェードインにならないように）
//! - ループの折り返し（plan.wraps）は、入れる音を折り返しの所でつなぐ（ストレッチから見ると続いた音。再生のノードのループと同じ）
//! - それ以外の飛び（同じデッキに次の曲・位置の不連続）は、区間を分けて新しく始める（seek）
//! - キー合わせは set_transpose_factor_semitones(shift)（再生の keyShift と同じ。tonality は再生の既定 8000Hz）
//! - パートの音（8ch）は、使う区間（segments_of の stems）だけ、もう 1 つのストレッチで同じ位置どおりに計算して pg・pm で混ぜる
use super::mixer::per_sample;
use super::render::{segments_of, Plan, Track, SR, STEP};
use crate::analysis::stems;
use crate::audio::decode::decode;
use anyhow::Result;
use signalsmith_stretch::Stretch;
use std::sync::Arc;
use std::sync::mpsc::{sync_channel, Receiver};

/// 分離した音の読み方: (track_id, 開始フレーム, フレーム数) → (8ch の planar, 曲全体のフレーム数)
pub type StemReader = Arc<dyn Fn(i64, usize, usize) -> Result<(Vec<Vec<f32>>, usize)> + Send + Sync>;

pub fn default_stem_reader() -> StemReader {
    Arc::new(stems::read_audio)
}

/// 再生のノード（SignalsmithStretch.mjs）の既定の tonalityHz
pub const TONALITY_HZ: f32 = 8000.0;
/// 鳴り始めの前に余分に計算する刻み（ブロック 0.12 秒 + 間隔 0.03 秒より長く）
pub const PRE_STEPS: usize = 16;
/// 1 刻みにこれより多く・逆向きに進んだら飛びとみなす（サンプル）
const MAX_ADVANCE: f64 = 8.0 * STEP as f64;

/// 再生と同じ設定のストレッチ（preset_default）
pub fn new_stretch(channels: usize, shift: f64) -> Stretch {
    let mut s = Stretch::preset_default(channels as u32, SR);
    if shift != 0.0 {
        s.set_transpose_factor_semitones(shift as f32, Some(TONALITY_HZ / SR as f32));
    }
    s
}

/// ストレッチに入れる音（インターリーブ、ch チャンネル）。data の先頭は曲の frame0 フレーム目
#[derive(Clone)]
pub struct Source {
    pub ch: usize,
    pub frame0: i64,
    pub data: Arc<Vec<f32>>,
}

impl Source {
    pub fn frames(&self) -> i64 {
        (self.data.len() / self.ch) as i64
    }
    /// 曲の frame のサンプル（範囲の外は 0）
    #[inline]
    fn copy_frame(&self, frame: i64, out: &mut [f32]) {
        let i = frame - self.frame0;
        if i >= 0 && i < self.frames() {
            let a = i as usize * self.ch;
            out.copy_from_slice(&self.data[a..a + self.ch]);
        } else {
            out.iter_mut().for_each(|v| *v = 0.0);
        }
    }
}

/// 1 つの区間（同じデッキ・同じ曲が続く刻み s0..s1）の、刻み → 入力の位置（ループの折り返しを足し戻した、続いた位置）
#[derive(Debug, Clone)]
pub struct RunPath {
    pub s0: usize,
    pub s1: usize,
    /// 刻みごとの続いた位置（サンプル。s0..s1）
    u: Vec<f64>,
    /// 頭・終わりの外側へ延ばすときの 1 刻みの進み（サンプル）
    v_start: f64,
    v_end: f64,
    /// 折り返し（続いた位置での継ぎ目, 戻る長さ（サンプル））
    seams: Vec<(i64, i64)>,
}

impl RunPath {
    pub fn new(plan: &Plan, d: usize, s0: usize, s1: usize) -> RunPath {
        let sr = SR as f64;
        let mut seams = vec![];
        let mut acc = 0i64;
        let mut u = Vec::with_capacity(s1 - s0);
        for k in s0..s1 {
            if k > s0 {
                if let Some(w) = plan.wraps.iter().find(|w| w.deck == d && w.k == k) {
                    let len = ((w.end - w.start) * sr).round() as i64;
                    seams.push(((w.end * sr).round() as i64 + acc, len));
                    acc += len;
                }
            }
            u.push(plan.pos[d][k] * sr + acc as f64);
        }
        let lim = |v: f64| v.clamp(0.25 * STEP as f64, 4.0 * STEP as f64);
        let v_start = if u.len() >= 2 { lim(u[1] - u[0]) } else { STEP as f64 };
        let v_end = if u.len() >= 2 { lim(u[u.len() - 1] - u[u.len() - 2]) } else { STEP as f64 };
        RunPath { s0, s1, u, v_start, v_end, seams }
    }

    /// 刻み k（区間の外も延ばして）の続いた位置（サンプル）
    pub fn at(&self, k: i64) -> f64 {
        let (s0, s1) = (self.s0 as i64, self.s1 as i64);
        if k < s0 {
            self.u[0] - (s0 - k) as f64 * self.v_start
        } else if k >= s1 {
            self.u[self.u.len() - 1] + (k - (s1 - 1)) as f64 * self.v_end
        } else {
            self.u[(k - s0) as usize]
        }
    }

    /// 続いた位置 j → 曲のフレーム
    #[inline]
    pub fn frame(&self, j: i64) -> i64 {
        let mut f = j;
        for &(seam, len) in &self.seams {
            if j >= seam {
                f -= len;
            }
        }
        f
    }

    /// 刻み k0..k1 を計算するのに読む曲のフレームの範囲（前後の余白つき）
    pub fn frame_range(&self, k0: usize, k1: usize, margin: i64) -> (i64, i64) {
        let (mut a, mut b) = (i64::MAX, i64::MIN);
        for k in (k0 as i64 - PRE_STEPS as i64 - 2)..=(k1 as i64 + 64) {
            let f = self.frame(self.at(k).round() as i64);
            a = a.min(f);
            b = b.max(f);
        }
        ((a - margin).max(0), (b + margin).max(0))
    }
}

/// 1 つの区間を流し続けるストレッチ（出力は最終の時刻に合わせて取り出す）
pub struct Streamer {
    ch: usize,
    stretch: Stretch,
    in_lat: i64,
    out_lat: i64,
    path: RunPath,
    src: Source,
    /// 次に処理する刻み（ストレッチの時刻）
    k_next: i64,
    /// 次に入れる入力（続いた位置）
    j_next: i64,
    fifo: Vec<f32>,
    /// fifo の先頭の最終の時刻（サンプル）
    fifo_t0: i64,
    inbuf: Vec<f32>,
    outbuf: Vec<f32>,
}

impl Streamer {
    /// path の刻み start から鳴らす（その PRE_STEPS 刻み前から計算しておく）
    pub fn new(ch: usize, shift: f64, path: RunPath, start: usize, src: Source) -> Streamer {
        let mut stretch = new_stretch(ch, shift);
        let in_lat = stretch.input_latency() as i64;
        let out_lat = stretch.output_latency() as i64;
        let k_start = start as i64 - PRE_STEPS as i64;
        let j0 = path.at(k_start).round() as i64 + in_lat;
        // 前の音（ブロック + 間隔より長く）を seek で入れておく
        let hist = (SR as f64 * 0.2) as i64;
        let mut buf = vec![0.0f32; hist as usize * ch];
        for (n, j) in (j0 - hist..j0).enumerate() {
            src.copy_frame(path.frame(j), &mut buf[n * ch..(n + 1) * ch]);
        }
        let rate = path.v_start / STEP as f64;
        stretch.seek(&buf, rate);
        Streamer {
            ch,
            stretch,
            in_lat,
            out_lat,
            src,
            k_next: k_start,
            j_next: j0,
            fifo: vec![],
            fifo_t0: k_start * STEP as i64 - out_lat,
            path,
            inbuf: vec![],
            outbuf: vec![0.0; STEP * ch],
        }
    }

    /// (input_latency, output_latency)
    pub fn latency(&self) -> (i64, i64) {
        (self.in_lat, self.out_lat)
    }

    fn step(&mut self) {
        let k = self.k_next;
        let j1 = self.path.at(k + 1).round() as i64 + self.in_lat;
        let n_in = (j1 - self.j_next).max(0) as usize;
        self.inbuf.resize(n_in * self.ch, 0.0);
        for n in 0..n_in {
            let f = self.path.frame(self.j_next + n as i64);
            self.src.copy_frame(f, &mut self.inbuf[n * self.ch..(n + 1) * self.ch]);
        }
        self.stretch.process(&self.inbuf, &mut self.outbuf);
        self.fifo.extend_from_slice(&self.outbuf);
        self.j_next += n_in as i64;
        self.k_next += 1;
    }

    /// 最終の時刻 t0..t1（サンプル）の音を out（インターリーブ、ch チャンネル）へ。時刻は前へしか進まない
    pub fn fill(&mut self, t0: i64, t1: i64, out: &mut [f32]) {
        while self.fifo_t0 + ((self.fifo.len() / self.ch) as i64) < t1 {
            self.step();
        }
        // 使い終わった所を捨てる
        if t0 > self.fifo_t0 {
            let drop = ((t0 - self.fifo_t0) as usize * self.ch).min(self.fifo.len());
            self.fifo.drain(..drop);
            self.fifo_t0 = t0;
        }
        let a = ((t0 - self.fifo_t0) as usize) * self.ch;
        let n = (t1 - t0) as usize * self.ch;
        out[..n].copy_from_slice(&self.fifo[a..a + n]);
    }
}

#[derive(Debug, Clone)]
struct Run {
    idx: usize,
    s0: usize,
    s1: usize,
}

#[derive(Debug, Clone)]
struct StemRun {
    idx: usize,
    track_id: i64,
    /// パートの音の区間
    s0: usize,
    s1: usize,
    /// その曲の区間（runs の番号。位置の計算はそれと同じ）
    run: usize,
}

/// 1 つの区間を別のスレッドで計算する（曲の読み込み → ストレッチ）。計算した音は FEED_STEPS 刻みずつ送る
struct RunFeed {
    rx: Receiver<Result<Vec<f32>>>,
    /// 受け取ってまだ使っていない音（インターリーブのステレオ）と、その頭の刻み
    buf: Vec<f32>,
    buf_s0: usize,
}

/// 区間の計算をこの刻み数ずつ送る（約 2 秒）
const FEED_STEPS: usize = 176;
/// 先に計算しておける量（FEED_STEPS 単位。約 3 分。次の曲を今の曲と並べて計算できるように）
const FEED_AHEAD: usize = 96;
/// これだけ先（刻み。約 3 分）に始まる区間は、もう計算を始める
const AHEAD_STEPS: usize = FEED_AHEAD * FEED_STEPS;

impl RunFeed {
    fn spawn(path: std::path::PathBuf, shift: f64, rp: RunPath) -> RunFeed {
        let (tx, rx) = sync_channel::<Result<Vec<f32>>>(FEED_AHEAD);
        let (s0, s1) = (rp.s0, rp.s1);
        std::thread::Builder::new()
            .name("stretch".into())
            .spawn(move || {
                let data = match decode(&path) {
                    Ok(d) => Arc::new(d.data),
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                let mut st = Streamer::new(2, shift, rp, s0, Source { ch: 2, frame0: 0, data });
                let mut a = s0;
                while a < s1 {
                    let b = s1.min(a + FEED_STEPS);
                    let mut v = vec![0.0f32; (b - a) * STEP * 2];
                    st.fill((a * STEP) as i64, (b * STEP) as i64, &mut v);
                    if tx.send(Ok(v)).is_err() {
                        return; // 使われなくなった（中止）
                    }
                    a = b;
                }
            })
            .expect("thread");
        RunFeed { rx, buf: vec![], buf_s0: s0 }
    }

    /// 刻み a..b の音を out へ（前へしか進まない）
    fn fill(&mut self, a: usize, b: usize, out: &mut [f32]) -> Result<()> {
        while self.buf_s0 + self.buf.len() / (STEP * 2) < b {
            match self.rx.recv() {
                Ok(Ok(v)) => self.buf.extend_from_slice(&v),
                Ok(Err(e)) => return Err(e),
                Err(_) => anyhow::bail!("ストレッチの計算が止まりました"),
            }
        }
        if a > self.buf_s0 {
            self.buf.drain(..(a - self.buf_s0) * STEP * 2);
            self.buf_s0 = a;
        }
        let n = (b - a) * STEP * 2;
        out[..n].copy_from_slice(&self.buf[..n]);
        Ok(())
    }
}

/// デッキ 1 つ分の音（操作の表どおりにストレッチする）。曲ごとの区間は別々のスレッドで計算し、
/// 今の区間と次の区間を並べて進める（2 つのデッキで最大 4 つ）
pub struct DeckAudio<'a> {
    d: usize,
    plan: &'a Plan,
    tracks: &'a [Track],
    runs: Vec<Run>,
    stem_runs: Vec<StemRun>,
    stems: StemReader,
    feeds: Vec<Option<RunFeed>>,
    stem: Option<(usize, Streamer)>,
}

impl<'a> DeckAudio<'a> {
    pub fn new(d: usize, tracks: &'a [Track], plan: &'a Plan, stems: StemReader) -> DeckAudio<'a> {
        // 同じ曲が続く区間を、位置の飛び（ループの折り返し以外）でさらに分ける
        let mut runs = vec![];
        let mut stem_runs = vec![];
        let segs = segments_of(plan, tracks);
        for g in segs.iter().filter(|g| g.deck == d && g.kind == "main") {
            let mut a = g.s0;
            for k in g.s0 + 1..g.s1 {
                let du = (plan.pos[d][k] - plan.pos[d][k - 1]) * SR as f64;
                let wrapped = plan.wraps.iter().any(|w| w.deck == d && w.k == k);
                if !wrapped && !(0.0..=MAX_ADVANCE).contains(&du) {
                    runs.push(Run { idx: g.idx, s0: a, s1: k });
                    a = k;
                }
            }
            runs.push(Run { idx: g.idx, s0: a, s1: g.s1 });
        }
        for g in segs.iter().filter(|g| g.deck == d && g.kind == "stems") {
            if let Some(ri) = runs.iter().position(|r| r.idx == g.idx && r.s0 <= g.s0 && g.s0 < r.s1) {
                let s1 = g.s1.min(runs[ri].s1);
                stem_runs.push(StemRun { idx: g.idx, track_id: g.track_id, s0: g.s0, s1, run: ri });
            }
        }
        let feeds = runs.iter().map(|_| None).collect();
        DeckAudio { d, plan, tracks, runs, stem_runs, stems, feeds, stem: None }
    }

    /// 曲が終わったデッキは、使い終わった区間を手放す
    pub fn release(&mut self) {
        self.stem = None;
    }

    /// 刻み c1 から AHEAD_STEPS 刻み先までに始まる区間の計算を始めておく（止まっているデッキでも呼ぶ）
    pub fn prefetch(&mut self, c1: usize) {
        for ri in 0..self.runs.len() {
            let r = &self.runs[ri];
            if r.s1 > c1 && r.s0 < c1 + AHEAD_STEPS {
                self.start_feed(ri);
            }
        }
    }

    fn start_feed(&mut self, ri: usize) {
        if ri >= self.runs.len() || self.feeds[ri].is_some() {
            return;
        }
        let r = &self.runs[ri];
        let t = &self.tracks[r.idx];
        let rp = RunPath::new(self.plan, self.d, r.s0, r.s1);
        self.feeds[ri] = Some(RunFeed::spawn(t.path.clone(), t.shift, rp));
    }

    fn ensure_stem(&mut self, si: usize) -> Result<()> {
        if self.stem.as_ref().map(|m| m.0 == si).unwrap_or(false) {
            return Ok(());
        }
        self.stem = None;
        let g = self.stem_runs[si].clone();
        let r = &self.runs[g.run];
        // 位置はその曲の区間と同じ計算（パートの区間の頭の PRE_STEPS 刻み前から）
        let path = RunPath::new(self.plan, self.d, r.s0, r.s1);
        let (a, b) = path.frame_range(g.s0, g.s1, (SR as f64 * 0.5) as i64);
        let (planar, _total) = (self.stems)(g.track_id, a as usize, (b - a).max(0) as usize)?;
        if planar.len() != 8 {
            anyhow::bail!("分離した音のチャンネル数が違います（{}）", planar.len());
        }
        let n = planar.iter().map(|v| v.len()).min().unwrap_or(0);
        let mut data = vec![0.0f32; n * 8];
        for (c, v) in planar.iter().enumerate() {
            for i in 0..n {
                data[i * 8 + c] = v[i];
            }
        }
        let src = Source { ch: 8, frame0: a, data: Arc::new(data) };
        self.stem = Some((si, Streamer::new(8, self.tracks[g.idx].shift, path, g.s0, src)));
        Ok(())
    }

    /// 刻み c0..c1 の音（インターリーブのステレオ。止まっている刻みは 0）。c0 は前の呼び出しの c1 から続けること
    pub fn render(&mut self, c0: usize, c1: usize) -> Result<Vec<f32>> {
        self.prefetch(c1);
        let n = (c1 - c0) * STEP;
        let mut x = vec![0.0f32; n * 2];
        for ri in 0..self.runs.len() {
            let (s0, s1) = (self.runs[ri].s0, self.runs[ri].s1);
            let (a, b) = (s0.max(c0), s1.min(c1));
            if a >= b {
                continue;
            }
            self.start_feed(ri);
            self.start_feed(ri + 1); // 次の区間（次の曲）も今のうちに計算を始める
            self.feeds[ri].as_mut().unwrap().fill(a, b, &mut x[(a - c0) * STEP * 2..(b - c0) * STEP * 2])?;
            if b == s1 {
                self.feeds[ri] = None; // 使い終わった
            }
        }
        // パートの音（Phase 5 B2）: pm で元の音と切り替え、パートごとの音量 pg を掛けて混ぜる
        let (plan, d) = (self.plan, self.d);
        if self.stem_runs.iter().any(|g| g.s0 < c1 && g.s1 > c0 && self.tracks[g.idx].parts_ok) {
            let w = per_sample(&plan.pm[d][c0..c1].iter().map(|&v| v as f64).collect::<Vec<_>>());
            let pg: Vec<Vec<f64>> = (0..4).map(|i| per_sample(&plan.pg[d][c0..c1].iter().map(|g| g[i]).collect::<Vec<_>>())).collect();
            for si in 0..self.stem_runs.len() {
                let g = &self.stem_runs[si];
                let (a, b) = (g.s0.max(c0), g.s1.min(c1));
                if a >= b || !self.tracks[g.idx].parts_ok {
                    continue;
                }
                if let Err(e) = self.ensure_stem(si) {
                    log::warn!("stems audio failed: {e:#}"); // パートの音が読めなければ元の音のまま
                    let g = &mut self.stem_runs[si];
                    g.s1 = g.s0;
                    continue;
                }
                let st = &mut self.stem.as_mut().unwrap().1;
                let m = (b - a) * STEP;
                let mut y = vec![0.0f32; m * 8];
                st.fill((a * STEP) as i64, (b * STEP) as i64, &mut y);
                for i in 0..m {
                    let gi = (a - c0) * STEP + i;
                    let ww = w[gi];
                    if ww == 0.0 {
                        continue;
                    }
                    let (mut l, mut r) = (0.0f64, 0.0f64);
                    for (p, gp) in pg.iter().enumerate() {
                        l += y[i * 8 + 2 * p] as f64 * gp[gi];
                        r += y[i * 8 + 2 * p + 1] as f64 * gp[gi];
                    }
                    x[2 * gi] = (x[2 * gi] as f64 * (1.0 - ww) + l * ww) as f32;
                    x[2 * gi + 1] = (x[2 * gi + 1] as f64 * (1.0 - ww) + r * ww) as f32;
                }
            }
        }
        if let Some((si, _)) = &self.stem {
            if self.stem_runs[*si].s1 <= c1 {
                self.stem = None; // 使い終わった
            }
        }
        Ok(x)
    }
}
