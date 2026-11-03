//! numpy / Python と同じ結果になる小道具（util.rs にない、ビット単位で合わせたいもの）。
//!
//! - pairwise_sum_f32/f64: numpy の add.reduce（1 次元・連続）の和の順序（初期値 0、8 本の累積、128 以下で分割終了）
//! - median: numpy.median（偶数個なら中央 2 つの平均 (a+b)/2）
//! - rint: numpy.round（偶数丸め）
//! - py_round: Python の round(x, n)（10 進で正しく丸める。0.125 → 0.12）

macro_rules! pairwise {
    ($name:ident, $t:ty) => {
        /// numpy の pairwise 和（numpy/_core/src/umath/loops_utils.h.src の pairwise_sum と同じ順序）
        pub fn $name(a: &[$t]) -> $t {
            let n = a.len();
            if n < 8 {
                let mut res: $t = 0.0;
                for &v in a {
                    res += v;
                }
                res
            } else if n <= 128 {
                let mut r: [$t; 8] = [a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]];
                let full = n - (n % 8);
                for c in a[8..full].chunks_exact(8) {
                    for j in 0..8 {
                        r[j] += c[j];
                    }
                }
                let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
                for &v in &a[full..] {
                    res += v;
                }
                res
            } else {
                let mut n2 = n / 2;
                n2 -= n2 % 8;
                $name(&a[..n2]) + $name(&a[n2..])
            }
        }
    };
}

pairwise!(pairwise_sum_f32, f32);
pairwise!(pairwise_sum_f64, f64);

/// pairwise_sum_f64 と同じ順序で Σ a[i]·w[i mod w.len()] を足す（(S * f[:, None]).sum() を大きな配列なしで）
pub fn pairwise_sum_weighted(a: &[f32], w: &[f64]) -> f64 {
    fn rec(a: &[f32], lo: usize, w: &[f64]) -> f64 {
        let n = a.len();
        if n <= 128 {
            let mut tmp = [0.0f64; 128];
            let mut k = lo % w.len();
            for (t, &v) in tmp.iter_mut().zip(a) {
                *t = v as f64 * w[k];
                k += 1;
                if k == w.len() {
                    k = 0;
                }
            }
            pairwise_sum_f64(&tmp[..n])
        } else {
            let mut n2 = n / 2;
            n2 -= n2 % 8;
            if n > 1 << 16 {
                let (x, y) = rayon::join(|| rec(&a[..n2], lo, w), || rec(&a[n2..], lo + n2, w));
                x + y
            } else {
                rec(&a[..n2], lo, w) + rec(&a[n2..], lo + n2, w)
            }
        }
    }
    if w.is_empty() {
        return 0.0;
    }
    rec(a, 0, w)
}

/// numpy の float32 配列の mean（pairwise 和 ÷ 個数、どちらも float32）。空なら NaN
pub fn mean_f32(a: &[f32]) -> f32 {
    if a.is_empty() {
        return f32::NAN;
    }
    pairwise_sum_f32(a) / a.len() as f32
}

/// numpy の float64 配列の mean
pub fn mean_f64(a: &[f64]) -> f64 {
    if a.is_empty() {
        return f64::NAN;
    }
    pairwise_sum_f64(a) / a.len() as f64
}

/// numpy.median（NaN を含めば NaN、空なら NaN）
pub fn median_f64(v: &[f64]) -> f64 {
    if v.is_empty() || v.iter().any(|x| x.is_nan()) {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// numpy.median（float32。偶数個なら float32 で (a+b)/2）
pub fn median_f32(v: &[f32]) -> f32 {
    if v.is_empty() || v.iter().any(|x| x.is_nan()) {
        return f32::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// numpy.nanmedian
pub fn nanmedian_f64(v: &[f64]) -> f64 {
    let f: Vec<f64> = v.iter().copied().filter(|x| !x.is_nan()).collect();
    median_f64(&f)
}

/// numpy.round / np.rint（ちょうど半分は偶数へ）
#[inline]
pub fn rint(x: f64) -> f64 {
    x.round_ties_even()
}

/// Python の round(x, nd)（10 進で正しく丸める。ちょうど半分は偶数へ）。
/// util::round は (x·10ⁿ).round() / 10ⁿ なので、0.125 → 0.13 のように Python と違うことがある
pub fn py_round(x: f64, nd: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", nd, x).parse().unwrap_or(x)
}

/// numpy.linspace(start, stop, num, endpoint) と同じ値（i·step + start、endpoint なら最後は stop）
pub fn linspace(start: f64, stop: f64, num: usize, endpoint: bool) -> Vec<f64> {
    let div = if endpoint { num.saturating_sub(1) } else { num };
    let delta = stop - start;
    let mut v: Vec<f64> = if div > 0 {
        let step = delta / div as f64;
        if step == 0.0 {
            (0..num).map(|i| i as f64 / div as f64 * delta + start).collect()
        } else {
            (0..num).map(|i| i as f64 * step + start).collect()
        }
    } else {
        (0..num).map(|i| i as f64 * delta + start).collect()
    };
    if endpoint && num > 1 {
        v[num - 1] = stop;
    }
    v
}

/// numpy.fft.rfftfreq(n, 1/sr)（i · (1/(n·(1/sr)))）
pub fn rfftfreq(n: usize, sr: f64) -> Vec<f64> {
    let val = 1.0 / (n as f64 * (1.0 / sr));
    (0..n / 2 + 1).map(|i| i as f64 * val).collect()
}

/// np.mod(x, 1.0)（float32。結果は [0, 1)）
#[inline]
pub fn fmod1_f32(x: f32) -> f32 {
    let m = x % 1.0f32;
    if m != 0.0 && m < 0.0 {
        m + 1.0
    } else if m == 0.0 {
        0.0
    } else {
        m
    }
}

/// float32 の tiny（np.finfo(np.float32).tiny）
pub const TINY_F32: f32 = f32::MIN_POSITIVE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding() {
        assert_eq!(py_round(0.125, 2), 0.12);
        assert_eq!(py_round(2.675, 2), 2.67);
        assert_eq!(py_round(0.0005, 3), 0.001);
        assert_eq!(rint(0.5), 0.0);
        assert_eq!(rint(1.5), 2.0);
        assert_eq!(rint(220.5), 220.0);
    }

    #[test]
    fn medians() {
        assert_eq!(median_f64(&[3.0, 1.0, 2.0, 4.0]), 2.5);
        assert_eq!(median_f32(&[3.0, 1.0, 2.0]), 2.0);
        assert!(median_f64(&[]).is_nan());
    }

    #[test]
    fn linspace_matches() {
        let v = linspace(-0.5, 0.5, 101, true);
        assert_eq!(v[0], -0.5);
        assert_eq!(v[100], 0.5);
        assert_eq!(v[50], 50.0 * 0.01 - 0.5);
    }
}
