//! ミキサー（render.py のミキサーの部分。frontend/engine/mixer.js と同じ構成）。
//!
//! 音量補正 → 3 バンドのアイソレーター（LR4 250Hz/2.5kHz＋オールパス補正）→ フィルター → クロスフェーダー（カーブ）
//! → エコー（フェーダーの前から分岐）→ マスター → コンプレッサー（再生の DynamicsCompressor の代わり）→ 先読みのリミッター。
//! フィルターの計算は scipy の sosfilt と同じ（転置直接形 II、f64）。音の配列はインターリーブのステレオ（L R L R …）。
use super::render::{Plan, Track, SR, STEP};
use super::stretch::{DeckAudio, StemReader};
use super::transition::value_f64;
use anyhow::Result;
use serde_json::{Map, Value};
use std::f64::consts::PI;

pub const XO_LOW: f64 = 250.0;
pub const XO_HIGH: f64 = 2500.0;
/// Butterworth の Q
pub const Q_B: f64 = std::f64::consts::FRAC_1_SQRT_2;
/// フィルターの Q（Web Audio は dB 指定で 0.9）
pub fn q_f() -> f64 {
    10f64.powf(0.9 / 20.0)
}
/// カットインの切り替えの長さ（サンプル。約 2ms。再生のクロスフェーダーの予約と同じくらい）
pub const CUT_FADE: f64 = 88.0;
/// 1 回に計算する刻み数（約 2 秒）
pub const CH: usize = 176;

/// 1 段の係数 [b0, b1, b2, a0(=1), a1, a2]（scipy の sos 形式）
pub type Sos = [f64; 6];

/// Web Audio の BiquadFilter と同じ式（RBJ）の 1 段。kind: lowpass / highpass / allpass
pub fn rbj(kind: &str, f: f64, q: f64) -> Sos {
    let w0 = 2.0 * PI * f.min(SR as f64 / 2.0 * 0.999) / SR as f64;
    let (c, al) = (w0.cos(), w0.sin() / (2.0 * q));
    let b = match kind {
        "lowpass" => [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0],
        "highpass" => [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0],
        _ => [1.0 - al, -2.0 * c, 1.0 + al], // allpass
    };
    let a = [1.0 + al, -2.0 * c, 1.0 - al];
    [b[0] / a[0], b[1] / a[0], b[2] / a[0], 1.0, a[1] / a[0], a[2] / a[0]]
}

/// 3 バンド（low / mid / high）の段
pub fn bands() -> [Vec<Sos>; 3] {
    [
        vec![rbj("lowpass", XO_LOW, Q_B), rbj("lowpass", XO_LOW, Q_B), rbj("allpass", XO_HIGH, Q_B)],
        vec![rbj("highpass", XO_LOW, Q_B), rbj("highpass", XO_LOW, Q_B), rbj("lowpass", XO_HIGH, Q_B), rbj("lowpass", XO_HIGH, Q_B)],
        vec![rbj("highpass", XO_LOW, Q_B), rbj("highpass", XO_LOW, Q_B), rbj("highpass", XO_HIGH, Q_B), rbj("highpass", XO_HIGH, Q_B)],
    ]
}

/// フィルターのつまみの素通しの位置（ローパス 22kHz → ハイパス 10Hz）
pub fn static_filter() -> Vec<Sos> {
    vec![rbj("lowpass", 22000.0, q_f()), rbj("highpass", 10.0, q_f())]
}

/// 自動の動きのハイパス量（0..1）→ フィルターつまみ → 周波数（mixer.js の setFilter と同じ）
pub fn hp_freq(hp_amount: f64) -> f64 {
    let (v, dz) = (0.5 + 0.5 * hp_amount, 0.03);
    if v > 0.5 + dz {
        10.0 * (10000.0f64 / 10.0).powf((v - 0.5 - dz) / (0.5 - dz))
    } else {
        10.0
    }
}

/// クロスフェーダーの位置 → (デッキ0 の倍率, デッキ1 の倍率)
pub fn xfade_gains(x: &[f64], curve: &str) -> (Vec<f64>, Vec<f64>) {
    match curve {
        "linear" => (x.iter().map(|v| 1.0 - v).collect(), x.to_vec()),
        "cut" => (x.iter().map(|v| ((1.0 - v) / 0.06).min(1.0)).collect(), x.iter().map(|v| (v / 0.06).min(1.0)).collect()),
        _ => (x.iter().map(|v| (v * PI / 2.0).cos()).collect(), x.iter().map(|v| (v * PI / 2.0).sin()).collect()),
    }
}

/// 刻みごとの値を、刻みの間は直線でつないでサンプルごとに（ジッパーノイズを防ぐ。最後の刻みはその値のまま）
pub fn per_sample(v: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(v.len() * STEP);
    for s in 0..v.len() {
        let nxt = if s + 1 < v.len() { v[s + 1] } else { v[s] };
        let d = nxt - v[s];
        for j in 0..STEP {
            out.push(v[s] + d * (j as f64 / STEP as f64));
        }
    }
    out
}

/// sos の段を続けた IIR（ステレオ。状態 z[段][ch][2]）
#[derive(Debug, Clone)]
pub struct SosFilter {
    pub sos: Vec<Sos>,
    pub z: Vec<[[f64; 2]; 2]>,
}

impl SosFilter {
    pub fn new(sos: Vec<Sos>) -> SosFilter {
        let n = sos.len();
        SosFilter { sos, z: vec![[[0.0; 2]; 2]; n] }
    }

    /// インターリーブのステレオをその場で
    pub fn process(&mut self, x: &mut [f64]) {
        for (s, sec) in self.sos.iter().enumerate() {
            let [b0, b1, b2, _, a1, a2] = *sec;
            // 左右を同じループで（計算の待ちが重ならないように）
            let [[mut l0, mut l1], [mut r0, mut r1]] = self.z[s];
            for f in x.chunks_exact_mut(2) {
                let (xl, xr) = (f[0], f[1]);
                let yl = b0 * xl + l0;
                let yr = b0 * xr + r0;
                l0 = b1 * xl - a1 * yl + l1;
                r0 = b1 * xr - a1 * yr + r1;
                l1 = b2 * xl - a2 * yl;
                r1 = b2 * xr - a2 * yr;
                f[0] = yl;
                f[1] = yr;
            }
            self.z[s] = [[l0, l1], [r0, r1]];
        }
    }
}

/// 1 チャンネル（トリム → アイソレーター → フィルター）
#[derive(Debug, Clone)]
pub struct Channel {
    bands: [SosFilter; 3],
    stat: SosFilter,
    hp_z: [[f64; 2]; 2],
}

impl Default for Channel {
    fn default() -> Self {
        Self::new()
    }
}

impl Channel {
    pub fn new() -> Channel {
        let [l, m, h] = bands();
        Channel { bands: [SosFilter::new(l), SosFilter::new(m), SosFilter::new(h)], stat: SosFilter::new(static_filter()), hp_z: [[0.0; 2]; 2] }
    }

    /// x: インターリーブのステレオ（刻み × STEP）。low・hp は刻みごとの値（LOW の量 0..1、ハイパスの量 0..1）。
    pub fn process(&mut self, x: &[f64], trim: f64, low: &[f64], hp: &[f64]) -> Vec<f64> {
        let xt: Vec<f64> = x.iter().map(|v| v * trim).collect();
        let mut out = vec![0.0; x.len()];
        for (k, band) in self.bands.iter_mut().enumerate() {
            let mut y = xt.clone();
            band.process(&mut y);
            if k == 0 {
                let g = per_sample(low);
                for (i, v) in y.iter_mut().enumerate() {
                    let gg = g[i / 2];
                    *v *= gg * gg; // lowKnob(level)=0.5*level → eqGain = level²
                }
            }
            for (o, v) in out.iter_mut().zip(&y) {
                *o += v;
            }
        }
        self.stat.process(&mut out);
        if hp.iter().any(|&v| v > 0.0) {
            // フィルターの繋ぎ: 刻みごとに周波数を変える
            for (s, &h) in hp.iter().enumerate() {
                let mut f = SosFilter { sos: vec![rbj("highpass", hp_freq(h), q_f())], z: vec![self.hp_z] };
                f.process(&mut out[s * STEP * 2..(s + 1) * STEP * 2]);
                self.hp_z = f.z[0];
            }
        }
        out
    }
}

/// フェーダーの前から分岐する遅延＋フィードバック（0.55）＋ 6kHz のローパス。遅延は 1 刻みより長い前提でまとめて処理する。
#[derive(Debug, Clone)]
pub struct Echo {
    buf: Vec<f64>,
    w: usize,
    lp: SosFilter,
    active: bool,
}

impl Default for Echo {
    fn default() -> Self {
        Self::new()
    }
}

fn round_half_even(x: f64) -> f64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 {
        2.0 * (x / 2.0).round()
    } else {
        r
    }
}

impl Echo {
    pub fn new() -> Echo {
        // Web Audio の既定の Q（1 = 0dB）
        Echo { buf: vec![0.0; SR as usize * 5 * 2], w: 0, lp: SosFilter::new(vec![rbj("lowpass", 6000.0, 1.0)]), active: false }
    }

    /// send_in: インターリーブのステレオ。delay_sec: 刻みごとの遅延（秒）
    pub fn process(&mut self, send_in: &[f64], delay_sec: &[f64]) -> Vec<f64> {
        let n = send_in.len() / 2;
        let mut out = vec![0.0; send_in.len()];
        if !self.active && send_in.iter().all(|&v| v == 0.0) {
            return out; // まだ一度も送っていなければ何もしない（速さのため）
        }
        self.active = true;
        let l = self.buf.len() / 2;
        for s in 0..n / STEP {
            let d = (round_half_even(delay_sec[s] * SR as f64) as i64).clamp(STEP as i64, (l - STEP) as i64) as usize;
            let mut delayed = vec![0.0; STEP * 2];
            for j in 0..STEP {
                let r = (self.w + l - d + j) % l;
                delayed[2 * j] = self.buf[2 * r];
                delayed[2 * j + 1] = self.buf[2 * r + 1];
            }
            self.lp.process(&mut delayed);
            let o = &mut out[s * STEP * 2..(s + 1) * STEP * 2];
            o.copy_from_slice(&delayed);
            for j in 0..STEP {
                let wi = (self.w + j) % l;
                for c in 0..2 {
                    self.buf[2 * wi + c] = send_in[(s * STEP + j) * 2 + c] + 0.55 * delayed[2 * j + c];
                }
            }
            self.w = (self.w + STEP) % l;
        }
        if self.buf.iter().all(|v| v.abs() < 1e-5) {
            self.active = false; // 余韻が消えた
        }
        out
    }
}

/// 再生側のリミッター（Web Audio の DynamicsCompressor: しきい値 −1.5dB・比 20・ニー 0・アタック 2ms・リリース 120ms）に近い動き。
/// DynamicsCompressor はこの設定で約 +0.86dB の持ち上げ（makeup gain）も掛けるので同じだけ掛ける。
#[derive(Debug, Clone)]
pub struct Compressor {
    att: f64,
    rel: f64,
    env: f64,
    g_prev: f64,
}

impl Default for Compressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compressor {
    pub const BLOCK: usize = 64;
    pub const THR_DB: f64 = -1.5;
    pub const RATIO: f64 = 20.0;

    pub fn makeup() -> f64 {
        10f64.powf(0.855 / 20.0)
    }

    pub fn new() -> Compressor {
        Compressor {
            att: (-(Self::BLOCK as f64) / (0.002 * SR as f64)).exp(),
            rel: (-(Self::BLOCK as f64) / (0.12 * SR as f64)).exp(),
            env: 0.0,
            g_prev: 1.0,
        }
    }

    /// インターリーブのステレオ。64 サンプルの倍数の分だけ返す
    pub fn process(&mut self, x: &[f64]) -> Vec<f64> {
        let nb = x.len() / 2 / Self::BLOCK;
        let slope = 1.0 - 1.0 / Self::RATIO;
        let mk = Self::makeup();
        let mut out = Vec::with_capacity(nb * Self::BLOCK * 2);
        let mut env = self.env;
        let mut prev = self.g_prev;
        for b in 0..nb {
            let blk = &x[b * Self::BLOCK * 2..(b + 1) * Self::BLOCK * 2];
            let pk = blk.iter().fold(0.0f64, |m, v| m.max(v.abs()));
            env = pk + (env - pk) * if pk > env { self.att } else { self.rel };
            let over = 20.0 * env.max(1e-9).log10() - Self::THR_DB;
            let g = if over > 0.0 { 10f64.powf(-over * slope / 20.0) } else { 1.0 };
            for j in 0..Self::BLOCK {
                let r = (prev + (g - prev) * (j as f64 / Self::BLOCK as f64)) * mk;
                out.push(blk[2 * j] * r);
                out.push(blk[2 * j + 1] * r);
            }
            prev = g;
        }
        self.env = env;
        self.g_prev = prev;
        out
    }
}

/// 最後の安全装置（サンプル単位の先読み）: 1.5ms 先の山まで見て上限（WAV は −0.3dBFS、MP3 は −1dBFS）を超えないよう下げ、約 50ms で戻す。
#[derive(Debug, Clone)]
pub struct Limiter {
    pub ceil: f64,
    a: f64,
    zi: f64,
    pending: Vec<f64>,
    comp: Compressor,
}

impl Limiter {
    pub const LOOK: usize = 64;

    pub fn new(ceil_db: f64) -> Limiter {
        Limiter {
            ceil: 10f64.powf(ceil_db / 20.0),
            a: (-1.0 / (0.05 * SR as f64 / 3.0)).exp(),
            zi: 0.0,
            pending: vec![0.0; Self::LOOK * 2],
            comp: Compressor::new(),
        }
    }

    pub fn process(&mut self, x: &[f64]) -> Vec<f64> {
        let mut xx = std::mem::take(&mut self.pending);
        xx.extend(self.comp.process(x));
        let m = xx.len() / 2;
        let req: Vec<f64> = (0..m).map(|i| (self.ceil / xx[2 * i].abs().max(xx[2 * i + 1].abs()).max(1e-9)).min(1.0)).collect();
        let look = min_ahead(&req, Self::LOOK);
        // red = lfilter([1-a], [1, -a], 1 - look)（状態は配列の最後まで進める。Python 版と同じ）
        let mut red = vec![0.0; m];
        let mut z = self.zi;
        for i in 0..m {
            let y = (1.0 - self.a) * (1.0 - look[i]) + z;
            z = self.a * y;
            red[i] = y;
        }
        self.zi = z;
        let n = m.saturating_sub(Self::LOOK);
        let mut out = Vec::with_capacity(n * 2);
        for i in 0..n {
            let g = look[i].min(1.0 - red[i]);
            out.push(xx[2 * i] * g);
            out.push(xx[2 * i + 1] * g);
        }
        self.pending = xx[n * 2..].to_vec();
        out
    }

    pub fn flush(&mut self) -> Vec<f64> {
        let c = self.ceil;
        let y = self.pending.iter().map(|v| v.clamp(-c, c)).collect();
        self.pending.clear();
        y
    }
}

/// out[i] = min(v[i..=i+look])（端は最後の値を延ばす。scipy の minimum_filter1d(size=look+1, origin=-look/2, mode='nearest')）
pub fn min_ahead(v: &[f64], look: usize) -> Vec<f64> {
    let n = v.len();
    let mut out = vec![0.0; n];
    let mut dq: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    // 後ろから: 窓 [i, i+look]
    for i in (0..n).rev() {
        while let Some(&b) = dq.back() {
            if v[b] >= v[i] {
                dq.pop_back();
            } else {
                break;
            }
        }
        dq.push_back(i);
        while let Some(&f) = dq.front() {
            if f > i + look {
                dq.pop_front();
            } else {
                break;
            }
        }
        out[i] = v[*dq.front().unwrap()];
    }
    out
}

/// float → 16bit（Python の (clip(x, -1, 1) * 32767).astype("<i2")。0 に向けて切り捨て）
pub fn to_pcm16(x: &[f64]) -> Vec<u8> {
    let mut b = Vec::with_capacity(x.len() * 2);
    for v in x {
        let s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

/// デッキごとの流れ（ストレッチした音 → チャンネル → エコー）
struct DeckChain<'a> {
    audio: DeckAudio<'a>,
    ch: Channel,
    echo: Echo,
}

/// 刻み c0..c1 のデッキの音（チャンネルを通した y と、エコーの戻り e）
struct DeckOut {
    y: Option<Vec<f64>>,
    e: Vec<f64>,
}

impl<'a> DeckChain<'a> {
    fn process(&mut self, d: usize, tracks: &[Track], plan: &Plan, target: f64, c0: usize, c1: usize) -> Result<DeckOut> {
        let n = (c1 - c0) * STEP;
        let tr_idx = &plan.track[d][c0..c1];
        let Some(&first) = tr_idx.iter().find(|&&v| v >= 0) else {
            // 止まっていても余韻は鳴る
            let e = self.echo.process(&vec![0.0; n * 2], &plan.echo_delay[d][c0..c1]);
            self.audio.release(); // 曲が終わったデッキは音声を手放す
            self.audio.prefetch(c1); // 次にこのデッキで鳴らす曲を先に計算しておく
            return Ok(DeckOut { y: None, e });
        };
        let x = self.audio.render(c0, c1)?;
        let lufs = tracks[first as usize].lufs;
        let trim = 10f64.powf((target - lufs).clamp(-12.0, 9.0) / 20.0);
        let x64: Vec<f64> = x.iter().map(|&v| v as f64).collect();
        let mut y = self.ch.process(&x64, trim, &plan.low[d][c0..c1], &plan.hp[d][c0..c1]);
        let jg = &plan.jg[d][c0..c1];
        if jg.iter().any(|&v| v != 1.0) {
            let g = per_sample(jg);
            for (i, v) in y.iter_mut().enumerate() {
                *v *= g[i / 2];
            }
        }
        let es = per_sample(&plan.echo[d][c0..c1]);
        let send: Vec<f64> = y.iter().enumerate().map(|(i, v)| v * es[i / 2]).collect();
        let e = self.echo.process(&send, &plan.echo_delay[d][c0..c1]);
        Ok(DeckOut { y: Some(y), e })
    }
}

/// 操作の表どおりに、各曲をストレッチしてミキサーに通す。split=true なら左＝デッキ0・右＝デッキ1（一致の試験用。音量の自動の動きを掛けない）。
pub struct Mixdown<'a> {
    tracks: &'a [Track],
    plan: &'a Plan,
    split: bool,
    target: f64,
    gains: [Vec<f64>; 2],
    chains: [DeckChain<'a>; 2],
    lim: Limiter,
}

impl<'a> Mixdown<'a> {
    pub fn new(tracks: &'a [Track], plan: &'a Plan, settings: &Map<String, Value>, split: bool, ceil_db: f64) -> Mixdown<'a> {
        Self::with_stems(tracks, plan, settings, split, ceil_db, super::stretch::default_stem_reader())
    }

    /// 分離した音の読み方を指定する（試験用）
    pub fn with_stems(
        tracks: &'a [Track],
        plan: &'a Plan,
        settings: &Map<String, Value>,
        split: bool,
        ceil_db: f64,
        stems: StemReader,
    ) -> Mixdown<'a> {
        let target = value_f64(settings.get("lufs_target")).unwrap_or(-14.0);
        let curve = settings.get("xfade_curve").and_then(|v| v.as_str()).unwrap_or("smooth");
        let (g0, g1) = xfade_gains(&plan.xf, curve);
        let mk = |d: usize| DeckChain { audio: DeckAudio::new(d, tracks, plan, stems.clone()), ch: Channel::new(), echo: Echo::new() };
        Mixdown { tracks, plan, split, target, gains: [g0, g1], chains: [mk(0), mk(1)], lim: Limiter::new(ceil_db) }
    }

    /// デッキ d の刻み c0..c1 のストレッチした音（ミキサーの手前。インターリーブのステレオ）
    pub fn deck_audio(&mut self, d: usize, c0: usize, c1: usize) -> Result<Vec<f32>> {
        self.chains[d].audio.render(c0, c1)
    }

    /// write: 16bit ステレオの PCM。progress(0..1) が false を返したら中止する（エラー「中止しました」）。
    pub fn run(&mut self, write: &mut dyn FnMut(&[u8]), progress: &mut dyn FnMut(f64) -> bool) -> Result<()> {
        let plan = self.plan;
        let n_all = plan.n;
        let mut c0 = 0;
        while c0 < n_all {
            let c1 = n_all.min(c0 + CH);
            let n = (c1 - c0) * STEP;
            let (tracks, target) = (self.tracks, self.target);
            let [a, b] = &mut self.chains;
            // 2 つのデッキ（ストレッチ・チャンネル・エコー）は別々のスレッドで
            let (ra, rb) = rayon::join(|| a.process(0, tracks, plan, target, c0, c1), || b.process(1, tracks, plan, target, c0, c1));
            let outs = [ra?, rb?];
            let mut mix = vec![0.0f64; n * 2];
            for (d, o) in outs.iter().enumerate() {
                match &o.y {
                    None => {
                        if self.split {
                            add_side(&mut mix, &o.e, d);
                        } else {
                            for (m, v) in mix.iter_mut().zip(&o.e) {
                                *m += v;
                            }
                        }
                    }
                    Some(y) => {
                        if self.split {
                            add_side(&mut mix, y, d); // 試験用: 音量の自動の動きを掛けずに、そのデッキの音を片側へ
                        } else {
                            let g = self.gain(d, c0, c1);
                            for i in 0..n {
                                mix[2 * i] += y[2 * i] * g[i] + o.e[2 * i];
                                mix[2 * i + 1] += y[2 * i + 1] * g[i] + o.e[2 * i + 1];
                            }
                        }
                    }
                }
            }
            let out = if self.split { mix } else { self.lim.process(&mix) };
            write(&to_pcm16(&out));
            if !progress(c1 as f64 / n_all as f64) {
                return Err(super::render::cancelled_error());
            }
            c0 = c1;
        }
        if !self.split {
            let y = self.lim.flush();
            write(&to_pcm16(&y));
        }
        Ok(())
    }

    /// デッキ d のクロスフェーダーの倍率（サンプルごと）。次の曲を前の曲の終わりで入れた刻みは、刻みの中の f の位置で
    /// 前の曲を消し、次の曲を鳴らす（刻みの間の直線のままだと 11.6ms のうちに混ざる。design_cutin.md ①）。
    pub fn gain(&self, d: usize, c0: usize, c1: usize) -> Vec<f64> {
        let gains = &self.gains[d];
        let mut g = per_sample(&gains[c0..c1]);
        let n = self.plan.n;
        for c in &self.plan.cuts {
            let k = c.k;
            if !(c0 <= k && k < c1) {
                continue;
            }
            let a = (k - c0) * STEP;
            for j in 0..STEP {
                let w = ((j as f64 - c.f * STEP as f64) / CUT_FADE).clamp(0.0, 1.0);
                g[a + j] = if d == c.out { (1.0 - w) * gains[k] } else { w * gains[(k + 1).min(n - 1)] };
            }
        }
        g
    }
}

/// そのデッキの音の (L+R)/2 を、左（デッキ0）か右（デッキ1）へ
fn add_side(mix: &mut [f64], y: &[f64], d: usize) {
    for i in 0..mix.len() / 2 {
        mix[2 * i + d] += (y[2 * i] + y[2 * i + 1]) * 0.5;
    }
}

