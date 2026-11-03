//! 共通の小道具: 数値（numpy の同名の関数と同じ結果になるもの）・時刻・丸め。
use std::time::{SystemTime, UNIX_EPOCH};

/// UNIX 時刻（秒、小数つき。Python の time.time()）
pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Python の round(x, n) に近い丸め（ちょうど半分のときの扱いだけ違うことがある）。
pub fn round(x: f64, n: i32) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let m = 10f64.powi(n);
    let r = (x * m).round() / m;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

pub fn round_f32(x: f32, n: i32) -> f64 {
    round(x as f64, n)
}

pub fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.iter().sum::<f64>() / v.len() as f64
}

/// 母標準偏差（numpy の std。ddof=0）
pub fn std(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let m = mean(v);
    (v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / v.len() as f64).sqrt()
}

/// numpy.percentile（線形補間。q は 0..100）。空なら NaN。
pub fn percentile(v: &[f64], q: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s: Vec<f64> = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    percentile_sorted(&s, q)
}

pub fn percentile_sorted(s: &[f64], q: f64) -> f64 {
    if s.is_empty() {
        return f64::NAN;
    }
    let pos = (q / 100.0).clamp(0.0, 1.0) * (s.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let f = pos - lo as f64;
    s[lo] + (s[hi] - s[lo]) * f
}

/// numpy.quantile（q は 0..1）
pub fn quantile(v: &[f64], q: f64) -> f64 {
    percentile(v, q * 100.0)
}

pub fn median(v: &[f64]) -> f64 {
    percentile(v, 50.0)
}

/// numpy.nanmedian（NaN を除いた中央値。全部 NaN なら NaN）
pub fn nanmedian(v: &[f64]) -> f64 {
    let f: Vec<f64> = v.iter().copied().filter(|x| !x.is_nan()).collect();
    median(&f)
}

/// 1 次の最小二乗（numpy.polyfit(x, y, 1)）。戻り値 (傾き, 切片)
pub fn polyfit1(x: &[f64], y: &[f64]) -> (f64, f64) {
    let n = x.len() as f64;
    let mx = x.iter().sum::<f64>() / n;
    let my = y.iter().sum::<f64>() / n;
    let mut sxx = 0.0;
    let mut sxy = 0.0;
    for (a, b) in x.iter().zip(y) {
        sxx += (a - mx) * (a - mx);
        sxy += (a - mx) * (b - my);
    }
    let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    (slope, my - slope * mx)
}

/// numpy.searchsorted（side='left'）: v[i-1] < x <= v[i] となる i
pub fn searchsorted_left(v: &[f64], x: f64) -> usize {
    v.partition_point(|&a| a < x)
}

/// numpy.searchsorted（side='right'）: v[i-1] <= x < v[i] となる i
pub fn searchsorted_right(v: &[f64], x: f64) -> usize {
    v.partition_point(|&a| a <= x)
}

/// numpy.interp（xp は昇順）
pub fn interp(x: f64, xp: &[f64], fp: &[f64]) -> f64 {
    if xp.is_empty() {
        return f64::NAN;
    }
    if x <= xp[0] {
        return fp[0];
    }
    if x >= xp[xp.len() - 1] {
        return fp[fp.len() - 1];
    }
    let i = searchsorted_right(xp, x) - 1;
    let (x0, x1) = (xp[i], xp[i + 1]);
    if x1 == x0 {
        return fp[i];
    }
    fp[i] + (fp[i + 1] - fp[i]) * (x - x0) / (x1 - x0)
}

/// numpy.argsort（安定。昇順）
pub fn argsort(v: &[f64]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|&a, &b| v[a].partial_cmp(&v[b]).unwrap_or(std::cmp::Ordering::Equal));
    idx
}

pub fn argmax(v: &[f64]) -> usize {
    let mut best = 0;
    for (i, x) in v.iter().enumerate() {
        if *x > v[best] {
            best = i;
        }
    }
    best
}

pub fn argmin(v: &[f64]) -> usize {
    let mut best = 0;
    for (i, x) in v.iter().enumerate() {
        if *x < v[best] {
            best = i;
        }
    }
    best
}

pub fn max(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}

pub fn min(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::INFINITY, f64::min)
}

/// numpy.diff
pub fn diff(v: &[f64]) -> Vec<f64> {
    v.windows(2).map(|w| w[1] - w[0]).collect()
}

/// 文字列を最大 n 文字に（Python の s[:n]）
pub fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 秒 → "m:ss"
pub fn fmt_mmss(s: f64) -> String {
    let s = s.max(0.0);
    format!("{}:{:02}", (s / 60.0).floor() as i64, (s % 60.0).floor() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_matches_numpy() {
        let v = [1.0, 2.0, 3.0, 4.0];
        assert!((percentile(&v, 50.0) - 2.5).abs() < 1e-12);
        assert!((percentile(&v, 80.0) - 3.4).abs() < 1e-12);
        assert!((median(&[3.0, 1.0, 2.0]) - 2.0).abs() < 1e-12);
    }

    #[test]
    fn polyfit_line() {
        let (a, b) = polyfit1(&[0.0, 1.0, 2.0], &[1.0, 3.0, 5.0]);
        assert!((a - 2.0).abs() < 1e-12 && (b - 1.0).abs() < 1e-12);
    }

    #[test]
    fn searchsorted() {
        let v = [1.0, 2.0, 2.0, 3.0];
        assert_eq!(searchsorted_left(&v, 2.0), 1);
        assert_eq!(searchsorted_right(&v, 2.0), 3);
        assert!((interp(2.5, &[2.0, 3.0], &[10.0, 20.0]) - 15.0).abs() < 1e-12);
    }
}
