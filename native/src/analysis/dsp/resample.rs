//! サンプルレート変換（librosa.resample(y, orig_sr, target_sr) の既定 res_type="soxr_hq" の移植）。
//!
//! libsoxr（python-soxr 1.1 が同梱する版）の HQ（20 ビット品質・線形位相）を、整数 2ⁿ 分の 1 のとき（44100→22050, 44100→11025）は
//! soxr と同じ構成で再現する:
//! - 1/2: 窓付き sinc（カイザー窓）の FIR 1 段（soxr の「DFT 段」。係数は lsx_design_lpf と同じ式で作る）
//! - 1/2ⁿ: ハーフバンド FIR（half_fir_coefs_8）を n−1 段 → 上の 1/2 段（減衰量の配分も soxr と同じ）
//! soxr は内部を float32 で FFT 畳み込みするので、出力の差は float32 の丸め程度（相対 3e-7 前後）。
//! それ以外の比率は、同じ通過域・阻止域の窓付き sinc の多相フィルタで変換する（soxr とは近いが同じではない）。
//!
//! 出力の長さは soxr と同じ round(n·比率)、そのあと librosa の fix_length で ceil(n·比率) に 0 を足す/切る。
use rayon::prelude::*;
use realfft::RealFftPlanner;
use std::f64::consts::PI;

/// librosa.resample(y, orig_sr=orig, target_sr=target)（res_type="soxr_hq"、fix=True、scale=False）
pub fn resample(y: &[f32], orig: u32, target: u32) -> Vec<f32> {
    if orig == target {
        return y.to_vec();
    }
    let n = y.len();
    let ratio = target as f64 / orig as f64;
    let n_fix = (n as f64 * ratio).ceil() as usize;
    let n_soxr = (n as f64 * ratio + 0.5).floor() as usize;
    let mut out = if orig % target == 0 && (orig / target).is_power_of_two() {
        soxr_pow2(y, (orig / target) as usize, n_soxr)
    } else {
        sinc_rational(y, orig, target, n_soxr)
    };
    out.resize(n_fix, 0.0);
    out
}

// ---------------------------------------------------------------- soxr のフィルタ設計（libsoxr filter.c）

fn linear_to_db(x: f64) -> f64 {
    x.log10() * 20.0
}

fn sine_phi(x: f64) -> f64 {
    ((2.0517e-07 * x - 1.1303e-04) * x + 0.023154) * x + 0.55924
}

fn sine_pow(x: f64) -> f64 {
    0.5f64.ln() / (x * 0.5).sin().ln()
}

fn inv_f_resp(drop: f64, a: f64) -> f64 {
    let x = sine_phi(a);
    let drop = (drop * (std::f64::consts::LN_10 * 0.05)).exp();
    let s = if drop > 0.5 { 1.0 - drop } else { drop };
    let x = s.powf(1.0 / sine_pow(x)).asin() / x;
    if drop > 0.5 {
        x
    } else {
        1.0 - x
    }
}

fn to_3db(a: f64) -> f64 {
    1.0 - inv_f_resp(-3.0, a)
}

fn kaiser_beta(att: f64, tr_bw: f64) -> f64 {
    if att >= 60.0 {
        const COEFS: [[f64; 4]; 10] = [
            [-6.784957e-10, 1.02856e-05, 0.1087556, -0.8988365 + 0.001],
            [-6.897885e-10, 1.027433e-05, 0.10876, -0.8994658 + 0.002],
            [-1.000683e-09, 1.030092e-05, 0.1087677, -0.9007898 + 0.003],
            [-3.654474e-10, 1.040631e-05, 0.1087085, -0.8977766 + 0.006],
            [8.106988e-09, 6.983091e-06, 0.1091387, -0.9172048 + 0.015],
            [9.519571e-09, 7.272678e-06, 0.1090068, -0.9140768 + 0.025],
            [-5.626821e-09, 1.342186e-05, 0.1083999, -0.9065452 + 0.05],
            [-9.965946e-08, 5.073548e-05, 0.1040967, -0.7672778 + 0.085],
            [1.604808e-07, -5.856462e-05, 0.1185998, -1.34824 + 0.1],
            [-1.511964e-07, 6.363034e-05, 0.1064627, -0.9876665 + 0.18],
        ];
        let realm = (tr_bw / 0.0005).ln() / 2f64.ln();
        let c0 = COEFS[(realm as i64).clamp(0, 9) as usize];
        let c1 = COEFS[(1 + realm as i64).clamp(0, 9) as usize];
        let b0 = ((c0[0] * att + c0[1]) * att + c0[2]) * att + c0[3];
        let b1 = ((c1[0] * att + c1[1]) * att + c1[2]) * att + c1[3];
        return b0 + (b1 - b0) * (realm - (realm as i64) as f64);
    }
    if att > 50.0 {
        return 0.1102 * (att - 8.7);
    }
    if att > 20.96 {
        return 0.58417 * (att - 20.96).powf(0.4) + 0.07886 * (att - 20.96);
    }
    0.0
}

/// 第 1 種変形ベッセル関数 I₀（べき級数）
fn bessel_i0(x: f64) -> f64 {
    let q = x * x / 4.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut k = 1.0;
    loop {
        term *= q / (k * k);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
        k += 1.0;
    }
    sum
}

fn make_lpf(num_taps: usize, fc: f64, beta: f64, rho: f64, scale: f64) -> Vec<f64> {
    let m = num_taps - 1;
    let mut h = vec![0.0; num_taps];
    let mult = scale / bessel_i0(beta);
    let mult1 = 1.0 / (0.5 * m as f64 + rho);
    for i in 0..=m / 2 {
        let z = i as f64 - 0.5 * m as f64;
        let x = z * PI;
        let y = z * mult1;
        let mut v = if x != 0.0 { (fc * x).sin() / x } else { fc };
        v *= bessel_i0(beta * (1.0 - y * y).sqrt()) * mult;
        h[i] = v;
        h[m - i] = v;
    }
    h
}

/// lsx_design_lpf（phases=1、k<0: タップ数 ≡ 1 (mod −k)）
fn design_lpf(fp: f64, fs: f64, fn_: f64, att: f64, k: i32) -> Vec<f64> {
    let phases = k.max(1) as f64;
    let modulo = (-k).max(1) as i64;
    let rho = 0.5;
    let (fp, fs) = (fp / fn_.abs(), fs / fn_.abs());
    let mut tr_bw = 0.5 * (fs - fp);
    tr_bw /= phases;
    let fs = fs / phases;
    tr_bw = tr_bw.min(0.5 * fs);
    let fc = fs - tr_bw;
    let beta = kaiser_beta(att, tr_bw * 0.5 / fc);
    let a = ((0.0007528358 - 1.577737e-05 * beta) * beta + 0.6248022) * beta + 0.06186902;
    let n = (a / tr_bw + 1.0).ceil() as i64;
    let n = (n + modulo - 2) / modulo * modulo + 1;
    make_lpf(n as usize, fc, beta, rho, phases)
}

/// soxr の HQ の通過域の端（Nyquist 比）と、1 段目の阻止域減衰量（dB）
fn hq_params() -> (f64, f64) {
    let bits = 20.0;
    let rej = bits * linear_to_db(2.0);
    let fp0 = 1.0 - 0.05 / to_3db(rej);
    let att = (bits + 1.0) * linear_to_db(2.0);
    (fp0, att)
}

/// soxr の half_fir_coefs_8 / _9（float32 版のコアが持つもの）
const HALF_FIR_8: [f64; 8] = [
    3.1154652365332069e-01,
    -8.7344917685739543e-02,
    3.6814458353637280e-02,
    -1.5189204581464479e-02,
    5.4540855610738801e-03,
    -1.5643862626630416e-03,
    3.1816575906323303e-04,
    -3.4799449225005688e-05,
];
const HALF_FIR_9: [f64; 9] = [
    3.1227034755311189e-01,
    -8.9221517147969526e-02,
    3.9139704015071934e-02,
    -1.7250558515852023e-02,
    6.8589440230476112e-03,
    -2.3045049636430419e-03,
    6.0963740543348963e-04,
    -1.1323803957431231e-04,
    1.1197769991000046e-05,
];

/// 段の入力（範囲外は 0）。ブロックごとに u[start..start+buf.len()] を buf に書く
trait Source: Sync {
    fn len(&self) -> usize;
    fn fill(&self, start: i64, buf: &mut [f64]);
}

/// float32 / float64 の配列そのまま
struct Plain<'a, T>(&'a [T]);

impl<T: Copy + Into<f64> + Sync> Source for Plain<'_, T> {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn fill(&self, start: i64, buf: &mut [f64]) {
        let n = self.0.len() as i64;
        for (i, b) in buf.iter_mut().enumerate() {
            let idx = start + i as i64;
            *b = if idx >= 0 && idx < n { self.0[idx as usize].into() } else { 0.0 };
        }
    }
}

/// ハーフバンド FIR で 1/2 にした列（u[t] = 0.5·x[2t] + Σ c_j (x[2t−2j−1] + x[2t+2j+1])。t ≥ 0 のみ、x は範囲外 0）。
/// 大きな中間の配列を作らないよう、必要な所だけその場で計算する
struct HalfBand<'a, S: Source> {
    x: &'a S,
    c: Vec<f64>,
}

impl<S: Source> HalfBand<'_, S> {
    fn reach(&self) -> usize {
        2 * self.c.len() - 1
    }
}

impl<S: Source> Source for HalfBand<'_, S> {
    fn len(&self) -> usize {
        (self.x.len() + self.reach()) / 2 + 1
    }
    fn fill(&self, start: i64, buf: &mut [f64]) {
        let r = self.reach() as i64;
        let nu = self.len() as i64;
        // x[2·start − r .. 2·(start+len) + r]
        let x0 = 2 * start - r;
        let mut xb = vec![0.0f64; 2 * buf.len() + 2 * r as usize + 1];
        self.x.fill(x0, &mut xb);
        for (i, b) in buf.iter_mut().enumerate() {
            let t = start + i as i64;
            if t < 0 || t >= nu {
                *b = 0.0;
                continue;
            }
            let ctr = (2 * i as i64 + r) as usize; // xb の中での 2t
            let mut s = 0.5 * xb[ctr];
            for (j, &cj) in self.c.iter().enumerate() {
                let o = 2 * j + 1;
                s += (xb[ctr - o] + xb[ctr + o]) * cj;
            }
            *b = s;
        }
    }
}

/// 中間の段を配列にしたもの（2⁴ 分の 1 以上のときだけ使う）
fn materialize<S: Source>(s: &S) -> Vec<f64> {
    let mut v = vec![0.0; s.len()];
    v.par_chunks_mut(65536).enumerate().for_each(|(i, c)| s.fill((i * 65536) as i64, c));
    v
}

/// conv[r] = Σ_j h[j]·u[r−j]（u は範囲外 0）の r = d·m + c（c = 中央のタップ、m = 0..n_out）を FFT（overlap-save）で求める。
/// FFT の丸めの雑音（1e-17 程度）がブロック全体に広がるので、フィルタの範囲に 0 でない入力がない出力はちょうど 0 にする
/// （soxr の 1/2 段もそうなる。無音の区間のクロマが雑音を正規化した値にならないように）
fn fir_decimate<S: Source>(u: &S, h: &[f64], d: usize, n_out: usize) -> Vec<f32> {
    if n_out == 0 {
        return vec![];
    }
    let l = h.len();
    let c = l / 2;
    let nfft = (4 * l).next_power_of_two().max(8192);
    let block = ((nfft - l + 1) / d) * d; // 1 ブロックで求める conv の個数（d の倍数）
    let per_block = block / d;
    let mut planner = RealFftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(nfft);
    let inv = planner.plan_fft_inverse(nfft);
    let mut hbuf = vec![0.0; nfft];
    hbuf[..l].copy_from_slice(h);
    let mut hspec = fwd.make_output_vec();
    fwd.process(&mut hbuf, &mut hspec).unwrap();
    let scale = 1.0 / nfft as f64;
    for v in hspec.iter_mut() {
        *v *= scale;
    }
    let mut out = vec![0.0f32; n_out];
    out.par_chunks_mut(per_block).enumerate().for_each_init(
        || {
            (
                vec![0.0f64; nfft],
                fwd.make_output_vec(),
                fwd.make_scratch_vec(),
                inv.make_output_vec(),
                inv.make_scratch_vec(),
                vec![0u32; nfft + 1],
            )
        },
        |(seg, spec, fs, res, is, nz), (b, chunk)| {
            let r0 = (c + b * block) as i64; // このブロックの最初の r
            let start = r0 - (l as i64 - 1);
            u.fill(start, seg);
            // 0 でない入力の累積数（この区間の出力が見る入力はすべて seg の中）
            nz[0] = 0;
            for (i, &v) in seg.iter().enumerate() {
                nz[i + 1] = nz[i] + (v != 0.0) as u32;
            }
            fwd.process_with_scratch(seg, spec, fs).unwrap();
            for (a, b) in spec.iter_mut().zip(&hspec) {
                *a *= *b;
            }
            // 実信号のスペクトル: 直流と Nyquist の虚部は 0（realfft の逆変換の前提）
            let last = spec.len() - 1;
            spec[0].im = 0.0;
            spec[last].im = 0.0;
            inv.process_with_scratch(spec, res, is).unwrap();
            for (k, o) in chunk.iter_mut().enumerate() {
                let i = l - 1 + k * d; // seg の中での r
                *o = if nz[i + 1] - nz[i + 1 - l] == 0 { 0.0 } else { res[i] as f32 };
            }
        },
    );
    out
}

/// soxr HQ の 1/d（d = 2ⁿ）。出力 n_out 個
fn soxr_pow2(y: &[f32], d: usize, n_out: usize) -> Vec<f32> {
    let (fp0, att0) = hq_params();
    let k = d.trailing_zeros() as usize; // 段の数
    let mut att = att0;
    if k > 1 {
        att += linear_to_db(k as f64);
    }
    let half: &[f64] = if att <= 136.51f32 as f64 { &HALF_FIR_8 } else { &HALF_FIR_9 };
    let hc: Vec<f64> = half.iter().map(|&v| v as f32 as f64).collect();
    // soxr の float32 版はタップを float32 で持つ
    let h: Vec<f64> = design_lpf(fp0, 1.0, 2.0, att, -4).iter().map(|&v| v as f32 as f64).collect();
    let x = Plain(y);
    match k {
        1 => fir_decimate(&x, &h, 2, n_out),
        _ => {
            // 最後のハーフバンド以外は配列にする（1/8 以下の比率のときだけ）
            let mut v: Option<Vec<f64>> = None;
            for _ in 0..k.saturating_sub(2) {
                let next = match &v {
                    None => materialize(&HalfBand { x: &x, c: hc.clone() }),
                    Some(prev) => materialize(&HalfBand { x: &Plain(&prev[..]), c: hc.clone() }),
                };
                v = Some(next);
            }
            match &v {
                None => fir_decimate(&HalfBand { x: &x, c: hc }, &h, 2, n_out),
                Some(prev) => fir_decimate(&HalfBand { x: &Plain(&prev[..]), c: hc }, &h, 2, n_out),
            }
        }
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// 2ⁿ 分の 1 以外の比率: 同じ通過域（0.913·Nyquist）・阻止域（Nyquist から）の窓付き sinc の多相フィルタ
fn sinc_rational(y: &[f32], orig: u32, target: u32, n_out: usize) -> Vec<f32> {
    let g = gcd(orig as u64, target as u64);
    let up = (target as u64 / g) as usize; // L
    let down = (orig as u64 / g) as usize; // M
    let (fp0, att) = hq_params();
    let fn_ = up.max(down) as f64;
    let h = design_lpf(fp0, 1.0, fn_, att, -2); // 奇数タップ（中央が 1 つ）
    let gain = up as f64;
    let c = (h.len() / 2) as i64;
    let tl = h.len() as i64;
    let n = y.len() as i64;
    (0..n_out)
        .into_par_iter()
        .with_min_len(1024)
        .map(|m| {
            let pos = m as i64 * down as i64 + c; // 上げたレートでの位置（フィルタの添字 = pos − n·L）
                                                  // 0 <= pos − n·L < tl
            let n_hi = pos.div_euclid(up as i64).min(n - 1);
            let n_lo = (pos - tl + up as i64).div_euclid(up as i64);
            let mut s = 0.0f64;
            let mut i = n_lo.max(0);
            while i <= n_hi {
                s += y[i as usize] as f64 * h[(pos - i * up as i64) as usize];
                i += 1;
            }
            (s * gain) as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_matches_soxr() {
        // soxr HQ 44100→22050 のフィルタ: 389 タップ、中央 0.47843591
        let (fp0, att) = hq_params();
        assert!((fp0 - 0.9137436532733066).abs() < 1e-12);
        let h = design_lpf(fp0, 1.0, 2.0, att, -4);
        assert_eq!(h.len(), 389);
        assert!((h[194] - 0.4784359133183267).abs() < 1e-12);
        let h4 = design_lpf(fp0, 1.0, 2.0, att + linear_to_db(2.0), -4);
        assert_eq!(h4.len(), 409);
    }

    #[test]
    fn three_stages() {
        // 88200→11025（ハーフバンド 2 段 + DFT 段）。soxr との差は 2.4e-7（実測）
        let y: Vec<f32> = (0..88200).map(|i| (2.0 * PI * 1000.0 * i as f64 / 88200.0).sin() as f32 * 0.5 + 0.25).collect();
        let z = resample(&y, 88200, 11025);
        assert_eq!(z.len(), 11025);
        let mid = &z[2000..9000];
        let mean: f64 = mid.iter().map(|&v| v as f64).sum::<f64>() / mid.len() as f64;
        let mx = mid.iter().cloned().fold(f32::MIN, f32::max) as f64;
        assert!((mean - 0.25).abs() < 1e-3 && (mx - 0.75).abs() < 2e-3, "{mean} {mx}");
    }

    #[test]
    fn lengths() {
        let y = vec![0.0f32; 1001];
        assert_eq!(resample(&y, 44100, 22050).len(), 501);
        assert_eq!(resample(&y, 44100, 11025).len(), 251);
        assert_eq!(resample(&y, 48000, 22050).len(), (1001.0f64 * 22050.0 / 48000.0).ceil() as usize);
    }

    #[test]
    fn dc_and_tone() {
        // 直流は 1 のまま、通過域の正弦波は振幅が保たれる
        let n = 44100;
        let y: Vec<f32> = (0..n).map(|i| (2.0 * PI * 1000.0 * i as f64 / 44100.0).sin() as f32 * 0.5 + 0.25).collect();
        for &t in &[22050u32, 11025, 16000] {
            let z = resample(&y, 44100, t);
            let mid = &z[z.len() / 4..3 * z.len() / 4];
            let mean: f64 = mid.iter().map(|&v| v as f64).sum::<f64>() / mid.len() as f64;
            let mx = mid.iter().cloned().fold(f32::MIN, f32::max) as f64;
            assert!((mean - 0.25).abs() < 1e-3, "{t}: {mean}");
            assert!((mx - 0.75).abs() < 2e-3, "{t}: {mx}");
        }
    }
}
