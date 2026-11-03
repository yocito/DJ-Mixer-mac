//! Beat This!（拍・小節頭の検出。djmix/analysis/beats.py の `detect` と beat_this パッケージの推論）。
//!
//! Python 版と同じ手順を Rust で行い、網の部分だけを ONNX（tools/export_onnx.py が書き出す beat_this_final0.onnx）で動かす:
//! 1. 22050Hz に変換（beat_this は soxr の既定の HQ。同じ特性の窓付き sinc フィルタ。44.1kHz からなら誤差 -110dB 程度）
//! 2. LogMelSpect（torchaudio の MelSpectrogram: n_fft 1024, hop 441, 30〜11000Hz, 128 帯, slaney, 正規化 frame_length,
//!    振幅（power 1）→ log1p(1000x)）
//! 3. split_predict_aggregate（1500 フレームの塊、境界 6 フレーム、重なりは前の塊を優先）
//! 4. Postprocessor(type="minimal")（±3 フレームの極大で logit > 0、隣り合う山をまとめ、小節頭を一番近い拍へ寄せる）
use crate::config::model_file;
use crate::ml;
use anyhow::{bail, Result};
use once_cell::sync::OnceCell;
use realfft::RealFftPlanner;
use std::sync::Arc;

pub const MODEL_FILE: &str = "beat_this_final0.onnx";
/// 拍の確率（logits）の 1 秒あたりのフレーム数
pub const FPS: f64 = 50.0;
pub const SAMPLE_RATE: u32 = 22050;
const N_FFT: usize = 1024;
const HOP: usize = 441;
pub const N_MELS: usize = 128;
const F_MIN: f64 = 30.0;
const F_MAX: f64 = 11000.0;
const LOG_MULT: f32 = 1000.0;
const CHUNK: usize = 1500;
const BORDER: usize = 6;

#[derive(Debug, Clone, Default)]
pub struct Detection {
    /// 拍の時刻（秒）
    pub beats: Vec<f64>,
    /// 小節頭の時刻（秒）
    pub downbeats: Vec<f64>,
    /// 拍の確率（50fps。sigmoid(logits)）
    pub beat_prob: Vec<f32>,
    /// 小節頭の確率（50fps）
    pub down_prob: Vec<f32>,
}

static MODEL: OnceCell<std::result::Result<Arc<ml::Model>, String>> = OnceCell::new();

fn model() -> Result<Arc<ml::Model>> {
    MODEL
        .get_or_init(|| {
            let path =
                model_file(MODEL_FILE).ok_or_else(|| format!("{MODEL_FILE} が見つかりません（tools/export_onnx.py で作る）"))?;
            let _ = ml::read_meta(&path);
            ml::Model::load(&path).map(Arc::new).map_err(|e| format!("{e:#}"))
        })
        .clone()
        .map_err(|e| anyhow::anyhow!(e))
}

/// 拍の検出を使えるか（ML 入りのビルドで、モデルのファイルがあり、ONNX Runtime を読める）。
pub fn available() -> bool {
    ml::compiled() && model_file(MODEL_FILE).is_some() && ml::ready()
}

/// Beat This! を実行する。mono: モノラルの音（任意のサンプリング周波数 sr）。
pub fn detect(mono: &[f32], sr: u32) -> Result<Detection> {
    let y = resample_to_22050(mono, sr);
    if y.len() <= N_FFT / 2 {
        bail!("音が短すぎます（{} サンプル）", mono.len());
    }
    let spect = log_mel_spect(&y);
    let (bl, dl) = predict(&spect)?;
    let (beats, downbeats) = postprocess(&bl, &dl);
    Ok(Detection {
        beats,
        downbeats,
        beat_prob: bl.iter().map(|&x| sigmoid(x)).collect(),
        down_prob: dl.iter().map(|&x| sigmoid(x)).collect(),
    })
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

// ---------------- 22050Hz への変換（soxr の HQ と同じ特性） ----------------
//
// soxr（HQ）の 2:1 の特性を測ると、10.2kHz まで平坦、10.55kHz で -6dB、11kHz で -100dB、ナイキスト以上は -150dB 以下の直線位相。
// Kaiser 窓の sinc（片側 196 タップ、β = 13.24、遮断 10549.5Hz）がこれにほぼ一致する（白色雑音で soxr との差の RMS が -105dB）。
// 範囲外は 0 とみなし、出力の 0 番目が入力の 0 番目に揃う（soxr.resample と同じ。長さは ceil(n × 22050 / sr)）。
const RS_FC: f64 = 10549.512 / 22050.0; // 出力のサンプリング周波数に対する遮断周波数（cycles/sample）
const RS_HALF: f64 = 98.0; // 片側の長さ（出力のサンプル数）
const RS_BETA: f64 = 13.2398;

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..200 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        let p = std::f64::consts::PI * x;
        p.sin() / p
    }
}

/// 窓付き sinc の値。tau: 入力のサンプル単位の時刻差、ratio: 入力 / 出力のサンプリング周波数の比
fn rs_kernel(tau: f64, ratio: f64) -> f64 {
    let half = RS_HALF * ratio;
    if tau.abs() > half {
        return 0.0;
    }
    let fc = RS_FC / ratio;
    let r = tau / half;
    2.0 * fc * sinc(2.0 * fc * tau) * bessel_i0(RS_BETA * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(RS_BETA)
}

/// モノラルの音を 22050Hz にする（beat_this の `soxr.resample(signal, in_rate=sr, out_rate=22050)` の代わり）。
pub fn resample_to_22050(x: &[f32], sr: u32) -> Vec<f32> {
    if sr == SAMPLE_RATE {
        return x.to_vec();
    }
    let out_len = (x.len() as u64 * SAMPLE_RATE as u64).div_ceil(sr as u64) as usize;
    if sr.is_multiple_of(SAMPLE_RATE) {
        return resample_integer(x, (sr / SAMPLE_RATE) as usize, out_len);
    }
    resample_generic(x, sr as f64 / SAMPLE_RATE as f64, out_len)
}

/// 整数分の 1（44.1kHz → 22050Hz など）。FFT の畳み込み（overlap-save）で全部の点を計算してから間引く。
fn resample_integer(x: &[f32], m: usize, out_len: usize) -> Vec<f32> {
    let h_half = (RS_HALF * m as f64).round() as usize;
    let k = 2 * h_half + 1;
    let mut h: Vec<f64> = (0..k).map(|j| rs_kernel(j as f64 - h_half as f64, m as f64)).collect();
    let s: f64 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= s);
    let n_fft = (4 * k).next_power_of_two().max(8192);
    let step = n_fft - (k - 1);
    let mut planner = RealFftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(n_fft);
    let inv = planner.plan_fft_inverse(n_fft);
    let mut hbuf = vec![0f64; n_fft];
    hbuf[..k].copy_from_slice(&h);
    let mut hspec = fwd.make_output_vec();
    fwd.process(&mut hbuf, &mut hspec).unwrap();
    let mut buf = vec![0f64; n_fft];
    let mut spec = fwd.make_output_vec();
    let mut res = vec![0f64; n_fft];
    let mut out = vec![0f32; out_len];
    let full = x.len(); // 全部の点の出力 z[i] = Σ_j h[j] x[i + h_half - j]（i = 0..n）
    let mut s0 = 0usize;
    while s0 < full {
        // c[m] = Σ_j h[j] x[m - j] の m = s0 + h_half .. + step を作る。入力は x[m0 - (k-1) ..]
        let m0 = (s0 + h_half) as i64;
        let base = m0 - (k as i64 - 1);
        for (i, b) in buf.iter_mut().enumerate() {
            let idx = base + i as i64;
            *b = if idx >= 0 && (idx as usize) < x.len() { x[idx as usize] as f64 } else { 0.0 };
        }
        fwd.process(&mut buf, &mut spec).unwrap();
        for (a, b) in spec.iter_mut().zip(hspec.iter()) {
            *a *= *b;
        }
        inv.process(&mut spec, &mut res).unwrap();
        let scale = 1.0 / n_fft as f64;
        for t in 0..step {
            let i = s0 + t;
            if i >= full {
                break;
            }
            if i.is_multiple_of(m) && i / m < out_len {
                out[i / m] = (res[k - 1 + t] * scale) as f32;
            }
        }
        s0 += step;
    }
    out
}

/// 任意の比（48kHz → 22050Hz など）。窓付き sinc を細かい表にして直接計算する（遅いが、使うのはまれ）。
fn resample_generic(x: &[f32], ratio: f64, out_len: usize) -> Vec<f32> {
    const RES: usize = 512; // 入力の 1 サンプルあたりの表の点数
    let half = RS_HALF * ratio;
    let n_tab = (half * RES as f64).ceil() as usize + 2;
    let tab: Vec<f64> = (0..n_tab).map(|i| rs_kernel(i as f64 / RES as f64, ratio)).collect();
    let g = |tau: f64| -> f64 {
        let p = tau.abs() * RES as f64;
        let i = p as usize;
        if i + 1 >= n_tab {
            return 0.0;
        }
        let f = p - i as f64;
        tab[i] * (1.0 - f) + tab[i + 1] * f
    };
    let mut out = vec![0f32; out_len];
    for (k, o) in out.iter_mut().enumerate() {
        let t = k as f64 * ratio;
        let j0 = (t - half).ceil() as i64;
        let j1 = (t + half).floor() as i64;
        let (mut acc, mut wsum) = (0f64, 0f64);
        for j in j0..=j1 {
            let w = g(t - j as f64);
            wsum += w;
            if j >= 0 && (j as usize) < x.len() {
                acc += w * x[j as usize] as f64;
            }
        }
        *o = if wsum != 0.0 { (acc / wsum) as f32 } else { 0.0 };
    }
    out
}

// ---------------- LogMelSpect ----------------

fn hz_to_mel_slaney(f: f64) -> f64 {
    let f_sp = 200.0 / 3.0;
    let min_log_hz = 1000.0;
    let min_log_mel = min_log_hz / f_sp;
    let logstep = (6.4f64).ln() / 27.0;
    if f >= min_log_hz {
        min_log_mel + (f / min_log_hz).ln() / logstep
    } else {
        f / f_sp
    }
}

fn mel_to_hz_slaney(m: f64) -> f64 {
    let f_sp = 200.0 / 3.0;
    let min_log_hz = 1000.0;
    let min_log_mel = min_log_hz / f_sp;
    let logstep = (6.4f64).ln() / 27.0;
    if m >= min_log_mel {
        min_log_hz * (logstep * (m - min_log_mel)).exp()
    } else {
        f_sp * m
    }
}

/// torchaudio.functional.melscale_fbanks(513, 30, 11000, 128, 22050, norm=None, mel_scale="slaney") の
/// 帯ごとの (最初の周波数ビン, 重み)。
fn mel_filters() -> Vec<(usize, Vec<f32>)> {
    let n_freqs = N_FFT / 2 + 1;
    let all_freqs: Vec<f64> = (0..n_freqs).map(|i| (SAMPLE_RATE / 2) as f64 * i as f64 / (n_freqs - 1) as f64).collect();
    let (m_min, m_max) = (hz_to_mel_slaney(F_MIN), hz_to_mel_slaney(F_MAX));
    let f_pts: Vec<f64> =
        (0..N_MELS + 2).map(|i| mel_to_hz_slaney(m_min + (m_max - m_min) * i as f64 / (N_MELS + 1) as f64)).collect();
    (0..N_MELS)
        .map(|m| {
            let w: Vec<f32> = all_freqs
                .iter()
                .map(|&f| {
                    let down = (f - f_pts[m]) / (f_pts[m + 1] - f_pts[m]);
                    let up = (f_pts[m + 2] - f) / (f_pts[m + 2] - f_pts[m + 1]);
                    down.min(up).max(0.0) as f32
                })
                .collect();
            let lo = w.iter().position(|&v| v > 0.0).unwrap_or(0);
            let hi = w.iter().rposition(|&v| v > 0.0).map(|i| i + 1).unwrap_or(lo);
            (lo, w[lo..hi].to_vec())
        })
        .collect()
}

/// LogMelSpect（22050Hz のモノラル → (フレーム数 × 128) 行優先）。フレーム数は 1 + len / 441。
pub fn log_mel_spect(y: &[f32]) -> Vec<f32> {
    let pad = N_FFT / 2;
    assert!(y.len() > pad, "too short for reflect padding");
    let n_frames = 1 + y.len() / HOP;
    // center=True, pad_mode="reflect"
    let at = |i: i64| -> f32 {
        let n = y.len() as i64;
        let mut j = i;
        if j < 0 {
            j = -j;
        }
        if j >= n {
            j = 2 * (n - 1) - j;
        }
        y[j as usize]
    };
    // 周期的なハン窓（torch.hann_window）。normalized="frame_length" は torch.stft(normalized=True) → 1/sqrt(n_fft)
    // （"window" なら窓の 2 乗和の平方根で割るほう。torchaudio.functional.functional._get_spec_norms）
    let win: Vec<f64> = (0..N_FFT).map(|n| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / N_FFT as f64).cos()).collect();
    let norm = 1.0 / (N_FFT as f64).sqrt();
    let filters = mel_filters();
    let mut planner = RealFftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(N_FFT);
    let mut buf = vec![0f64; N_FFT];
    let mut spec = fft.make_output_vec();
    let mut mag = vec![0f32; N_FFT / 2 + 1];
    let mut out = vec![0f32; n_frames * N_MELS];
    for t in 0..n_frames {
        let start = (t * HOP) as i64 - pad as i64;
        for (n, b) in buf.iter_mut().enumerate() {
            *b = at(start + n as i64) as f64 * win[n];
        }
        fft.process(&mut buf, &mut spec).unwrap();
        for (m, c) in mag.iter_mut().zip(spec.iter()) {
            *m = (c.norm() * norm) as f32;
        }
        let row = &mut out[t * N_MELS..(t + 1) * N_MELS];
        for (m, (lo, w)) in filters.iter().enumerate() {
            let mut acc = 0f32;
            for (k, wv) in w.iter().enumerate() {
                acc += mag[lo + k] * wv;
            }
            row[m] = (LOG_MULT * acc).ln_1p();
        }
    }
    out
}

// ---------------- split_predict_aggregate ----------------

/// beat_this.inference.split_piece の塊の開始位置（border の分だけ前から。負もある）。
pub fn chunk_starts(len: usize) -> Vec<i64> {
    let (len, chunk, border) = (len as i64, CHUNK as i64, BORDER as i64);
    let step = chunk - 2 * border;
    let mut starts: Vec<i64> = (0..).map(|i| -border + i * step).take_while(|&s| s < len - border).collect();
    if starts.is_empty() {
        starts.push(-border);
    }
    if len > step {
        *starts.last_mut().unwrap() = len - (chunk - border);
    }
    starts
}

/// スペクトログラム（T × 128）→ 拍・小節頭の logits（T）。
pub fn predict(spect: &[f32]) -> Result<(Vec<f32>, Vec<f32>)> {
    let model = model()?;
    predict_with(&model, spect)
}

pub fn predict_with(model: &ml::Model, spect: &[f32]) -> Result<(Vec<f32>, Vec<f32>)> {
    let len = spect.len() / N_MELS;
    let starts = chunk_starts(len);
    let mut beat = vec![-1000f32; len];
    let mut down = vec![-1000f32; len];
    // keep_first: 後ろの塊から書き、前の塊で上書きする
    let mut buf: Vec<f32> = Vec::with_capacity(CHUNK * N_MELS);
    for &s in starts.iter().rev() {
        let a = s.max(0) as usize;
        let b = ((s + CHUNK as i64).min(len as i64)) as usize;
        let left = (-s).max(0) as usize;
        let right = (BORDER as i64).min(s + CHUNK as i64 - len as i64).max(0) as usize;
        buf.clear();
        buf.resize(left * N_MELS, 0.0);
        buf.extend_from_slice(&spect[a * N_MELS..b * N_MELS]);
        buf.resize(buf.len() + right * N_MELS, 0.0);
        let t = buf.len() / N_MELS;
        let out = model.run(&[ml::Input { name: "spect", shape: vec![1, t, N_MELS], data: &buf }])?;
        if out.len() != 2 || out[0].data.len() != t || out[1].data.len() != t {
            bail!("beat_this の出力の形が違います: {:?}", out.iter().map(|o| o.shape.clone()).collect::<Vec<_>>());
        }
        // 前後 BORDER フレームを捨てて [s + BORDER, s + CHUNK - BORDER) に置く
        let src_b = &out[0].data[BORDER..t - BORDER];
        let src_d = &out[1].data[BORDER..t - BORDER];
        let dst0 = (s + BORDER as i64).max(0) as usize;
        let dst1 = ((s + CHUNK as i64 - BORDER as i64) as usize).min(len);
        let n = dst1.saturating_sub(dst0).min(src_b.len());
        beat[dst0..dst0 + n].copy_from_slice(&src_b[..n]);
        down[dst0..dst0 + n].copy_from_slice(&src_d[..n]);
    }
    Ok((beat, down))
}

// ---------------- Postprocessor(type="minimal") ----------------

fn peaks(l: &[f32]) -> Vec<usize> {
    let n = l.len();
    (0..n)
        .filter(|&i| {
            let lo = i.saturating_sub(3);
            let hi = (i + 4).min(n);
            let mx = l[lo..hi].iter().copied().fold(f32::NEG_INFINITY, f32::max);
            l[i] == mx && l[i] > 0.0
        })
        .collect()
}

/// beat_this.model.postprocessor.deduplicate_peaks（width 以内に続く山を、その平均の位置 1 つにする）
fn deduplicate_peaks(peaks: &[usize], width: f64) -> Vec<f64> {
    let mut out = vec![];
    let mut it = peaks.iter().map(|&p| p as f64);
    let Some(mut p) = it.next() else {
        return out;
    };
    let mut c = 1.0;
    for p2 in it {
        if p2 - p <= width {
            c += 1.0;
            p += (p2 - p) / c;
        } else {
            out.push(p);
            p = p2;
            c = 1.0;
        }
    }
    out.push(p);
    out
}

/// logits → (拍, 小節頭)（秒）。
pub fn postprocess(beat: &[f32], down: &[f32]) -> (Vec<f64>, Vec<f64>) {
    let beat_time: Vec<f64> = deduplicate_peaks(&peaks(beat), 1.0).iter().map(|f| f / FPS).collect();
    let mut down_time: Vec<f64> = deduplicate_peaks(&peaks(down), 1.0).iter().map(|f| f / FPS).collect();
    if !beat_time.is_empty() {
        for d in down_time.iter_mut() {
            let mut best = 0;
            for (i, b) in beat_time.iter().enumerate() {
                if (b - *d).abs() < (beat_time[best] - *d).abs() {
                    best = i;
                }
            }
            *d = beat_time[best];
        }
    }
    down_time.sort_by(|a, b| a.partial_cmp(b).unwrap());
    down_time.dedup();
    (beat_time, down_time)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_like_python() {
        // np.arange(-6, len - 6, 1488)、len > 1488 なら最後を len - 1494 に
        assert_eq!(chunk_starts(1001), vec![-6]);
        assert_eq!(chunk_starts(1488), vec![-6]);
        assert_eq!(chunk_starts(1490), vec![-6, -4]);
        assert_eq!(chunk_starts(1801), vec![-6, 307]);
        assert_eq!(chunk_starts(5000), vec![-6, 1482, 2970, 3506]);
    }

    #[test]
    fn dedup_like_python() {
        // Python の deduplicate_peaks の結果（それまでの平均と比べるので 3 は別の山になる）
        assert_eq!(deduplicate_peaks(&[1, 2, 3, 10, 11], 1.0), vec![1.5, 3.0, 10.5]);
        assert_eq!(deduplicate_peaks(&[1, 2, 3, 4], 1.0), vec![1.5, 3.5]);
        assert_eq!(deduplicate_peaks(&[5], 1.0), vec![5.0]);
    }

    #[test]
    fn resample_lengths() {
        assert_eq!(resample_to_22050(&[0.0; 10], 44100).len(), 5);
        assert_eq!(resample_to_22050(&[0.0; 11], 44100).len(), 6);
        assert_eq!(resample_to_22050(&[0.0; 44101], 44100).len(), 22051);
        assert_eq!(resample_to_22050(&vec![0.0; 48000], 48000).len(), 22050);
    }

    #[test]
    fn resample_sine_gain() {
        // 1kHz は平坦（0dB）、10.8kHz は約 -32dB（soxr の HQ と同じ）
        for (f, want_db) in [(1000.0, 0.0), (10800.0, -32.4)] {
            for sr in [44100u32, 48000] {
                let x: Vec<f32> =
                    (0..sr as usize).map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / sr as f64).sin() as f32).collect();
                let y = resample_to_22050(&x, sr);
                let seg = &y[3000..y.len() - 3000];
                let rms = (seg.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / seg.len() as f64).sqrt();
                let db = 20.0 * (rms * 2f64.sqrt()).log10();
                assert!((db - want_db).abs() < 0.5, "sr {sr} f {f}: {db} dB");
            }
        }
    }
}
