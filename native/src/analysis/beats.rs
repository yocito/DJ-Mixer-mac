//! 拍の検出の後処理（djmix/analysis/beats.py）。
//!
//! - 倍/半テンポの取り違え区間の補正（曲の途中で 2 倍/0.5 倍に入れ替わる区間だけ）
//! - 拍時刻の平滑化（20ms 刻みの量子化を局所回帰で除去。本物のテンポ変化は残す）
//! - 拍なし区間（ルバート・ブレイク・無音）
use crate::util::median;

/// ibi[i]（beats[i]→beats[i+1]）が拍の途切れ（局所中央値の k 倍超かつ min_gap 秒超）なら true。長さは len(beats)-1。
pub fn gap_mask_k(beats: &[f64], k: f64, min_gap: f64) -> Vec<bool> {
    if beats.len() < 2 {
        return vec![];
    }
    let ibi: Vec<f64> = beats.windows(2).map(|w| w[1] - w[0]).collect();
    let n = ibi.len();
    let mut out = vec![false; n];
    let mut buf: Vec<f64> = Vec::with_capacity(16);
    for i in 0..n {
        let d = ibi[i];
        let lo = i.saturating_sub(8);
        let hi = (i + 9).min(n);
        let local = if hi - lo > 1 {
            buf.clear();
            buf.extend_from_slice(&ibi[lo..i]);
            buf.extend_from_slice(&ibi[i + 1..hi]);
            median(&buf)
        } else {
            d
        };
        out[i] = d > k * local && d > min_gap;
    }
    out
}

pub fn gap_mask(beats: &[f64]) -> Vec<bool> {
    gap_mask_k(beats, 1.75, 1.0)
}

/// 拍ごとの区間の番号（拍の途切れで 1 つ増える）。Python の `np.concatenate([[0], np.cumsum(gap_mask(beats))])`
pub fn segment_ids(beats: &[f64]) -> Vec<usize> {
    let g = gap_mask(beats);
    let mut out = Vec::with_capacity(beats.len());
    if beats.is_empty() {
        return out;
    }
    out.push(0);
    let mut s = 0;
    for x in g {
        if x {
            s += 1;
        }
        out.push(s);
    }
    out
}

// ---- ここから下は DSP の移植（detect の後の処理一式: local_tempo, fix_octave, smooth, no_beat_regions, raw_metrics, classify） ----

use super::dsp::npy::{mean_f32, mean_f64, median_f64, nanmedian_f64, pairwise_sum_f32, py_round, rint};
use super::types::{Metrics, FPS};
use crate::util::{polyfit1, searchsorted_left};

/// 拍時刻 → 50fps のフレーム番号（np.clip((beats * FPS).round().astype(int), 0, n-1)。round は偶数丸め）
fn frames_of(beats: &[f64], n: usize) -> Vec<usize> {
    beats.iter().map(|&b| (rint(b * FPS) as i64).clamp(0, (n as i64 - 1).max(0)) as usize).collect()
}

/// 各拍の前後 win 拍（拍の途切れはまたがない）に直線を当てはめた傾きから求めた局所 BPM。求まらない拍は NaN。
pub fn local_tempo_win(beats: &[f64], win: usize) -> Vec<f64> {
    let n = beats.len();
    let mut out = vec![f64::NAN; n];
    if n < 3 {
        return out;
    }
    let seg = segment_ids(beats);
    let h = win / 2;
    let mut xs = Vec::with_capacity(win + 1);
    let mut ys = Vec::with_capacity(win + 1);
    for i in 0..n {
        let (lo, hi) = (i.saturating_sub(h), (i + h + 1).min(n));
        xs.clear();
        ys.clear();
        for j in lo..hi {
            if seg[j] == seg[i] {
                xs.push(j as f64 - i as f64);
                ys.push(beats[j]);
            }
        }
        if xs.len() < 3 {
            continue;
        }
        let slope = polyfit1(&xs, &ys).0;
        if slope > 0.0 {
            out[i] = 60.0 / slope;
        }
    }
    out
}

/// local_tempo(beats, win=8)
pub fn local_tempo(beats: &[f64]) -> Vec<f64> {
    local_tempo_win(beats, 8)
}

/// mask が min_len 以上続く区間 [start, end) の列（Python の _runs）
pub fn runs(mask: &[bool], min_len: usize) -> Vec<(usize, usize)> {
    let mut out = vec![];
    let mut start: Option<usize> = None;
    for i in 0..=mask.len() {
        let m = i < mask.len() && mask[i];
        match (m, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                if i - s >= min_len {
                    out.push((s, i));
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// 曲の中で局所テンポが曲全体の約 2 倍/約 0.5 倍になっている区間を補正する。戻り値: (拍時刻, 補正した区間数)。
///
/// 2 倍区間は 1 拍おきに間引く。残す位相は、区間の直前の拍から通常の拍間隔で続く側（なければ拍確率の高い側）。
/// 0.5 倍区間は拍の中点を補う。
pub fn fix_octave(beats: &[f64], beat_prob: &[f32]) -> (Vec<f64>, u32) {
    if beats.len() < 16 {
        return (beats.to_vec(), 0);
    }
    let lt = local_tempo(beats);
    let reference = nanmedian_f64(&lt);
    let ratio: Vec<f64> = lt.iter().map(|v| v / reference).collect();
    let dbl = runs(&ratio.iter().map(|&r| r > 1.65 && r < 2.35).collect::<Vec<_>>(), 8);
    let hlf = runs(&ratio.iter().map(|&r| r > 0.43 && r < 0.57).collect::<Vec<_>>(), 4);
    let mut edits: Vec<(bool, usize, usize)> = dbl.iter().map(|&(a, b)| (true, a, b)).collect();
    edits.extend(hlf.iter().map(|&(a, b)| (false, a, b)));
    // 後ろから処理すると添字がずれない（Python の sorted(key=-start) は安定）
    edits.sort_by(|x, y| y.1.cmp(&x.1));
    let mut out = beats.to_vec();
    let mut fixed = 0;
    for (is_double, a, b) in edits {
        let seg: Vec<f64> = out[a..b].to_vec();
        let repl: Vec<f64> = if is_double {
            let ref_ibi = 60.0 / reference;
            let phase = if a > 0 {
                let expect = out[a - 1] + ref_ibi;
                // np.argmin([|seg0 − expect|, |seg1 − expect|])（同じなら 0）
                if seg.len() > 1 && (seg[1] - expect).abs() < (seg[0] - expect).abs() {
                    1
                } else {
                    0
                }
            } else {
                let fr = frames_of(&seg, beat_prob.len());
                let even: Vec<f32> = fr.iter().step_by(2).map(|&f| beat_prob.get(f).copied().unwrap_or(0.0)).collect();
                let odd: Vec<f32> = fr.iter().skip(1).step_by(2).map(|&f| beat_prob.get(f).copied().unwrap_or(0.0)).collect();
                (pairwise_sum_f32(&even) < pairwise_sum_f32(&odd)) as usize
            };
            seg.iter().skip(phase).step_by(2).copied().collect()
        } else {
            let mut merged = Vec::with_capacity(seg.len() * 2);
            for i in 0..seg.len() {
                merged.push(seg[i]);
                if i + 1 < seg.len() {
                    merged.push((seg[i] + seg[i + 1]) / 2.0);
                }
            }
            merged
        };
        out.splice(a..b, repl);
        fixed += 1;
    }
    (out, fixed)
}

/// 前後 half 拍の局所回帰で当てはめ直す。残差が keep_ms を超える拍は本物のテンポ変化とみなして元の値を残す。
pub fn smooth_with(beats: &[f64], half: usize, keep_ms: f64) -> Vec<f64> {
    let n = beats.len();
    if n < 5 {
        return beats.to_vec();
    }
    let seg = segment_ids(beats);
    let mut out = beats.to_vec();
    let mut xs = Vec::with_capacity(2 * half + 1);
    let mut ys = Vec::with_capacity(2 * half + 1);
    for i in 0..n {
        xs.clear();
        ys.clear();
        for j in i.saturating_sub(half)..(i + half + 1).min(n) {
            if seg[j] == seg[i] {
                xs.push(j as f64 - i as f64);
                ys.push(beats[j]);
            }
        }
        if xs.len() < 4 {
            continue;
        }
        // np.polyval(np.polyfit(x, y, 1), 0) = 切片
        let v = polyfit1(&xs, &ys).1;
        if (v - beats[i]).abs() * 1000.0 < keep_ms {
            out[i] = v;
        }
    }
    // 平滑化で順序が入れ替わることはないが、念のため単調増加を保証する（np.maximum.accumulate）
    for i in 1..n {
        if out[i - 1].is_nan() || out[i - 1] > out[i] {
            out[i] = out[i - 1];
        }
    }
    out
}

/// smooth(beats, half=4, keep_ms=30)
pub fn smooth(beats: &[f64]) -> Vec<f64> {
    smooth_with(beats, 4, 30.0)
}

/// SYNC に使えない区間: 拍が途切れている区間、拍の確からしさが低い拍が続く区間、
/// 倍/半の補正後もテンポが曲の基準から大きく外れたままの区間（3 連符のノリに引っ張られた 1.5 倍など）。
pub fn no_beat_regions(beats: &[f64], beat_prob: &[f32], duration: f64) -> Vec<(f64, f64)> {
    let n = beats.len();
    if n < 8 {
        return vec![(0.0, duration)];
    }
    let mut regions: Vec<(f64, f64)> = vec![];
    for (i, g) in gap_mask(beats).into_iter().enumerate() {
        if g {
            regions.push((beats[i], beats[i + 1]));
        }
    }
    let fr = frames_of(beats, beat_prob.len());
    // float32 の配列と Python の float の比較は float32 で（NEP 50）
    let low: Vec<bool> = fr.iter().map(|&f| beat_prob.get(f).map_or(false, |&p| p < 0.3f32)).collect();
    for (a, b) in runs(&low, 8) {
        regions.push((beats[a], beats[b - 1]));
    }
    let lt = local_tempo(beats);
    let reference = nanmedian_f64(&lt);
    let off: Vec<bool> = lt
        .iter()
        .map(|&v| {
            let d = (v / reference - 1.0).abs();
            (if d.is_nan() { 0.0 } else { d }) > 0.25
        })
        .collect();
    for (a, b) in runs(&off, 6) {
        regions.push((beats[a.saturating_sub(1)], beats[b.min(n - 1)]));
    }
    if beats[0] > 3.0 {
        regions.push((0.0, beats[0]));
    }
    if duration - beats[n - 1] > 3.0 {
        regions.push((beats[n - 1], duration));
    }
    regions.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let mut merged: Vec<(f64, f64)> = vec![];
    for (s, e) in regions {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        merged.push((s, e));
    }
    merged.into_iter().map(|(s, e)| (py_round(s, 3), py_round(e, 3))).collect()
}

/// Phase 0 の分類(A/B/C)に使った指標。分類の閾値が Phase 0 で決めたものなので、補正前の拍で測る。
pub fn raw_metrics(raw_beats: &[f64], raw_downs: &[f64], beat_prob: &[f32]) -> Metrics {
    let fr = frames_of(raw_beats, beat_prob.len());
    let conf = if fr.is_empty() || beat_prob.is_empty() {
        0.0
    } else {
        mean_f32(&fr.iter().map(|&f| beat_prob[f]).collect::<Vec<_>>()) as f64
    };
    let lt = local_tempo(raw_beats);
    let valid: Vec<f64> = lt.into_iter().filter(|v| v.is_finite()).collect();
    let bpm = if valid.is_empty() { 0.0 } else { median_f64(&valid) };
    let unstable = if valid.is_empty() {
        1.0
    } else {
        valid.iter().filter(|&&v| (v / bpm - 1.0).abs() > 0.25).count() as f64 / valid.len() as f64
    };
    let mut local = vec![];
    let mut s = 0usize;
    let ii: Vec<f64> = (0..16).map(|i| i as f64).collect();
    while s + 16 < raw_beats.len() {
        let seg = &raw_beats[s..s + 16];
        let d: Vec<f64> = seg.windows(2).map(|w| w[1] - w[0]).collect();
        let dmax = d.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if !(dmax > 1.75 * median_f64(&d)) {
            let (p0, p1) = polyfit1(&ii, seg);
            let sq: Vec<f64> = (0..16).map(|i| (seg[i] - (p0 * ii[i] + p1)).powi(2)).collect();
            local.push(mean_f64(&sq).sqrt());
        }
        s += 16;
    }
    let bi: Vec<usize> = raw_downs.iter().map(|&d| searchsorted_left(raw_beats, d - 1e-3)).collect();
    let lengths: Vec<i64> = bi.windows(2).map(|w| w[1] as i64 - w[0] as i64).collect();
    let dom_ratio = if lengths.is_empty() {
        0.0
    } else {
        let mut cnt = std::collections::BTreeMap::new();
        for &l in &lengths {
            *cnt.entry(l).or_insert(0usize) += 1;
        }
        *cnt.values().max().unwrap() as f64 / lengths.len() as f64
    };
    Metrics {
        conf,
        unstable,
        local16_ms: if local.is_empty() { 999.0 } else { median_f64(&local) * 1000.0 },
        downbeat_consistency: dom_ratio,
    }
}

/// Phase 0 の A/B/C 分類（phase0/classify.py と同じ規則）。
pub fn classify(m: &Metrics, no_beat_total: f64, drift_pct: f64) -> (String, Vec<String>) {
    if m.conf < 0.9 || m.local16_ms > 30.0 || m.unstable > 0.4 {
        return ("C".into(), vec!["拍の検出が不安定".into()]);
    }
    let mut reasons = vec![];
    // Python の {:.0%} は 100 倍して {:.0f}（偶数丸め）
    if m.unstable > 0.05 {
        reasons.push(format!("倍/半テンポの取り違え区間 {:.0}%", m.unstable * 100.0));
    }
    if m.downbeat_consistency < 0.85 {
        reasons.push(format!("小節頭の揺らぎ（一致率 {:.0}%）", m.downbeat_consistency * 100.0));
    }
    if drift_pct.abs() > 5.0 {
        reasons.push(format!("テンポが曲中で {:+.0}% 変化", drift_pct));
    }
    if no_beat_total > 15.0 {
        reasons.push(format!("拍なし区間 {:.0}秒", no_beat_total));
    }
    (if reasons.is_empty() { "A" } else { "B" }.into(), reasons)
}
