//! クロマ（librosa.feature.chroma_stft の移植）: チューニングの推定（estimate_tuning / piptrack / pitch_tuning）と
//! クロマのフィルタバンク（filters.chroma）。
//!
//! estimate_tuning はヒストグラムの最大のビン（0.01 半音刻み）を選ぶので、float32 の演算順まで librosa（numba の部分を含む）に合わせる。
use super::npy::{fmod1_f32, linspace, median_f32, rfftfreq};
use super::stft::Spec;
use rayon::prelude::*;

/// 1 フレーム分の piptrack（S は power スペクトル）。検出した (pitch, mag) を out に足す
fn piptrack_frame(
    s: &[f32],
    fft_freqs: &[f64],
    fmin: f64,
    fmax: f64,
    sr: f64,
    n_fft: usize,
    out: &mut Vec<(f32, f32)>,
) {
    let n = s.len();
    if n < 3 {
        return;
    }
    // ref_value = 0.1 * max(S)（float32）
    let mx = s.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let ref_value = 0.1f32 * mx;
    let sm = |k: usize| if s[k] > ref_value { s[k] } else { 0.0f32 };
    for k in 1..n {
        if !(fmin <= fft_freqs[k] && fft_freqs[k] < fmax) {
            continue;
        }
        // util.localmax（最後の要素は x[-1] > x[-2] のみ）
        let is_max = if k == n - 1 { sm(k) > sm(k - 1) } else { sm(k) > sm(k - 1) && sm(k) >= sm(k + 1) };
        if !is_max {
            continue;
        }
        // np.gradient（float32）と _parabolic_interpolation（numba: 一部 float64）
        let (avg, shift) = if k == n - 1 {
            (s[k] - s[k - 1], 0.0f32)
        } else {
            let avg = (s[k + 1] - s[k - 1]) / 2.0f32;
            let a = (s[k + 1] + s[k - 1]) as f64 - 2.0 * s[k] as f64;
            let b = (s[k + 1] - s[k - 1]) as f64 / 2.0;
            let shift = if b.abs() >= a.abs() { 0.0f32 } else { (-b / a) as f32 };
            (avg, shift)
        };
        let dskew = 0.5f32 * avg * shift;
        let pitch = ((k as f64 + shift as f64) * sr / n_fft as f64) as f32;
        let mag = s[k] + dskew;
        out.push((pitch, mag));
    }
}

/// librosa.estimate_tuning(S=power, sr=sr, bins_per_octave=12)（piptrack の fmin=150, fmax=4000, threshold=0.1）
pub fn estimate_tuning(power: &Spec, sr: f64) -> f64 {
    let n_fft = 2 * (power.n_bins - 1);
    let fft_freqs = rfftfreq(n_fft, sr);
    let fmin = 150.0f64.max(0.0);
    let fmax = 4000.0f64.min(sr / 2.0);
    let found: Vec<(f32, f32)> = (0..power.n_frames)
        .into_par_iter()
        .fold(Vec::new, |mut acc, t| {
            piptrack_frame(power.frame(t), &fft_freqs, fmin, fmax, sr, n_fft, &mut acc);
            acc
        })
        .reduce(Vec::new, |mut a, b| {
            a.extend(b);
            a
        });
    let mags: Vec<f32> = found.iter().filter(|(p, _)| *p > 0.0).map(|&(_, m)| m).collect();
    let threshold = if mags.is_empty() { 0.0f32 } else { median_f32(&mags) };
    let freqs: Vec<f32> = found.iter().filter(|&&(p, m)| m >= threshold && p > 0.0).map(|&(p, _)| p).collect();
    pitch_tuning(&freqs, 0.01, 12)
}

/// librosa.pitch_tuning（float32 の周波数から 0.01 半音刻みのヒストグラムの最頻値）
pub fn pitch_tuning(freqs: &[f32], resolution: f64, bins_per_octave: u32) -> f64 {
    let f: Vec<f32> = freqs.iter().copied().filter(|&x| x > 0.0).collect();
    if f.is_empty() {
        return 0.0;
    }
    let a440_16 = (440.0f64 / 16.0) as f32;
    let bpo = bins_per_octave as f32;
    let nbins = (1.0 / resolution).ceil() as usize;
    let edges = linspace(-0.5, 0.5, nbins + 1, true);
    let mut counts = vec![0usize; nbins];
    for &x in &f {
        let mut r = fmod1_f32(bpo * (x / a440_16).log2());
        if r >= 0.5 {
            r -= 1.0;
        }
        let r = r as f64;
        if r < edges[0] || r > edges[nbins] {
            continue;
        }
        // edges[i] <= r < edges[i+1]（最後のビンは右端を含む）
        let i = edges.partition_point(|&e| e <= r);
        let bin = if i > nbins { nbins - 1 } else { i - 1 };
        counts[bin.min(nbins - 1)] += 1;
    }
    let mut best = 0;
    for (i, &c) in counts.iter().enumerate() {
        if c > counts[best] {
            best = i;
        }
    }
    edges[best]
}

/// librosa.filters.chroma(sr=sr, n_fft=n_fft, tuning=tuning)（n_chroma=12, ctroct=5, octwidth=2, norm=2, base_c=True）。
/// 戻り値は [12][n_fft/2+1] の float32
pub fn chroma_filter(sr: f64, n_fft: usize, tuning: f64) -> Vec<Vec<f32>> {
    let n_chroma = 12usize;
    let nc = n_chroma as f64;
    let freqs = linspace(0.0, sr, n_fft, false);
    let a440 = 440.0 * 2.0f64.powf(tuning / nc);
    let mut frqbins: Vec<f64> = Vec::with_capacity(n_fft);
    let oct: Vec<f64> = freqs[1..].iter().map(|f| nc * (f / (a440 / 16.0)).log2()).collect();
    frqbins.push(oct[0] - 1.5 * nc);
    frqbins.extend(oct);
    let mut binwidth: Vec<f64> = frqbins.windows(2).map(|w| (w[1] - w[0]).max(1.0)).collect();
    binwidth.push(1.0);
    let n_chroma2 = (nc / 2.0).round_ties_even();
    // wts[c][i]
    let mut wts = vec![vec![0.0f64; n_fft]; n_chroma];
    for c in 0..n_chroma {
        for i in 0..n_fft {
            let d = frqbins[i] - c as f64;
            let a = d + n_chroma2 + 10.0 * nc;
            let d = (a % nc) - n_chroma2; // np.remainder（a > 0）
            let q = 2.0 * d / binwidth[i];
            wts[c][i] = (-0.5 * (q * q)).exp();
        }
    }
    // normalize(norm=2, axis=0)
    for i in 0..n_fft {
        let mut s = 0.0;
        for c in 0..n_chroma {
            s += wts[c][i] * wts[c][i];
        }
        let mut len = s.sqrt();
        if len < f64::MIN_POSITIVE {
            len = 1.0;
        }
        for c in 0..n_chroma {
            wts[c][i] /= len;
        }
    }
    // オクターブ方向のガウス窓
    for i in 0..n_fft {
        let q = (frqbins[i] / nc - 5.0) / 2.0;
        let g = (-0.5 * (q * q)).exp();
        for c in 0..n_chroma {
            wts[c][i] *= g;
        }
    }
    // base_c: roll(-3)
    let nb = n_fft / 2 + 1;
    (0..n_chroma).map(|c| wts[(c + 3) % n_chroma][..nb].iter().map(|&v| v as f32).collect()).collect()
}

/// float32 の内積を float64 で（8 本の累積。librosa は BLAS の sgemm なので和の順序はもともと合わせられない）
pub fn dot(a: &[f32], b: &[f32]) -> f64 {
    let mut acc = [0.0f64; 8];
    let n = a.len().min(b.len());
    let (ca, cb) = (&a[..n - n % 8], &b[..n - n % 8]);
    for (x, y) in ca.chunks_exact(8).zip(cb.chunks_exact(8)) {
        for k in 0..8 {
            acc[k] += x[k] as f64 * y[k] as f64;
        }
    }
    let mut s = acc.iter().sum::<f64>();
    for i in n - n % 8..n {
        s += a[i] as f64 * b[i] as f64;
    }
    s
}

/// chroma_stft の残り: einsum（フィルタ × power）と normalize(norm=inf, axis=chroma)。戻り値は [フレーム][12]
pub fn chroma_from_power(power: &Spec, fb: &[Vec<f32>]) -> Vec<[f32; 12]> {
    (0..power.n_frames)
        .into_par_iter()
        .map(|t| {
            let s = power.frame(t);
            let mut raw = [0f32; 12];
            for (c, w) in fb.iter().enumerate() {
                raw[c] = dot(w, s) as f32;
            }
            let mut mx = 0.0f64;
            for &v in &raw {
                mx = mx.max((v as f64).abs());
            }
            if mx < super::npy::TINY_F32 as f64 {
                mx = 1.0;
            }
            let mut out = [0f32; 12];
            for c in 0..12 {
                out[c] = (raw[c] as f64 / mx) as f32;
            }
            out
        })
        .collect()
}
