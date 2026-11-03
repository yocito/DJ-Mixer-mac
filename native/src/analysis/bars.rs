//! 拍子と小節頭の位相（djmix/analysis/bars.py）。
//!
//! 各拍の「小節頭らしさ」（ダウンビート確率）を使い、
//! 1. 拍子 M: 小節頭らしさが M 拍ごとに最もはっきり周期的に現れる M（3〜7）。4 を優先
//! 2. 位相: 8 小節の窓ごとに、M 拍おきの小節頭らしさの合計が最大になる位相
//! 3. 信頼度: 窓の中での 1 位と 2 位の位相の差
use super::beats::segment_ids;
use super::types::{Bar, FPS};
use crate::util::round;

pub const METERS: [u32; 5] = [3, 4, 5, 6, 7];

/// 各拍の小節頭らしさ（拍の前後 1 フレームの最大値）。
pub fn downbeat_strength(beats: &[f64], down_prob: &[f32]) -> Vec<f64> {
    let n = down_prob.len() as i64;
    if n == 0 {
        return vec![0.0; beats.len()];
    }
    beats
        .iter()
        .map(|&b| {
            let fr = ((b * FPS).round_ties_even() as i64).clamp(0, n - 1); // numpy の round（偶数丸め）
            let lo = (fr - 1).clamp(0, n - 1);
            let hi = (fr + 1).clamp(0, n - 1);
            (down_prob[lo as usize] as f64).max(down_prob[fr as usize] as f64).max(down_prob[hi as usize] as f64)
        })
        .collect()
}

/// numpy の argsort(sc)[::-1] と同じ順（同点は後ろの番号が先）
fn order_desc(sc: &[f64]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..sc.len()).collect();
    idx.sort_by(|&a, &b| sc[a].partial_cmp(&sc[b]).unwrap_or(std::cmp::Ordering::Equal).then(a.cmp(&b)));
    idx.reverse();
    idx
}

/// 小節のリスト [{beat, time, conf}]。拍の途切れ（拍なし区間）の後は位相を推定し直す。
pub fn bar_phases(beats: &[f64], strength: &[f64], m: u32) -> Vec<Bar> {
    let n = beats.len();
    let m = m as usize;
    if n < m || m == 0 {
        return vec![];
    }
    let seg = segment_ids(beats);
    let nseg = seg.last().copied().unwrap_or(0) + 1;
    let mut bars = vec![];
    for sid in 0..nseg {
        let idx: Vec<usize> = (0..n).filter(|&i| seg[i] == sid).collect();
        if idx.len() < m {
            continue;
        }
        let s: Vec<f64> = idx.iter().map(|&i| strength[i]).collect();
        let len = idx.len();
        let win = m * 8;
        let mut phase = vec![0usize; len];
        let mut conf = vec![0f64; len];
        let mut c = 0;
        while c < len {
            let a = c.saturating_sub(win / 2);
            let b = (c + win / 2).min(len);
            let w = &s[a..b];
            let sc: Vec<f64> = (0..m)
                .map(|p| {
                    let off = ((p as i64 - a as i64).rem_euclid(m as i64)) as usize;
                    let mut sum = 0.0;
                    let mut cnt = 0usize;
                    let mut k = off;
                    while k < w.len() {
                        sum += w[k];
                        cnt += 1;
                        k += m;
                    }
                    sum / cnt.max(1) as f64
                })
                .collect();
            let order = order_desc(&sc);
            let p = order[0];
            let margin = if m > 1 { (sc[order[0]] - sc[order[1]]) / (sc[order[0]] + 1e-6) } else { 0.0 };
            for j in c..(c + m).min(len) {
                phase[j] = p;
                conf[j] = margin;
            }
            c += m;
        }
        let mut last: i64 = -1_000_000_000;
        for j in 0..len {
            if (j as i64 - phase[j] as i64).rem_euclid(m as i64) == 0 && j as i64 - last >= m as i64 - 1 {
                bars.push(Bar {
                    beat: idx[j] as i64,
                    time: round(beats[idx[j]], 4),
                    conf: round((conf[j] * 2.0).clamp(0.0, 1.0), 3),
                    meter: None,
                });
                last = j as i64;
            }
        }
    }
    bars
}

// ---- ここから下は DSP の移植（estimate_meter と、拍子が途中で変わる曲の Viterbi: track_meter, bar_phases_varying, meter_summary） ----
//
// 小節頭らしさ（downbeat_strength）は Python では float32 の配列（Beat This! の確率が float32 のため）。
// その平均や差は numpy が float32 で計算するので、estimate_meter と bar_phases_varying の conf は float32 の演算順まで真似る
// （meter_scores の小数 4 桁・conf の小数 3 桁が Python と一致するように）。track_meter は Python でも float64。

use super::dsp::npy::{mean_f32, mean_f64, median_f32, py_round};
use std::collections::BTreeMap;

/// 位相を最良にしたときの「M 拍おきの平均 − それ以外の平均」。窓ごとに位相を選び、中央値をとる（float32）。
fn contrast(s: &[f32], m: usize) -> f32 {
    let win = m * 8;
    let stop = (s.len() as i64 - win as i64 + 1).max(1) as usize;
    let mut vals: Vec<f32> = vec![];
    let mut on = Vec::with_capacity(win);
    let mut off = Vec::with_capacity(win);
    let mut st = 0;
    while st < stop {
        let w = &s[st..(st + win).min(s.len())];
        st += m * 2;
        if w.len() < m * 4 {
            continue;
        }
        let mut best: Option<f32> = None;
        for p in 0..m {
            on.clear();
            off.clear();
            for (i, &v) in w.iter().enumerate() {
                if i >= p && (i - p) % m == 0 {
                    on.push(v);
                } else {
                    off.push(v);
                }
            }
            let d = mean_f32(&on) - mean_f32(&off);
            // Python の max(): 大きいときだけ入れ替える（同じなら先のもの）
            if best.map_or(true, |b| d > b) {
                best = Some(d);
            }
        }
        vals.push(best.unwrap());
    }
    if vals.is_empty() {
        0.0
    } else {
        median_f32(&vals)
    }
}

/// 拍子 M（3〜7）と、拍子ごとのコントラスト（小数 4 桁に丸めたもの。キーは "3"〜"7"）。
/// 4 拍子を基準にし、他の拍子は明確に上回るときだけ採用する（ジャズの 4 拍子が 2 や 8 と紛れやすい）。
pub fn estimate_meter(strength: &[f64]) -> (u32, BTreeMap<String, f64>) {
    let s: Vec<f32> = strength.iter().map(|&v| v as f32).collect();
    let scores: Vec<(u32, f64)> = METERS.iter().map(|&m| (m, contrast(&s, m as usize) as f64)).collect();
    let get = |m: u32| scores.iter().find(|x| x.0 == m).unwrap().1;
    let mut best = scores[0];
    for &x in &scores[1..] {
        if x.1 > best.1 {
            best = x;
        }
    }
    let mut best = best.0;
    if best != 4 && get(best) < get(4) * 1.15 + 0.02 {
        best = 4;
    }
    // 6 は 3 の倍数なので、3 と区別できないときは 3 にそろえる
    if best == 6 && get(3) >= get(6) * 0.9 {
        best = 3;
    }
    (best, scores.iter().map(|&(m, v)| (m.to_string(), py_round(v, 4))).collect())
}

// 状態は (拍子 m, 小節内の位置 b)。小節の途中は b → b+1 だけ、小節の終わりで同じ拍子の頭へ（罰則なし）か別の拍子の頭へ（罰則 λ）。
// 観測は、小節頭の状態に log p、それ以外に log(1−p)。4/4 の小節頭には小さな加点。λ=8 は合成データで決めた値（bars.py の説明）。
pub const SWITCH_PENALTY: f64 = 8.0;
pub const PRIOR4: f64 = 1.5;
/// 局所の平均の窓（拍）
pub const NEUTRAL_WIN: usize = 32;

/// 各拍の前後 win 拍の小節頭らしさの平均（観測を「その辺りの平均と比べてどうか」にするため）。
pub fn local_baseline(strength: &[f64], win: usize) -> Vec<f64> {
    let n = strength.len();
    if n == 0 {
        return vec![];
    }
    let pad: Vec<f64> = if n > win {
        // np.pad(mode="reflect")（端の値は繰り返さない）
        let (l, r) = (win / 2, win - win / 2 - 1);
        let mut p = Vec::with_capacity(n + l + r);
        for k in 0..l {
            p.push(strength[l - k]);
        }
        p.extend_from_slice(strength);
        for k in 0..r {
            p.push(strength[n - 2 - k]);
        }
        p
    } else {
        vec![mean_f64(strength); n + win - 1]
    };
    let k = 1.0 / win as f64;
    (0..n).map(|i| pad[i..i + win].iter().fold(0.0, |acc, &v| acc + v * k)).collect()
}

/// track_meter の引数（Python のキーワード引数の既定値）
#[derive(Clone, Debug)]
pub struct MeterParams {
    pub meters: Vec<u32>,
    pub switch_penalty: f64,
    pub prior4: f64,
    pub eps: f64,
    pub neutral: bool,
}

impl Default for MeterParams {
    fn default() -> Self {
        MeterParams { meters: METERS.to_vec(), switch_penalty: SWITCH_PENALTY, prior4: PRIOR4, eps: 1e-6, neutral: true }
    }
}

/// 各拍の小節頭らしさから、拍子の途中変化を許す Viterbi で、拍ごとの (拍子 m, 小節内の位置 b) を返す（b=1 が小節頭）。
pub fn track_meter(strength: &[f64]) -> Vec<(u32, u32)> {
    track_meter_with(strength, &MeterParams::default())
}

pub fn track_meter_with(strength: &[f64], prm: &MeterParams) -> Vec<(u32, u32)> {
    let states: Vec<(u32, u32)> = prm.meters.iter().flat_map(|&m| (1..=m).map(move |b| (m, b))).collect();
    let ns = states.len();
    let kk = strength.len();
    if kk == 0 {
        return vec![];
    }
    let idx = |m: u32, b: u32| states.iter().position(|&s| s == (m, b)).unwrap();
    // 各状態への遷移元 (元の状態, 遷移の点)。Python の trans 行列の -inf でない所
    let mut preds: Vec<Vec<(usize, f64)>> = vec![vec![]; ns];
    for (i, &(m, b)) in states.iter().enumerate() {
        if b < m {
            preds[idx(m, b + 1)].push((i, 0.0));
        } else {
            for &m2 in &prm.meters {
                preds[idx(m2, 1)].push((i, if m2 == m { 0.0 } else { -prm.switch_penalty }));
            }
        }
    }
    for pv in preds.iter_mut() {
        pv.sort_by_key(|x| x.0); // argmax は小さい番号が先
    }
    let is_down: Vec<bool> = states.iter().map(|&(_, b)| b == 1).collect();
    let bias: Vec<f64> = states.iter().map(|&(m, b)| if m == 4 && b == 1 { prm.prior4 / 4.0 } else { 0.0 }).collect();
    let p: Vec<f64> = strength.iter().map(|&v| v.clamp(prm.eps, 1.0 - prm.eps)).collect();
    let pb: Vec<f64> = if prm.neutral {
        local_baseline(&p, NEUTRAL_WIN).into_iter().map(|v| v.clamp(prm.eps, 1.0 - prm.eps)).collect()
    } else {
        vec![]
    };
    let obs = |k: usize, s: usize| -> f64 {
        let mut o = if is_down[s] { p[k].ln() } else { (1.0 - p[k]).ln() } + bias[s];
        if prm.neutral {
            o += if is_down[s] { ((1.0 - pb[k]) / pb[k]).ln() } else { 0.0 };
        }
        o
    };
    let mut score: Vec<f64> = (0..ns).map(|s| obs(0, s) + if states[s].0 == 4 { prm.prior4 } else { 0.0 }).collect();
    let mut back = vec![0u16; kk * ns];
    let mut next = vec![0.0; ns];
    for k in 1..kk {
        for j in 0..ns {
            let mut bi = usize::MAX;
            let mut bv = f64::NEG_INFINITY;
            for &(i, t) in &preds[j] {
                let c = score[i] + t;
                if bi == usize::MAX || c > bv {
                    bi = i;
                    bv = c;
                }
            }
            back[k * ns + j] = bi as u16;
            next[j] = bv + obs(k, j);
        }
        std::mem::swap(&mut score, &mut next);
    }
    let mut last = 0;
    for s in 1..ns {
        if score[s] > score[last] {
            last = s;
        }
    }
    let mut path = vec![last];
    for k in (1..kk).rev() {
        last = back[k * ns + last] as usize;
        path.push(last);
    }
    path.reverse();
    path.into_iter().map(|s| states[s]).collect()
}

/// 拍子の途中変化を許した小節のリスト（Bar の meter に拍子）。拍の途切れ（拍なし区間）ごとに分けて Viterbi を掛ける。
/// conf は bar_phases と同じ尺度（小節頭の小節頭らしさと、同じ小節の他の拍との差。float32 で計算）。
pub fn bar_phases_varying(beats: &[f64], strength: &[f64]) -> Vec<Bar> {
    bar_phases_varying_with(beats, strength, &MeterParams::default())
}

pub fn bar_phases_varying_with(beats: &[f64], strength: &[f64], prm: &MeterParams) -> Vec<Bar> {
    let n = beats.len();
    if n < 3 {
        return vec![];
    }
    let seg = segment_ids(beats);
    let nseg = seg.last().copied().unwrap_or(0) + 1;
    let mut bars = vec![];
    for sid in 0..nseg {
        let idx: Vec<usize> = (0..n).filter(|&i| seg[i] == sid).collect();
        if idx.len() < 3 {
            continue;
        }
        let s: Vec<f64> = idx.iter().map(|&i| strength[i]).collect();
        let s32: Vec<f32> = s.iter().map(|&v| v as f32).collect();
        let path = track_meter_with(&s, prm);
        let heads: Vec<usize> = (0..path.len()).filter(|&j| path[j].1 == 1).collect();
        for (a, &j) in heads.iter().enumerate() {
            let m = path[j].0;
            let end = if a + 1 < heads.len() { heads[a + 1] } else { idx.len().min(j + m as usize) };
            let rest = &s32[(j + 1).min(end)..end];
            let margin: f32 = if rest.is_empty() { 0.0 } else { (s32[j] - mean_f32(rest)) / (s32[j] + 1e-6f32) };
            bars.push(Bar {
                beat: idx[j] as i64,
                time: py_round(beats[idx[j]], 4),
                conf: py_round((margin * 2.0).clamp(0.0, 1.0) as f64, 3),
                meter: Some(m),
            });
        }
    }
    bars
}

/// bar_phases_varying の結果のまとめ（多数決の拍子・切り替えの回数・拍子ごとの小節数）
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MeterSummary {
    pub meter: u32,
    pub switches: usize,
    pub counts: BTreeMap<u32, usize>,
}

/// 多数決の拍子（既存の meter と互換。同数なら小さい拍子）・切り替えの回数・拍子ごとの小節数。
pub fn meter_summary(bars: &[Bar]) -> MeterSummary {
    let ms: Vec<u32> = bars.iter().filter_map(|b| b.meter).collect();
    if ms.is_empty() {
        return MeterSummary { meter: 4, switches: 0, counts: BTreeMap::new() };
    }
    let mut counts = BTreeMap::new();
    for &m in &ms {
        *counts.entry(m).or_insert(0usize) += 1;
    }
    let mut meter = 0;
    let mut best = 0;
    for (&m, &c) in &counts {
        if c > best {
            best = c;
            meter = m;
        }
    }
    MeterSummary { meter, switches: ms.windows(2).filter(|w| w[0] != w[1]).count(), counts }
}
