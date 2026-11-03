//! ラウドネス・小節ごとの特徴量・キー・3 帯域波形（djmix/analysis/features.py。design_phase1.md §3.3 の 7〜10）。
//!
//! Python 版（numpy / scipy / librosa / pyloudnorm）と同じ値になるように、信号処理は analysis::dsp に移植してある。
use super::dsp::butter::{butter_sos, sosfilt_absmax, BType};
use super::dsp::chroma::{chroma_filter, chroma_from_power, estimate_tuning};
use super::dsp::loudness::integrated_loudness;
use super::dsp::npy::{mean_f32, pairwise_sum_f32, pairwise_sum_f64, pairwise_sum_weighted, py_round, rfftfreq, rint};
use super::dsp::onset::onset_detect;
use super::dsp::resample::resample;
use super::dsp::stft::{stft_mag, Spec};
use super::types::{BarFeats, Extra, KeyInfo, KEY_NAMES, NCOLS, WAVE_RATE};
use crate::audio::decode::Stereo;
use rayon::prelude::*;

const CAMELOT_MAJOR: [&str; 12] = ["8B", "3B", "10B", "5B", "12B", "7B", "2B", "9B", "4B", "11B", "6B", "1B"];
const CAMELOT_MINOR: [&str; 12] = ["5A", "12A", "7A", "2A", "9A", "4A", "11A", "6A", "1A", "8A", "3A", "10A"];
const KS_MAJOR: [f64; 12] = [6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88];
const KS_MINOR: [f64; 12] = [6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17];

/// キー（曲全体・冒頭 30 秒・末尾 30 秒）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Keys {
    pub all: KeyInfo,
    pub head30: KeyInfo,
    pub tail30: KeyInfo,
}

/// (integrated LUFS, サンプルピーク dBFS)。LUFS が有限でない（無音・400ms 未満）ときは −70。
pub fn loudness(audio: &Stereo, sr: u32) -> (f64, f64) {
    let lufs = integrated_loudness(&audio.data, 2, sr as f64);
    // Python: max(1e-9, np.float32) は np.float32 のまま → log10 も 20 倍も float32
    let peak = audio.data.iter().fold(0f32, |m, &v| m.max(v.abs()));
    let peak_db = if (peak as f64) > 1e-9 { (20.0f32 * peak.log10()) as f64 } else { 20.0 * 1e-9f64.log10() };
    (if lufs.is_finite() { lufs } else { -70.0 }, peak_db)
}

/// 3 帯域（低 <200Hz / 中 200Hz〜2.5kHz / 高 >2.5kHz）のピーク。帯域ごとに n 点ずつ並べた (3, n) の uint8。
/// 3 帯域共通の最大値で正規化（Python 版 waveform と同じ: 区切りは小数の (i·sr/200) を偶数丸め）。
pub fn waveform(mono: &[f32], sr: u32) -> Vec<u8> {
    let rate = WAVE_RATE as f64;
    let n = ((mono.len() as u64 * WAVE_RATE as u64) as f64 / sr as f64) as usize;
    let edges: Vec<usize> = (0..=n).map(|i| rint((i as u64 * sr as u64) as f64 / rate) as usize).collect();
    let fs = sr as f64;
    let filters = [
        butter_sos(4, &[200.0], BType::Lowpass, fs),
        butter_sos(4, &[200.0, 2500.0], BType::Bandpass, fs),
        butter_sos(4, &[2500.0], BType::Highpass, fs),
    ];
    let bands: Vec<Vec<f64>> = filters.par_iter().map(|sos| sosfilt_absmax(sos, mono, &edges)).collect();
    if n == 0 {
        return vec![];
    }
    let mut m = bands.iter().flat_map(|b| b.iter()).cloned().fold(f64::NEG_INFINITY, f64::max);
    if m == 0.0 {
        m = 1.0;
    }
    bands.iter().flat_map(|b| b.iter().map(move |&v| (v / m * 255.0).clamp(0.0, 255.0) as u8)).collect()
}

/// np.corrcoef(x, y)[0, 1]
fn corrcoef(x: &[f64; 12], y: &[f64; 12]) -> f64 {
    let mx = pairwise_sum_f64(x) / 12.0;
    let my = pairwise_sum_f64(y) / 12.0;
    let (mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0);
    for i in 0..12 {
        let (a, b) = (x[i] - mx, y[i] - my);
        sxx += a * a;
        syy += b * b;
        sxy += a * b;
    }
    let f = 1.0 / 11.0;
    let (cxx, cyy, cxy) = (sxx * f, syy * f, sxy * f);
    (cxy / cxx.sqrt() / cyy.sqrt()).clamp(-1.0, 1.0)
}

/// Krumhansl のプロファイルとの相関で最も近いキー
pub fn estimate_key(chroma_mean: &[f64; 12]) -> KeyInfo {
    if !chroma_mean.iter().any(|&v| v > 0.0) {
        return KeyInfo { key: None, camelot: None, corr: 0.0 };
    }
    let mut best: Option<(f64, usize, bool)> = None;
    for tonic in 0..12 {
        for (major, prof) in [(true, &KS_MAJOR), (false, &KS_MINOR)] {
            let mut rolled = [0.0; 12];
            for i in 0..12 {
                rolled[i] = prof[(i + 12 - tonic) % 12];
            }
            let r = corrcoef(chroma_mean, &rolled);
            if best.is_none() || r > best.unwrap().0 {
                best = Some((r, tonic, major));
            }
        }
    }
    let (r, tonic, major) = best.unwrap();
    KeyInfo {
        key: Some(format!("{} {}", KEY_NAMES[tonic], if major { "major" } else { "minor" })),
        camelot: Some(if major { CAMELOT_MAJOR[tonic] } else { CAMELOT_MINOR[tonic] }.to_string()),
        corr: py_round(r, 3),
    }
}

/// 22050Hz の信号から、フレームごとのクロマ・低域の和・RMS（librosa の chroma_stft / stft / rms、hop=2048, n_fft=4096）
pub struct FrameFeats {
    pub chroma: Vec<[f32; 12]>,
    pub low: Vec<f32>,
    pub rms: Vec<f32>,
    /// フレームの時刻（秒）
    pub t: Vec<f64>,
    pub tuning: f64,
}

pub fn frame_features(y: &[f32]) -> FrameFeats {
    let sr = 22050.0;
    let (n_fft, hop) = (4096usize, 2048usize);
    let mag: Spec = stft_mag(y, n_fft, hop);
    let power = mag.power();
    let tuning = estimate_tuning(&power, sr);
    let fb = chroma_filter(sr, n_fft, tuning);
    let chroma = chroma_from_power(&power, &fb);
    let freqs = rfftfreq(n_fft, sr);
    let nlow = freqs.iter().filter(|&&f| f < 200.0).count();
    // spec[freqs < 200].sum(axis=0): ビンの順に float32 で逐次に足す
    let low: Vec<f32> = (0..mag.n_frames)
        .map(|t| {
            let mut acc = 0f32;
            for &v in &mag.frame(t)[..nlow] {
                acc += v;
            }
            acc
        })
        .collect();
    // librosa.feature.rms(frame_length=4096, hop_length=2048)（中央に合わせて 0 を詰める）
    let half = (n_fft / 2) as i64;
    let len = y.len() as i64;
    let rms: Vec<f32> = (0..mag.n_frames)
        .into_par_iter()
        .map(|t| {
            let start = (t * hop) as i64 - half;
            let sq: Vec<f32> = (0..n_fft as i64)
                .map(|i| {
                    let idx = start + i;
                    let v = if idx >= 0 && idx < len { y[idx as usize] } else { 0.0 };
                    v * v
                })
                .collect();
            (pairwise_sum_f32(&sq) / n_fft as f32).sqrt()
        })
        .collect();
    let t: Vec<f64> = (0..mag.n_frames).map(|i| (i * hop) as f64 / sr).collect();
    FrameFeats { chroma, low, rms, t, tuning }
}

fn chroma_mean(ff: &FrameFeats, idx: &[usize]) -> [f32; 12] {
    let mut c = [0f32; 12];
    let mut buf = Vec::with_capacity(idx.len());
    for (k, ck) in c.iter_mut().enumerate() {
        buf.clear();
        buf.extend(idx.iter().map(|&i| ff.chroma[i][k]));
        *ck = mean_f32(&buf);
    }
    c
}

fn key_of(ff: &FrameFeats, idx: &[usize]) -> KeyInfo {
    let m = if idx.is_empty() { [0f32; 12] } else { chroma_mean(ff, idx) };
    let mut v = [0f64; 12];
    for i in 0..12 {
        v[i] = m[i] as f64;
    }
    estimate_key(&v)
}

/// 小節ごとの特徴量（行 = 小節、列 = rms, low, chroma_C..B, harmonic）と、キー（全体・冒頭 30 秒・末尾 30 秒）。
///
/// 和音の濃さ（harmonic）= クロマの最大値の突出度。ドラムだけの区間では小さくなる（打合せ5）。
pub fn bar_features(mono: &[f32], sr: u32, bar_times: &[f64], duration: f64) -> (BarFeats, Keys) {
    let y = resample(mono, sr, 22050);
    let ff = frame_features(&y);
    bar_features_from(&ff, bar_times, duration)
}

/// bar_features の後半（フレームの特徴量から）
pub fn bar_features_from(ff: &FrameFeats, bar_times: &[f64], duration: f64) -> (BarFeats, Keys) {
    let t = &ff.t;
    let mut edges = bar_times.to_vec();
    edges.push(duration);
    let mut feats: BarFeats = Vec::with_capacity(bar_times.len());
    for w in edges.windows(2) {
        let (a, b) = (w[0], w[1]);
        let mut sel: Vec<usize> = (0..t.len()).filter(|&i| t[i] >= a && t[i] < b).collect();
        if sel.is_empty() && !t.is_empty() {
            // np.argmin(np.abs(t - a))
            let mut best = 0;
            for i in 0..t.len() {
                if (t[i] - a).abs() < (t[best] - a).abs() {
                    best = i;
                }
            }
            sel = vec![best];
        }
        let c = chroma_mean(ff, &sel);
        let cmax = c.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let harm = (cmax - mean_f32(&c)) / (cmax + 1e-9f32);
        let rms: Vec<f32> = sel.iter().map(|&i| ff.rms[i]).collect();
        let low: Vec<f32> = sel.iter().map(|&i| ff.low[i]).collect();
        let mut row = [0f32; NCOLS];
        row[0] = mean_f32(&rms);
        row[1] = mean_f32(&low);
        row[2..14].copy_from_slice(&c);
        row[14] = harm;
        feats.push(row);
    }
    // 曲内で相対化（曲どうしの比較はラウドネス正規化後に Phase 3 で行う）
    for col in 0..2 {
        let mx = feats.iter().map(|r| r[col]).fold(f32::NEG_INFINITY, f32::max);
        if mx > 0.0 {
            for r in feats.iter_mut() {
                r[col] /= mx;
            }
        }
    }
    let all: Vec<usize> = (0..t.len()).collect();
    let head: Vec<usize> = (0..t.len()).filter(|&i| t[i] < 30.0).collect();
    let tail: Vec<usize> = (0..t.len()).filter(|&i| t[i] > duration - 30.0).collect();
    let keys = Keys { all: key_of(ff, &all), head30: key_of(ff, &head), tail30: key_of(ff, &tail) };
    (feats, keys)
}

/// 曲どうしを比べるための曲全体の特徴量（Phase 3。design_phase3.md §3.1）。
/// centroid: スペクトルの明るさ(Hz)、low_ratio: 150Hz 未満の割合、onset_rate: 1 秒あたりのオンセット数。
pub fn track_extra(mono: &[f32], sr: u32) -> Extra {
    let y = resample(mono, sr, 11025);
    track_extra_11025(&y)
}

/// track_extra の本体（11025Hz の信号から）
pub fn track_extra_11025(y: &[f32]) -> Extra {
    let sr = 11025.0;
    if y.is_empty() {
        return Extra { centroid: 0.0, low_ratio: 0.0, onset_rate: 0.0 };
    }
    let s = stft_mag(y, 2048, 512);
    let f = rfftfreq(2048, sr);
    // S は (ビン, フレーム) の F 順の配列なので、メモリ順（フレームごと）の pairwise 和
    let tot = pairwise_sum_f32(&s.data) as f64 + 1e-9;
    let num = pairwise_sum_weighted(&s.data, &f);
    // S[f < 150] は (ビン, フレーム) の C 順になる
    let nlow = f.iter().filter(|&&x| x < 150.0).count();
    let mut lowv = Vec::with_capacity(nlow * s.n_frames);
    for k in 0..nlow {
        for t in 0..s.n_frames {
            lowv.push(s.data[t * s.n_bins + k]);
        }
    }
    let low = pairwise_sum_f32(&lowv);
    let onsets = onset_detect(&s.power(), sr, 512);
    Extra {
        centroid: py_round(num / tot, 1),
        // np.float32 / Python の float は float32 で割る（NEP 50）
        low_ratio: py_round((low / tot as f32) as f64, 4),
        onset_rate: py_round(onsets.len() as f64 / 1e-3f64.max(y.len() as f64 / sr), 3),
    }
}
