//! 高速で書き出す（djmix/export/render.py。design_phase3.md §6.2）。再生せずに、キュー（使う範囲・つなぎ・設定）からミックスを計算する。
//!
//! 再生（frontend/engine/runner.js・sync.js・mixer.js）と同じ規則を、512 サンプル（約 11.6ms）刻みの「操作の表」として計算し、
//! その表どおりに各曲をストレッチ（Signalsmith Stretch。再生と同じエンジンの C++ 版。stretch.rs）してミキサー（mixer.rs）に通す。
//!
//! 1. 拍の写像: 前の曲に位相を固定して入る（次の曲の入り口の小節頭 ＝ 重なりの始まりの小節頭。エコーアウト・カットは前の曲の終わり）、
//!    重なりの間は 次の曲の拍位置 = phi + k × 前の曲の拍位置（再生では 20ms ごとの追従で近づける値を、ここでは正確に計算する）、
//!    繋ぎの後は 8 小節かけて原速へ（テンポフェーダーの幅で頭打ち。runner と同じ）、次の重なりが近ければ打ち切る
//! 2. 自動の動き: transition.rs（transition.js の levels() と同じ値）
//! 3. ミキサー: 音量補正 → 3 バンドのアイソレーター（LR4 250Hz/2.5kHz＋オールパス補正）→ フィルター → クロスフェーダー（カーブ）
//!    → エコー（フェーダーの前から分岐）→ マスター → 先読みのリミッター
//!
//! 手で動かした操作は入らない。チャンネルフェーダーは全開、GAIN は 0dB として計算する。
//!
//! Python 版の既定（engine='browser'）は画面の WebAssembly でストレッチを計算してもらっていたが、Mac 版はここで
//! 同じ Signalsmith を続けて（区切らずに）動かすので、画面に頼む区間は無い（stage は prepare → mix → done）。
use super::mixer::Mixdown;
use super::transition::{self as tr, Join};
use super::writer::WavSink;
use crate::analysis::types::{Analysis, CutBar};
use crate::analysis::{cache, cutpoints, stems};
use crate::util::searchsorted_left;
use crate::{db, edits};
use anyhow::{anyhow, bail, Result};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

pub const SR: u32 = 44100;
/// 操作の表の刻み（サンプル）
pub const STEP: usize = 512;
pub const DT: f64 = STEP as f64 / SR as f64;
pub const RETURN_BARS: f64 = 8.0;
/// パートの音は、使う所の少し前（約 2 秒）から計算しておく
pub const PRIME_STEPS: usize = 176;

// ---------------------------------------------------------------- 拍の位置（frontend/engine/beatmap.js と同じ）
#[derive(Debug, Clone)]
pub struct BeatMap {
    pub b: Vec<f64>,
    pub meter: u32,
    pub bar_times: Vec<f64>,
    pub no_beat: Vec<(f64, f64)>,
    pub n: usize,
}

impl BeatMap {
    pub fn new(a: &Analysis) -> BeatMap {
        Self::from_parts(a.beats.clone(), a.meter, a.bar_times(), a.no_beat.clone())
    }

    /// 拍が 2 つ未満だと位置を決められないので、120 BPM の拍を足しておく（Python 版では落ちる）
    pub fn from_parts(mut b: Vec<f64>, meter: u32, bar_times: Vec<f64>, no_beat: Vec<(f64, f64)>) -> BeatMap {
        while b.len() < 2 {
            let last = b.last().copied().unwrap_or(0.0);
            b.push(if b.is_empty() { 0.0 } else { last + 0.5 });
        }
        let n = b.len();
        BeatMap { b, meter, bar_times, no_beat, n }
    }

    /// 拍子（0 なら 4）
    pub fn meter_or4(&self) -> f64 {
        if self.meter == 0 {
            4.0
        } else {
            self.meter as f64
        }
    }

    pub fn pos(&self, t: f64) -> f64 {
        let (b, n) = (&self.b, self.n);
        if t <= b[0] {
            return (t - b[0]) / (b[1] - b[0]);
        }
        if t >= b[n - 1] {
            return (n - 1) as f64 + (t - b[n - 1]) / (b[n - 1] - b[n - 2]);
        }
        let i = searchsorted_left(b, t) - 1;
        i as f64 + (t - b[i]) / (b[i + 1] - b[i])
    }

    pub fn time(&self, p: f64) -> f64 {
        let (b, n) = (&self.b, self.n);
        if p <= 0.0 {
            return b[0] + p * (b[1] - b[0]);
        }
        if p >= (n - 1) as f64 {
            return b[n - 1] + (p - (n - 1) as f64) * (b[n - 1] - b[n - 2]);
        }
        let i = p as usize;
        b[i] + (p - i as f64) * (b[i + 1] - b[i])
    }

    pub fn beat_len(&self, t: f64) -> f64 {
        let i = (searchsorted_left(&self.b, t) as i64 - 1).clamp(0, self.n as i64 - 2) as usize;
        self.b[i + 1] - self.b[i]
    }

    pub fn in_no_beat(&self, t: f64) -> bool {
        self.no_beat.iter().any(|&(s, e)| s <= t && t < e)
    }
}

// ---------------------------------------------------------------- 操作の表（runner.js の規則）
#[derive(Debug, Clone)]
pub struct Track {
    pub idx: usize,
    /// キューの項目（track_id・join_tpl・join_bars・join_params・use_start・use_end…）
    pub item: Map<String, Value>,
    pub track_id: i64,
    /// 曲のファイル
    pub path: PathBuf,
    pub bm: BeatMap,
    pub t0: f64,
    pub t1: f64,
    pub lufs: f64,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// 分離した音があり、パートごとに鳴らせる（Phase 5 B2）
    pub parts_ok: bool,
    /// キー合わせ（半音。この曲が次の曲として入る繋ぎで決まる。Phase 4）
    pub shift: f64,
    /// 小節ごとの音量 [{t, db}]（カットインの音量合わせ。analysis/cutpoints.rs）
    pub cut_bars: Vec<CutBar>,
}

impl Track {
    pub fn new(idx: usize, item: Map<String, Value>, path: PathBuf, bm: BeatMap, t0: f64, t1: f64, lufs: f64) -> Track {
        let track_id = item.get("track_id").and_then(|v| v.as_i64()).unwrap_or(0);
        Track {
            idx,
            item,
            track_id,
            path,
            bm,
            t0,
            t1,
            lufs,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            parts_ok: false,
            shift: 0.0,
            cut_bars: vec![],
        }
    }
}

/// 次の曲を前の曲の終わりで入れた所
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Cut {
    /// 刻み
    pub k: usize,
    /// 刻みの中の位置 0..1
    pub f: f64,
    /// 前の曲のデッキ
    pub out: usize,
}

/// 曲目（開始時刻つき）
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mark {
    pub t: f64,
    pub title: String,
    pub artist: String,
    pub album: String,
}

/// ループの折り返し（Python 版には無い記録。ストレッチに続けて入れる音を、折り返しの所でつなぐのに使う）。
/// 刻み k の位置は、刻み k-1 の位置から end（秒）まで進み、start へ戻ってから進んだ位置。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Wrap {
    pub deck: usize,
    pub k: usize,
    pub start: f64,
    pub end: f64,
}

/// 操作の表。デッキ 0/1 ごとに、刻みごとの 曲の番号（-1 = 止まっている）・入力位置（秒）・LOW・ハイパス・エコーの送り、と共通のクロスフェーダー。
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub n: usize,
    pub track: [Vec<i32>; 2],
    pub pos: [Vec<f64>; 2],
    pub low: [Vec<f64>; 2],
    pub hp: [Vec<f64>; 2],
    pub echo: [Vec<f64>; 2],
    pub echo_delay: [Vec<f64>; 2],
    /// パートの音で鳴らすか（0/1。Phase 5 B2）
    pub pm: [Vec<u8>; 2],
    /// パートごとの音量（刻み × 4）
    pub pg: [Vec<[f64; 4]>; 2],
    /// カットインの音量合わせ（倍率。design_cutin.md ③）
    pub jg: [Vec<f64>; 2],
    pub xf: Vec<f64>,
    pub cuts: Vec<Cut>,
    pub marks: Vec<Mark>,
    pub wraps: Vec<Wrap>,
}

/// _judge_join の結果（runner.plan() と同じ計画）。acc・ramp_from・start_k は進めながら書き換える
#[derive(Debug, Clone)]
pub struct JoinPlan {
    pub join: Join,
    pub parts: Option<String>,
    pub ov_start: f64,
    pub ov_end: f64,
    pub p_s: f64,
    pub p_e: f64,
    pub beats: f64,
    pub k: f64,
    pub phi: f64,
    pub synced: bool,
    pub late: bool,
    pub in_time: f64,
    pub reason: Option<String>,
    pub cut: bool,
    pub trim: f64,
    pub trim_sec: f64,
    pub loop_bars: i64,
    pub loop_start: Option<f64>,
    pub loop_end: Option<f64>,
    pub loop_beats: f64,
    pub ramp_to: Option<f64>,
    pub acc: f64,
    pub ramp_from: f64,
    pub start_k: Option<usize>,
}

impl JoinPlan {
    fn ramp(&self) -> Option<f64> {
        self.ramp_to.filter(|v| *v != 0.0)
    }
}

/// runner.plan() と同じ計画（次の曲の shift もここで決める）。
pub fn judge_join(out: &Track, inn: &mut Track, rate_out: f64, target: f64) -> JoinPlan {
    let mut j = tr::join_of(Some(&out.item));
    let (bm_o, bm_i) = (&out.bm, &inn.bm);
    let geo = |jj: &Join| {
        let ov = tr::overlap_bars(&bm_o.bar_times, out.t1, jj); // ループ（Phase 4）を含む
        let end = if ov.end_bar < bm_o.bar_times.len() { bm_o.bar_times[ov.end_bar] } else { out.t1 };
        (ov, ov.start_time, end)
    };
    let (mut ov, mut s, mut e) = geo(&j);
    let in_bar = tr::in_start_bar(&bm_i.bar_times, inn.t0);
    let in_time = if bm_i.bar_times.is_empty() { inn.t0 } else { bm_i.bar_times[in_bar] };
    let jd = tr::judge(bm_o, bm_i, s, e, in_time, rate_out);
    if jd.reason.is_some() && !tr::enters_at_end(&j.tpl) {
        j = Join { tpl: "echo".into(), bars: 2, params: j.params.clone() };
        (ov, s, e) = geo(&j);
    }
    let synced = jd.reason.is_none() && j.tpl != "cut";
    let late = !synced || tr::enters_at_end(&j.tpl);
    let cut = j.tpl == "cut";
    let mut trim = 0.0;
    if cut {
        // カットインの入りの音量合わせ（runner と同じ。design_cutin.md ③）
        let bar_o = bm_o.beat_len(e) * bm_o.meter_or4();
        let bar_i = bm_i.beat_len(in_time) * bm_i.meter_or4();
        let auto = cutpoints::auto_trim_db(&out.cut_bars, e, bar_o, out.lufs, &inn.cut_bars, in_time, bar_i, inn.lufs, target);
        trim = cutpoints::trim_db(&j.params, auto);
    }
    let (p_s, p_e) = (bm_o.pos(s), bm_o.pos(e));
    let phi = bm_i.pos(in_time) - jd.k * (if late { p_e } else { p_s });
    let loop_bars = if late { 0 } else { ov.loop_bars };
    let loop_start = if loop_bars != 0 { Some(bm_o.bar_times[ov.loop_start_bar]) } else { None };
    let loop_beats = match loop_start {
        Some(ls) => p_e - bm_o.pos(ls),
        None => 0.0,
    };
    let beats = (p_e - p_s + loop_beats).max(1.0);
    let ramp_to = if !late && j.str_param("tempo") == Some("ramp") {
        Some(tr::ramp_target(bm_o, bm_i, e, bm_i.time(bm_i.pos(in_time) + jd.k * beats), jd.k))
    } else {
        None
    };
    inn.shift = tr::value_f64(j.param("shift")).unwrap_or(0.0);
    let parts = if !late && tr::uses_parts(Some(&j)) && out.parts_ok && inn.parts_ok {
        j.str_param("parts").map(|s| s.to_string())
    } else {
        None
    };
    let trim_sec = if trim != 0.0 { cutpoints::TRIM_BACK_BARS * bm_i.beat_len(in_time) * bm_i.meter_or4() } else { 0.0 };
    JoinPlan {
        join: j,
        parts,
        ov_start: s,
        ov_end: e,
        p_s,
        p_e,
        beats,
        k: jd.k,
        phi,
        synced,
        late,
        in_time: if synced || cut { in_time } else { inn.t0 },
        reason: jd.reason,
        cut,
        trim,
        trim_sec,
        loop_bars,
        loop_start,
        loop_end: if loop_bars != 0 { Some(e) } else { None },
        loop_beats,
        ramp_to,
        acc: 0.0,
        ramp_from: 1.0,
        start_k: None,
    }
}

#[derive(Debug, Clone)]
struct DeckState {
    t: Option<usize>,
    pos: f64,
    rate: f64,
    low: f64,
    hp: f64,
    echo: f64,
    delay: f64,
    pm: u8,
    pg: [f64; 4],
    jg: Option<(f64, f64, f64)>,
    playing: bool,
    just: bool,
}

impl DeckState {
    fn new() -> DeckState {
        DeckState {
            t: None,
            pos: 0.0,
            rate: 1.0,
            low: 1.0,
            hp: 0.0,
            echo: 0.0,
            delay: 0.3,
            pm: 0,
            pg: [1.0; 4],
            jg: None,
            playing: false,
            just: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Solo,
    Pre,
    Overlap,
    Post,
    Last,
}

#[derive(Debug, Clone, Copy)]
struct Ramp {
    from: f64,
    start: f64,
    beats: f64,
}

fn mark_of(t: f64, tk: &Track) -> Mark {
    Mark { t, title: tk.title.clone(), artist: tk.artist.clone(), album: tk.album.clone() }
}

fn two_mut<T>(v: &mut [T], a: usize, b: usize) -> (&mut T, &mut T) {
    assert!(a != b);
    if a < b {
        let (x, y) = v.split_at_mut(b);
        (&mut x[a], &mut y[0])
    } else {
        let (x, y) = v.split_at_mut(a);
        (&mut y[0], &mut x[b])
    }
}

/// 操作の表を作る（render.py の build_plan をそのまま移したもの。tracks の shift もここで決まる）。
pub fn build_plan(tracks: &mut [Track], settings: &Map<String, Value>) -> Plan {
    let tempo_range = tr::value_f64(settings.get("tempo_range")).unwrap_or(0.08);
    let tempo_return = settings.get("tempo_return") != Some(&Value::Bool(false));
    let target = tr::value_f64(settings.get("lufs_target")).unwrap_or(-14.0);
    let mut out = Plan::default();
    if tracks.is_empty() {
        return out;
    }

    // デッキの状態
    let mut deck = [DeckState::new(), DeckState::new()];
    let mut xf = 0.0; // 0 = デッキ0 側
    let mut cur_d = 0usize;
    deck[0].t = Some(0);
    deck[0].pos = tracks[0].t0;
    out.marks.push(mark_of(0.0, &tracks[0]));
    let mut n_step = 0usize;
    let mut ramp: Option<Ramp> = None; // 原速へ戻す
    let mut plan: Option<JoinPlan> = None; // いまの繋ぎの計画
    let mut phase = Phase::Solo; // solo（次の繋ぎ待ち）/ pre / overlap / post
    let mut i = 0usize; // 主になっている曲の番号
    let ntr = tracks.len();

    deck[0].playing = true;
    deck[1].playing = false;

    // runner の arm と同じ: 前の曲だけが聞こえる状態にして、次の繋ぎを待機させる
    let arm = |tracks: &mut [Track],
               deck: &mut [DeckState; 2],
               i: usize,
               cur_d: usize,
               xf: &mut f64,
               plan: &mut Option<JoinPlan>,
               phase: &mut Phase,
               ramp: &mut Option<Ramp>| {
        *xf = if cur_d == 0 { 0.0 } else { 1.0 };
        let rate_out = deck[cur_d].rate;
        let (out_t, inn_t) = two_mut(tracks, i, i + 1);
        let pl = judge_join(out_t, inn_t, rate_out, target);
        let other = 1 - cur_d;
        let o = &mut deck[other];
        o.t = Some(i + 1);
        o.pos = pl.in_time;
        o.rate = 1.0;
        o.low = 1.0;
        o.hp = 0.0;
        o.echo = 0.0;
        o.playing = false;
        o.pm = 0;
        o.pg = [1.0; 4];
        o.jg = None;
        let c = &mut deck[cur_d];
        c.low = 1.0;
        c.hp = 0.0;
        c.echo = 0.0;
        *plan = Some(pl);
        *ramp = None; // 次の繋ぎを待機させたら、原速へ戻すのは打ち切る（runner と同じ）
        *phase = Phase::Pre;
    };

    if ntr > 1 {
        arm(tracks, &mut deck, i, cur_d, &mut xf, &mut plan, &mut phase, &mut ramp);
    }
    loop {
        let other = 1 - cur_d;
        // ---- 主の曲の進み ----
        if let Some(r) = ramp {
            let o = &mut deck[cur_d];
            let p = ((tracks[i].bm.pos(o.pos) - r.start) / r.beats).clamp(0.0, 1.0);
            let rr = r.from + (1.0 - r.from) * p;
            let tf = ((rr - 1.0) / tempo_range).clamp(-1.0, 1.0);
            o.rate = 1.0 + tf * tempo_range;
            if p >= 1.0 {
                ramp = None;
            }
        }
        if phase == Phase::Pre || phase == Phase::Overlap {
            let pl = plan.as_mut().unwrap();
            let [d0, d1] = &mut deck;
            let (o, inn) = if cur_d == 0 { (d0, d1) } else { (d1, d0) };
            // 前の曲の終わりで次の曲を入れる繋ぎ（カットイン・エコーアウト）: 前の曲が小節の頭に来るのは、この刻みの f の位置。
            // 次の曲の入り口がちょうどそこで鳴るよう、刻みの頭の位置を f だけ手前にする（刻みの f より前の音は Mixdown で消す）
            if pl.late && !inn.playing && o.pos >= pl.ov_end - DT * o.rate {
                let f = ((pl.ov_end - o.pos) / (o.rate.max(1e-6) * DT)).clamp(0.0, 1.0);
                inn.playing = true;
                inn.just = true;
                pl.start_k = Some(n_step);
                if !pl.synced {
                    inn.pos = pl.in_time - f * DT;
                }
                if pl.trim != 0.0 {
                    inn.jg = Some((10f64.powf(pl.trim / 20.0), (n_step as f64 + f) * DT, pl.trim_sec));
                }
                out.cuts.push(Cut { k: n_step, f, out: cur_d });
                out.marks.push(mark_of((n_step as f64 + f) * DT, &tracks[i + 1]));
            }
            if phase == Phase::Pre && o.pos >= pl.ov_start {
                phase = Phase::Overlap;
                pl.acc = (tracks[i].bm.pos(o.pos) - pl.p_s).max(0.0); // 拍の進みを足し合わせる（ループで巻き戻るため）
                pl.ramp_from = o.rate;
                if !pl.late {
                    inn.playing = true;
                    out.marks.push(mark_of(n_step as f64 * DT, &tracks[i + 1]));
                }
            }
            if phase == Phase::Overlap {
                let bm_o = &tracks[i].bm;
                let u = (pl.acc / pl.beats).clamp(0.0, 1.0);
                if let Some(rt) = pl.ramp() {
                    // テンポランプ: 前の曲の速度を直線で変える（runner と同じ）
                    o.rate = pl.ramp_from + (rt - pl.ramp_from) * u;
                }
                let lv = tr::levels(&pl.join, u, pl.beats);
                xf = if cur_d == 0 { lv.xf } else { 1.0 - lv.xf };
                o.low = lv.low_out;
                o.hp = lv.hp_out;
                o.echo = lv.echo_out;
                inn.low = lv.low_in;
                o.delay = bm_o.beat_len(o.pos) / o.rate.max(1e-6) * 0.75;
                if pl.parts.is_some() {
                    // パートごとの音量（runner と同じ。前の曲は重なりの頭でパートの音へ）
                    if let Some(pg) = tr::part_gains(&pl.join, u, pl.beats) {
                        o.pm = 1;
                        o.pg = pg.out;
                        inn.pm = 1;
                        inn.pg = pg.inn;
                    }
                }
                let ended = if pl.late {
                    // 次の曲を入れた刻みの次で（入れた刻みの f までは前の曲が鳴る）
                    pl.start_k.map(|k| n_step > k).unwrap_or(false)
                } else if pl.loop_bars != 0 || pl.ramp().is_some() {
                    pl.acc >= pl.beats - 0.02
                } else {
                    o.pos >= pl.ov_end - 0.01
                };
                if ended {
                    // 繋ぎ終わり: 前の曲を止め、次の曲を主にする
                    let lv = tr::levels(&pl.join, 1.0, pl.beats);
                    xf = if cur_d == 0 { lv.xf } else { 1.0 - lv.xf };
                    o.playing = false;
                    o.echo = 0.0;
                    o.low = 1.0;
                    o.hp = 0.0;
                    o.pm = 0;
                    o.pg = [1.0; 4];
                    inn.pm = 0;
                    inn.pg = [1.0; 4]; // 次の曲は全パート 1 になっているので元の音へ戻す
                    // この刻みの次の曲の位置も進めておく（主が入れ替わると「従」の位置の計算を通らず、1 刻み止まっていた）
                    if pl.synced {
                        inn.pos = tracks[i + 1].bm.time(pl.phi + pl.k * (pl.p_s + pl.acc));
                    } else if !std::mem::take(&mut inn.just) {
                        inn.pos += DT;
                    }
                    inn.low = if lv.low_in != 0.0 { lv.low_in } else { 1.0 };
                    inn.playing = true;
                    // 次の曲の今の速度（位相を固定した写像の傾き）
                    let r = if pl.synced {
                        // 拍を合わせていた速度（直前 4 拍の平均）をテンポフェーダーの幅で頭打ちにする（runner と同じ）
                        let r = tr::synced_rate(&tracks[i + 1].bm, &tracks[i].bm, inn.pos, o.pos, pl.k, o.rate);
                        let r = 1.0 + ((r - 1.0) / tempo_range).clamp(-1.0, 1.0) * tempo_range;
                        if pl.ramp().is_some() {
                            1.0 // テンポランプなら次の曲はもう原速
                        } else {
                            r
                        }
                    } else {
                        1.0
                    };
                    inn.rate = r;
                    let inn_pos = inn.pos;
                    cur_d = other;
                    i += 1;
                    phase = Phase::Post;
                    if tempo_return && (r - 1.0).abs() > 0.002 {
                        let bm = &tracks[i].bm;
                        ramp = Some(Ramp { from: r, start: bm.pos(inn_pos), beats: RETURN_BARS * bm.meter_or4() });
                    }
                }
            }
        }
        // ---- 次の曲（従）の位置 ----
        if (phase == Phase::Pre || phase == Phase::Overlap) && deck[1 - cur_d].playing {
            let pl = plan.as_ref().unwrap();
            let main_pos = deck[cur_d].pos;
            let s = &mut deck[1 - cur_d];
            if pl.synced {
                // 前の曲の拍位置は足し合わせた値で（ループで巻き戻っても次の曲は進み続ける）
                let p_out = if phase == Phase::Overlap { pl.p_s + pl.acc } else { tracks[i].bm.pos(main_pos) };
                s.pos = tracks[i + 1].bm.time(pl.phi + pl.k * p_out);
            } else if !std::mem::take(&mut s.just) {
                s.pos += DT;
            }
        }
        // ---- 刻みを書き出す ----
        for (d, st) in deck.iter().enumerate() {
            out.track[d].push(match st.t {
                Some(t) if st.playing => tracks[t].idx as i32,
                _ => -1,
            });
            out.pos[d].push(st.pos);
            out.low[d].push(st.low);
            out.hp[d].push(st.hp);
            out.echo[d].push(st.echo);
            out.echo_delay[d].push(st.delay);
            out.pm[d].push(st.pm);
            out.pg[d].push(st.pg);
            let mut jg = 1.0;
            if let Some((g0, ta, sec)) = st.jg {
                // 入りの音量合わせ: 切り替えの時刻に g、そこから直線で 1 へ（mixer.js の setJoinGainAt と同じ）
                jg = g0 + (1.0 - g0) * ((n_step as f64 * DT - ta) / sec).clamp(0.0, 1.0);
            }
            out.jg[d].push(jg);
        }
        out.xf.push(xf);
        n_step += 1;
        // ---- 時間を進める ----
        let p0 = if phase == Phase::Overlap { Some(tracks[i].bm.pos(deck[cur_d].pos)) } else { None };
        {
            let m = &mut deck[cur_d];
            m.pos += m.rate * DT;
        }
        if phase == Phase::Overlap {
            let pl = plan.as_mut().unwrap();
            let m = &mut deck[cur_d];
            let mut wrapped = false;
            if let (true, Some(ls), Some(le)) = (pl.loop_bars != 0, pl.loop_start, pl.loop_end) {
                if ls <= m.pos - m.rate * DT && m.pos >= le {
                    m.pos -= le - ls; // ループ: 最後の小節を繰り返す
                    wrapped = true;
                    out.wraps.push(Wrap { deck: cur_d, k: n_step, start: ls, end: le });
                }
            }
            let d = tracks[i].bm.pos(m.pos) - p0.unwrap();
            pl.acc += (d + if wrapped { pl.loop_beats } else { 0.0 }).max(0.0);
        }
        if phase == Phase::Post {
            // 次の繋ぎを待機させる: 原速へ戻し終えたら、または次の重なりが近いとき（runner と同じ）
            if i + 1 < ntr {
                let t = &tracks[i];
                let m = &deck[cur_d];
                let bar_sec = t.bm.beat_len(m.pos) * t.bm.meter_or4() / m.rate.max(1e-6);
                let soon = t.t1 - m.pos < (tr::join_of(Some(&t.item)).bars as f64 + 4.0) * bar_sec;
                if ramp.is_none() || soon {
                    arm(tracks, &mut deck, i, cur_d, &mut xf, &mut plan, &mut phase, &mut ramp);
                }
            } else {
                phase = Phase::Last;
            }
        }
        let mpos = deck[cur_d].pos;
        if (phase == Phase::Last || phase == Phase::Solo || ntr == 1) && mpos >= tracks[i].t1 - 0.01 {
            break;
        }
        if phase == Phase::Pre && mpos >= tracks[i].t1 + 5.0 {
            break; // 念のため（計画が壊れていても止まる）
        }
        if n_step as f64 > 6.0 * 3600.0 / DT {
            break;
        }
    }
    out.n = n_step;
    out
}

/// パートごとに鳴らす繋ぎに使う曲（tracks の番号）。分離した音を用意するのに使う。
pub fn parts_tracks(tracks: &[Track]) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    for w in tracks.windows(2) {
        if tr::uses_parts(Some(&tr::join_of(Some(&w[0].item)))) {
            out.insert(w[0].idx);
            out.insert(w[1].idx);
        }
    }
    out
}

/// デッキごとに、同じ曲が続く区間（main、2ch）と、パートの音を使う区間（stems、8ch）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Segment {
    pub id: usize,
    pub deck: usize,
    pub idx: usize,
    pub track_id: i64,
    pub shift: f64,
    pub kind: &'static str,
    pub channels: usize,
    pub s0: usize,
    pub s1: usize,
}

/// render.py の segments_of と同じ区間の一覧（ストレッチの単位にも使う）。
pub fn segments_of(plan: &Plan, tracks: &[Track]) -> Vec<Segment> {
    let mut segs = vec![];
    for d in 0..2 {
        let trk = &plan.track[d];
        let mut s = 0usize;
        while s < plan.n {
            if trk[s] < 0 {
                s += 1;
                continue;
            }
            let idx = trk[s];
            let mut e = s;
            while e < plan.n && trk[e] == idx {
                e += 1;
            }
            let t = &tracks[idx as usize];
            let base = Segment {
                id: 0,
                deck: d,
                idx: idx as usize,
                track_id: t.track_id,
                shift: t.shift,
                kind: "main",
                channels: 2,
                s0: s,
                s1: e,
            };
            segs.push(base.clone());
            let on: Vec<usize> = (s..e).filter(|&k| plan.pm[d][k] > 0).collect();
            if !on.is_empty() && t.parts_ok {
                let mut groups = vec![];
                let mut g0 = on[0];
                for w in on.windows(2) {
                    if w[1] - w[0] > 2 * PRIME_STEPS {
                        groups.push((g0, w[0]));
                        g0 = w[1];
                    }
                }
                groups.push((g0, *on.last().unwrap()));
                for (a, b) in groups {
                    segs.push(Segment {
                        kind: "stems",
                        channels: 8,
                        s0: s.max(a.saturating_sub(PRIME_STEPS)),
                        s1: e.min(b + 3),
                        ..base.clone()
                    });
                }
            }
            s = e;
        }
    }
    for (k, g) in segs.iter_mut().enumerate() {
        g.id = k;
    }
    segs
}

// ---------------------------------------------------------------- 書き出しの実行
#[derive(Debug, Clone)]
struct JobState {
    stage: &'static str,
    progress: f64,
    message: Option<String>,
    error: Option<String>,
    result: Option<Value>,
}

pub struct RenderJob {
    pub items: Vec<Map<String, Value>>,
    pub settings: Map<String, Value>,
    pub out_path: PathBuf,
    pub fmt: String,
    pub want_tracklist: bool,
    state: Mutex<JobState>,
    cancel_flag: AtomicBool,
    running: AtomicBool,
    t_start: Instant,
}

const CANCELLED: &str = "中止しました";

impl RenderJob {
    pub fn new(
        items: Vec<Map<String, Value>>,
        settings: Map<String, Value>,
        out_path: PathBuf,
        fmt: &str,
        tracklist: bool,
    ) -> Arc<RenderJob> {
        Arc::new(RenderJob {
            items,
            settings,
            out_path,
            fmt: fmt.to_string(),
            want_tracklist: tracklist,
            state: Mutex::new(JobState {
                stage: "prepare",
                progress: 0.0,
                message: Some("準備しています…".into()),
                error: None,
                result: None,
            }),
            cancel_flag: AtomicBool::new(false),
            running: AtomicBool::new(false),
            t_start: Instant::now(),
        })
    }

    /// 進み具合（Python 版と同じキー）
    pub fn status(&self) -> Value {
        let s = self.state.lock().clone();
        let el = self.t_start.elapsed().as_secs_f64();
        let eta = if 0.02 < s.progress && s.progress < 1.0 { Some(el / s.progress * (1.0 - s.progress)) } else { None };
        json!({
            "progress": crate::util::round(s.progress, 4),
            "message": s.message,
            "error": s.error,
            "done": s.result.is_some(),
            "result": s.result,
            "eta_sec": eta.filter(|e| *e != 0.0).map(|e| tr::py_round(e) as i64),
            "stage": s.stage,
            "segments": 0,
            "segments_done": 0,
        })
    }

    pub fn stage(&self) -> &'static str {
        self.state.lock().stage
    }
    pub fn error(&self) -> Option<String> {
        self.state.lock().error.clone()
    }
    pub fn result(&self) -> Option<Value> {
        self.state.lock().result.clone()
    }
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel_flag.load(Ordering::SeqCst)
    }

    /// 画面に計算を頼む区間（Mac 版はここで計算するので常に空。API の互換のため）
    pub fn segment_info(&self) -> Vec<Value> {
        vec![]
    }

    fn set(&self, f: impl FnOnce(&mut JobState)) {
        f(&mut self.state.lock());
    }

    /// キューの項目から曲を読む（DB・解析キャッシュ・ユーザー修正・カットインの音量）
    pub fn load_tracks(&self) -> Result<Vec<Track>> {
        let mut out = vec![];
        for (k, it) in self.items.iter().enumerate() {
            let tid = it.get("track_id").and_then(|v| v.as_i64().or_else(|| tr::value_f64(Some(v)).map(|x| x as i64)));
            let not_analyzed = || anyhow!("キューの {} 曲目が解析されていません", k + 1);
            let tid = tid.ok_or_else(not_analyzed)?;
            let row = db::with(|c| {
                use rusqlite::OptionalExtension;
                Ok(c.query_row("SELECT path, title, artist, album FROM tracks WHERE id = ?", [tid], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                })
                .optional()?)
            })?;
            let Some((path, title, artist, album)) = row else { return Err(not_analyzed()) };
            let Some(a) = cache::read(tid) else { return Err(not_analyzed()) };
            let a = edits::apply(tid, &a);
            if a.beats.len() < 2 {
                bail!("キューの {} 曲目の拍が取れていません", k + 1);
            }
            let t0 = tr::value_f64(it.get("use_start")).unwrap_or(0.0);
            let t1 = tr::value_f64(it.get("use_end")).unwrap_or(a.duration);
            let cut_bars = cutpoints::for_track(tid).map(|c| c.bars.clone()).unwrap_or_default();
            let mut item = it.clone();
            item.insert("path".into(), json!(path));
            let mut t = Track::new(k, item, PathBuf::from(&path), BeatMap::new(&a), t0, t1, a.lufs);
            t.track_id = tid;
            t.title = title.unwrap_or_default();
            t.artist = artist.unwrap_or_default();
            t.album = album.unwrap_or_default();
            t.cut_bars = cut_bars;
            out.push(t);
        }
        Ok(out)
    }

    /// 書き出しを実行する（このスレッドで）。tracks を渡せば DB から読まずにそれを使う（試験用）。
    pub fn run(&self, tracks: Option<Vec<Track>>) {
        self.running.store(true, Ordering::SeqCst);
        let r = self.run_inner(tracks);
        if let Err(e) = r {
            let msg: String = e.to_string().chars().take(300).collect();
            self.set(|s| {
                s.error = Some(if msg.is_empty() { "error".into() } else { msg });
                s.stage = "error";
            });
            log::warn!("export failed: {e:#}");
        }
        self.running.store(false, Ordering::SeqCst);
    }

    fn run_inner(&self, tracks: Option<Vec<Track>>) -> Result<()> {
        let mut tracks = match tracks {
            Some(t) => t,
            None => self.load_tracks()?,
        };
        if tracks.is_empty() {
            bail!("ミックスキューが空です");
        }
        // パートごとに鳴らす繋ぎ（Phase 5 B2）: 分離した音が無い曲は、ここで分離して保存する（1 曲 20 秒ほど）
        let need: Vec<usize> = parts_tracks(&tracks).into_iter().collect();
        for (n, &k) in need.iter().enumerate() {
            let tid = tracks[k].track_id;
            if !stems::audio_ready(tid) {
                self.set(|s| s.message = Some(format!("パートの音を用意しています（{} / {}）…", n + 1, need.len())));
                if let Err(e) = stems::compute_audio_for_track(tid, &tracks[k].path.to_string_lossy()) {
                    log::warn!("stems failed {tid}: {e:#}"); // 分離できなければ、その繋ぎはパートを分けない
                }
                if self.is_cancelled() {
                    bail!(CANCELLED);
                }
            }
            tracks[k].parts_ok = stems::audio_ready(tid);
        }
        self.set(|s| s.message = Some("繋ぎを計算しています…".into()));
        let plan = build_plan(&mut tracks, &self.settings);
        self.set(|s| s.stage = "mix");
        let ceil_db = if self.fmt == "wav" { -0.3 } else { -1.0 };
        let mut sink = WavSink::new(&self.out_path, &self.fmt, SR)?;
        let res = (|| -> Result<()> {
            let mut mixer = Mixdown::new(&tracks, &plan, &self.settings, false, ceil_db);
            let mut write_err: Option<std::io::Error> = None;
            mixer.run(
                &mut |pcm: &[u8]| {
                    if write_err.is_none() {
                        if let Err(e) = sink.write(pcm) {
                            write_err = Some(e);
                        }
                    }
                },
                &mut |done: f64| {
                    self.set(|s| {
                        s.progress = done * 0.97;
                        s.message = None;
                    });
                    !self.is_cancelled()
                },
            )?;
            if let Some(e) = write_err {
                return Err(e.into());
            }
            Ok(())
        })();
        if let Err(e) = res {
            sink.abort();
            return Err(e);
        }
        self.set(|s| s.message = Some("ファイルにしています…".into()));
        let marks: Vec<Value> = plan.marks.iter().map(|m| serde_json::to_value(m).unwrap()).collect();
        let result = sink.close(if self.want_tracklist { Some(&marks) } else { None })?;
        self.set(|s| {
            s.result = Some(result);
            s.progress = 1.0;
            s.message = Some("書き出しました".into());
            s.stage = "done";
        });
        Ok(())
    }
}

/// 中止したときのエラーの文言
pub fn cancelled_error() -> anyhow::Error {
    anyhow!(CANCELLED)
}

static JOB: Lazy<Mutex<Option<Arc<RenderJob>>>> = Lazy::new(|| Mutex::new(None));

/// 書き出しを始める（別スレッド）。書き出し中ならエラー（サーバーは 409 にする）。
pub fn start(
    items: Vec<Map<String, Value>>,
    settings: Map<String, Value>,
    out_path: PathBuf,
    fmt: &str,
    tracklist: bool,
) -> Result<Arc<RenderJob>> {
    let mut g = JOB.lock();
    if let Some(j) = g.as_ref() {
        if j.is_running() {
            bail!("書き出し中です。終わってから始めてください");
        }
    }
    let job = RenderJob::new(items, settings, out_path, fmt, tracklist);
    job.running.store(true, Ordering::SeqCst); // スレッドが動き出す前に次の start が来ても弾く
    *g = Some(job.clone());
    let j2 = job.clone();
    std::thread::Builder::new().name("render".into()).spawn(move || j2.run(None))?;
    Ok(job)
}

/// いまの（最後の）書き出し
pub fn current() -> Option<Arc<RenderJob>> {
    JOB.lock().clone()
}

/// いまの書き出しを中止する
pub fn cancel() {
    if let Some(j) = current() {
        j.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_refuses_while_running() {
        let busy = RenderJob::new(vec![], Map::new(), PathBuf::from("/nonexistent/x.wav"), "wav", false);
        busy.running.store(true, Ordering::SeqCst);
        *JOB.lock() = Some(busy.clone());
        let e = start(vec![], Map::new(), PathBuf::from("/nonexistent/y.wav"), "wav", false).err().unwrap();
        assert_eq!(e.to_string(), "書き出し中です。終わってから始めてください");
        assert!(Arc::ptr_eq(&current().unwrap(), &busy));
        cancel();
        assert!(busy.is_cancelled());
        busy.running.store(false, Ordering::SeqCst);
        *JOB.lock() = None;
    }
}
