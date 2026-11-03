//! 短時間フーリエ変換（librosa.stft: center=True・pad_mode="constant"・Hann 窓）。
//!
//! librosa は float32 の信号に float64 の窓を掛けて float64 で FFT し、結果を complex64 に入れる。
//! np.abs はその complex64 の hypotf。ここでも FFT は float64、実部・虚部を float32 に丸めてから hypot をとる。
use rayon::prelude::*;
use realfft::RealFftPlanner;
use std::f64::consts::PI;

/// scipy.signal.get_window("hann", n, fftbins=True)（周期的な Hann 窓）
pub fn hann(n: usize) -> Vec<f64> {
    // scipy: general_cosine(n+1, [0.5, 0.5]) の最後を落とす。fac = linspace(-π, π, n+1)
    let step = (PI - (-PI)) / n as f64;
    (0..n).map(|i| 0.5 + 0.5 * (i as f64 * step + (-PI)).cos()).collect()
}

/// glibc の hypotf（float64 で sqrt(x²+y²) を計算して float32 に丸める。numpy の np.abs(complex64) が使うもの）
#[inline]
pub fn hypotf(x: f32, y: f32) -> f32 {
    let (a, b) = (x as f64, y as f64);
    (a * a + b * b).sqrt() as f32
}

/// 振幅スペクトル（行 = フレーム、列 = ビン。float32）
#[derive(Clone, Debug, Default)]
pub struct Spec {
    pub n_frames: usize,
    pub n_bins: usize,
    pub data: Vec<f32>,
}

impl Spec {
    pub fn frame(&self, t: usize) -> &[f32] {
        &self.data[t * self.n_bins..(t + 1) * self.n_bins]
    }
    /// 各要素を 2 乗した float32 の行列（librosa の power=2 のスペクトログラム: |X|**2 を float32 で）
    pub fn power(&self) -> Spec {
        Spec { n_frames: self.n_frames, n_bins: self.n_bins, data: self.data.par_iter().map(|&v| v * v).collect() }
    }
}

/// フレーム数（center=True: 1 + len/hop）
pub fn n_frames(len: usize, hop: usize) -> usize {
    1 + len / hop
}

/// np.abs(librosa.stft(y, n_fft, hop_length=hop))
pub fn stft_mag(y: &[f32], n_fft: usize, hop: usize) -> Spec {
    let n_bins = n_fft / 2 + 1;
    let nf = n_frames(y.len(), hop);
    let win = hann(n_fft);
    let mut planner = RealFftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(n_fft);
    let mut data = vec![0f32; nf * n_bins];
    let pad = (n_fft / 2) as i64;
    let len = y.len() as i64;
    data.par_chunks_mut(n_bins).enumerate().for_each_init(
        || (fft.make_input_vec(), fft.make_output_vec(), fft.make_scratch_vec()),
        |(buf, spec, scratch), (t, row)| {
            let start = (t * hop) as i64 - pad;
            for (i, b) in buf.iter_mut().enumerate() {
                let idx = start + i as i64;
                *b = if idx >= 0 && idx < len { win[i] * y[idx as usize] as f64 } else { 0.0 };
            }
            fft.process_with_scratch(buf, spec, scratch).unwrap();
            for (o, c) in row.iter_mut().zip(spec.iter()) {
                *o = hypotf(c.re as f32, c.im as f32);
            }
        },
    );
    Spec { n_frames: nf, n_bins, data }
}
