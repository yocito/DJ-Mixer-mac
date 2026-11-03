//! メルスペクトログラム（librosa.filters.mel の Slaney 型・norm="slaney"、power_to_db）。
//! フィルタは三角形なので 0 でない範囲だけ持つ（掛け算の結果は密な行列の積と同じ）。
use super::npy::{linspace, rfftfreq};
use super::stft::Spec;
use rayon::prelude::*;

const F_SP: f64 = 200.0 / 3.0;
const MIN_LOG_HZ: f64 = 1000.0;

fn min_log_mel() -> f64 {
    (MIN_LOG_HZ - 0.0) / F_SP
}

fn logstep() -> f64 {
    6.4f64.ln() / 27.0
}

/// librosa.hz_to_mel（htk=False）
pub fn hz_to_mel(f: f64) -> f64 {
    let mut m = (f - 0.0) / F_SP;
    if f >= MIN_LOG_HZ {
        m = min_log_mel() + (f / MIN_LOG_HZ).ln() / logstep();
    }
    m
}

/// librosa.mel_to_hz（htk=False）
pub fn mel_to_hz(m: f64) -> f64 {
    let mut f = 0.0 + F_SP * m;
    if m >= min_log_mel() {
        f = MIN_LOG_HZ * (logstep() * (m - min_log_mel())).exp();
    }
    f
}

/// メルフィルタ 1 本（0 でない範囲の開始ビンと重み）
#[derive(Clone, Debug)]
pub struct MelBand {
    pub start: usize,
    pub w: Vec<f32>,
}

/// librosa.filters.mel(sr=sr, n_fft=n_fft, n_mels=n_mels, fmin=fmin, fmax=fmax)
pub fn mel_filter(sr: f64, n_fft: usize, n_mels: usize, fmin: f64, fmax: f64) -> Vec<MelBand> {
    let fftfreqs = rfftfreq(n_fft, sr);
    let mels = linspace(hz_to_mel(fmin), hz_to_mel(fmax), n_mels + 2, true);
    let mel_f: Vec<f64> = mels.iter().map(|&m| mel_to_hz(m)).collect();
    let fdiff: Vec<f64> = mel_f.windows(2).map(|w| w[1] - w[0]).collect();
    (0..n_mels)
        .map(|i| {
            let enorm = 2.0 / (mel_f[i + 2] - mel_f[i]);
            let row: Vec<f32> = fftfreqs
                .iter()
                .map(|&f| {
                    let lower = -(mel_f[i] - f) / fdiff[i];
                    let upper = (mel_f[i + 2] - f) / fdiff[i + 1];
                    let w = lower.min(upper).max(0.0) as f32; // float32 の配列に入る
                    (w as f64 * enorm) as f32 // weights *= enorm（float64 で掛けて float32 に）
                })
                .collect();
            let start = row.iter().position(|&v| v != 0.0).unwrap_or(row.len());
            let end = row.iter().rposition(|&v| v != 0.0).map(|e| e + 1).unwrap_or(start);
            MelBand { start, w: row[start..end.max(start)].to_vec() }
        })
        .collect()
}

/// メルスペクトログラム（power スペクトル × フィルタ）。戻り値は [フレーム][n_mels] の float32
pub fn melspec(power: &Spec, bank: &[MelBand]) -> Vec<Vec<f32>> {
    (0..power.n_frames)
        .into_par_iter()
        .map(|t| {
            let s = power.frame(t);
            bank.iter()
                .map(|b| {
                    let mut acc = 0.0f64;
                    for (k, &w) in b.w.iter().enumerate() {
                        acc += w as f64 * s[b.start + k] as f64;
                    }
                    acc as f32
                })
                .collect()
        })
        .collect()
}

/// librosa.power_to_db(S)（ref=1.0, amin=1e-10, top_db=80。float32）
pub fn power_to_db(s: &mut [Vec<f32>]) {
    let amin = 1e-10f32;
    let mut mx = f32::NEG_INFINITY;
    for row in s.iter_mut() {
        for v in row.iter_mut() {
            *v = 10.0f32 * v.max(amin).log10();
            mx = mx.max(*v);
        }
    }
    let floor = mx - 80.0f32;
    for row in s.iter_mut() {
        for v in row.iter_mut() {
            *v = v.max(floor);
        }
    }
}
