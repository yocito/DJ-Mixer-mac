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
