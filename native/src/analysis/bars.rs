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
            let fr = ((b * FPS).round() as i64).clamp(0, n - 1);
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
