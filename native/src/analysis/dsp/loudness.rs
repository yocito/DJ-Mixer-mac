//! 積分ラウドネス（pyloudnorm.Meter(rate).integrated_loudness の移植。ITU-R BS.1770-4）。
//!
//! K 特性は pyloudnorm の係数（RBJ の式で high_shelf 4dB/1500Hz/Q=1/√2 → high_pass 38Hz/Q=0.5 を作る。
//! ITU の表の係数とはわずかに違う）。400ms のブロック・75% の重なり・絶対ゲート −70 LUFS・相対ゲート −10 LU。
use super::npy::{mean_f64, pairwise_sum_f64, rint};
use rayon::prelude::*;
use std::f64::consts::PI;

/// 双 2 次フィルタ（b, a は a0 で割った値）
#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    pub b: [f64; 3],
    pub a: [f64; 3],
}

/// pyloudnorm の IIRfilter.generate_coefficients（high_shelf / high_pass のみ）
pub fn pyln_filter(g: f64, q: f64, fc: f64, rate: f64, kind: &str) -> Biquad {
    let a_ = 10f64.powf(g / 40.0);
    let w0 = 2.0 * PI * (fc / rate);
    let alpha = w0.sin() / (2.0 * q);
    let c = w0.cos();
    let (b0, b1, b2, a0, a1, a2) = match kind {
        "high_shelf" => {
            let sa = a_.sqrt();
            (
                a_ * ((a_ + 1.0) + (a_ - 1.0) * c + 2.0 * sa * alpha),
                -2.0 * a_ * ((a_ - 1.0) + (a_ + 1.0) * c),
                a_ * ((a_ + 1.0) + (a_ - 1.0) * c - 2.0 * sa * alpha),
                (a_ + 1.0) - (a_ - 1.0) * c + 2.0 * sa * alpha,
                2.0 * ((a_ - 1.0) - (a_ + 1.0) * c),
                (a_ + 1.0) - (a_ - 1.0) * c - 2.0 * sa * alpha,
            )
        }
        "high_pass" => ((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha),
        _ => panic!("未対応のフィルタ: {kind}"),
    };
    Biquad { b: [b0 / a0, b1 / a0, b2 / a0], a: [a0 / a0, a1 / a0, a2 / a0] }
}

/// scipy.signal.lfilter（3 係数、直接形 II 転置。scipy の DOUBLE_filt と同じ式の順）
pub fn lfilter(f: &Biquad, x: &mut [f64]) {
    // lfilter は内部でもう一度 a0 で割る（a0=1 なので値は変わらない）
    let a0 = f.a[0];
    let b = [f.b[0] / a0, f.b[1] / a0, f.b[2] / a0];
    let a = [f.a[0] / a0, f.a[1] / a0, f.a[2] / a0];
    let (mut z0, mut z1) = (0.0f64, 0.0f64);
    for v in x.iter_mut() {
        let xn = *v;
        let yn = z0 + b[0] * xn;
        z0 = z1 + xn * b[1] - yn * a[1];
        z1 = xn * b[2] - yn * a[2];
        *v = yn;
    }
}

/// 1 チャンネル分: K 特性をかけ、ブロック j（[l_j, u_j)）ごとの平均二乗 z[j] を返す。
/// 二乗の列は全部は持たず、まだ使うブロックの範囲だけ持つ（numpy と同じ pairwise 和にするため連続した配列で）
fn channel_blocks(data: &[f32], nch: usize, ch: usize, rate: f64, bounds: &[(usize, usize)]) -> Vec<f64> {
    let shelf = pyln_filter(4.0, 1.0 / 2f64.sqrt(), 1500.0, rate, "high_shelf");
    let hp = pyln_filter(0.0, 0.5, 38.0, rate, "high_pass");
    let n = data.len() / nch;
    let t_g = 0.400f64;
    let scale = 1.0 / (t_g * rate);
    let norm = |f: &Biquad| {
        let a0 = f.a[0];
        ([f.b[0] / a0, f.b[1] / a0, f.b[2] / a0], [f.a[0] / a0, f.a[1] / a0, f.a[2] / a0])
    };
    let (b1, a1) = norm(&shelf);
    let (b2, a2) = norm(&hp);
    let nb = bounds.len();
    let mut z = vec![0.0f64; nb];
    // 2 段のフィルタは区切りごとに 2 つのスレッドで流れ作業にし（high_shelf が区切り k のとき high_pass は k−1）、
    // そこまでで揃ったブロックの和はさらに別のタスクで計算する。各フィルタの中の計算順は逐次のまま
    const SEG: usize = 1 << 18;
    let nseg = n.div_ceil(SEG);
    let (mut s0, mut s1) = (0.0f64, 0.0f64);
    let mut hp_state = (0.0f64, 0.0f64);
    rayon::scope(|sc| {
        let mut rest: &mut [f64] = &mut z[..];
        let mut buf: Vec<f64> = Vec::with_capacity(SEG + 32768);
        let mut base = 0usize; // buf[0] の標本番号
        let mut j = 0usize;
        let mut pos = 0usize; // high_pass まで済んだ標本数
        let mut prev: Option<Vec<f64>> = None;
        for k in 0..=nseg {
            let shelf_in = if k < nseg { Some(&data[k * SEG * nch..((k + 1) * SEG).min(n) * nch]) } else { None };
            let hp_in = prev.take();
            let (cur, _) = rayon::join(
                || {
                    shelf_in.map(|x| {
                        x.chunks_exact(nch)
                            .map(|frame| {
                                let xn = frame[ch] as f64;
                                let yn = s0 + b1[0] * xn;
                                s0 = s1 + xn * b1[1] - yn * a1[1];
                                s1 = xn * b1[2] - yn * a1[2];
                                yn
                            })
                            .collect::<Vec<f64>>()
                    })
                },
                || {
                    let Some(xs) = hp_in else { return };
                    let (mut h0, mut h1) = hp_state;
                    for &xn in &xs {
                        let yn = h0 + b2[0] * xn;
                        h0 = h1 + xn * b2[1] - yn * a2[1];
                        h1 = xn * b2[2] - yn * a2[2];
                        buf.push(yn * yn);
                    }
                    hp_state = (h0, h1);
                    pos += xs.len();
                    let mut j1 = j;
                    while j1 < nb && bounds[j1].1 <= pos {
                        j1 += 1;
                    }
                    if j1 == j {
                        return;
                    }
                    let shared = std::sync::Arc::new(std::mem::take(&mut buf));
                    let (head, tail) = std::mem::take(&mut rest).split_at_mut(j1 - j);
                    rest = tail;
                    let bs = &bounds[j..j1];
                    let (sh, b0) = (shared.clone(), base);
                    sc.spawn(move |_| {
                        for (k, &(l, u)) in bs.iter().enumerate() {
                            head[k] = if u > l { scale * pairwise_sum_f64(&sh[l - b0..u - b0]) } else { 0.0 };
                        }
                    });
                    // まだ使うブロックの先頭から後ろを持ち越す
                    let keep = if j1 < nb { bounds[j1].0.clamp(base, pos) } else { pos };
                    buf = Vec::with_capacity(SEG + (pos - keep) + 32768);
                    buf.extend_from_slice(&shared[keep - base..]);
                    base = keep;
                    j = j1;
                },
            );
            prev = cur;
        }
    });
    z
}

/// pyloudnorm.Meter(rate).integrated_loudness(data)。data はインターリーブ（nch チャンネル、最大 5）。
/// 長さが 400ms に満たないとき pyloudnorm は例外を出すが、ここでは NaN を返す（呼び出し側で −70 にする）。
pub fn integrated_loudness(data: &[f32], nch: usize, rate: f64) -> f64 {
    let n_samples = if nch == 0 { 0 } else { data.len() / nch };
    let t_g = 0.400f64;
    if nch == 0 || nch > 5 || (n_samples as f64) < t_g * rate {
        return f64::NAN;
    }
    let g = [1.0, 1.0, 1.0, 1.41, 1.41];
    let gamma_a = -70.0;
    let overlap = 0.75;
    let step = 1.0 - overlap;
    let t = n_samples as f64 / rate;
    let num_blocks = (rint((t - t_g) / (t_g * step)) as i64 + 1).max(0) as usize;
    let bounds: Vec<(usize, usize)> = (0..num_blocks)
        .map(|j| {
            let jf = j as f64;
            let l = (t_g * (jf * step) * rate) as usize;
            let u = (t_g * (jf * step + 1.0) * rate) as usize;
            (l.min(n_samples), u.min(n_samples))
        })
        .collect();
    let z: Vec<Vec<f64>> = (0..nch).into_par_iter().map(|ch| channel_blocks(data, nch, ch, rate, &bounds)).collect();
    let block_l: Vec<f64> = (0..num_blocks)
        .map(|j| {
            let s: Vec<f64> = (0..nch).map(|i| g[i] * z[i][j]).collect();
            -0.691 + 10.0 * pairwise_sum_f64(&s).log10()
        })
        .collect();
    let avg_gated = |sel: &[usize]| -> Vec<f64> {
        (0..nch)
            .map(|i| {
                let v: Vec<f64> = sel.iter().map(|&j| z[i][j]).collect();
                mean_f64(&v)
            })
            .collect()
    };
    let j_g: Vec<usize> = (0..num_blocks).filter(|&j| block_l[j] >= gamma_a).collect();
    let za = avg_gated(&j_g);
    let s: Vec<f64> = (0..nch).map(|i| g[i] * za[i]).collect();
    let gamma_r = -0.691 + 10.0 * pairwise_sum_f64(&s).log10() - 10.0;
    let j_g: Vec<usize> = (0..num_blocks).filter(|&j| block_l[j] > gamma_r && block_l[j] > gamma_a).collect();
    // nan_to_num
    let za: Vec<f64> = avg_gated(&j_g).into_iter().map(|v| if v.is_nan() { 0.0 } else { v }).collect();
    let s: Vec<f64> = (0..nch).map(|i| g[i] * za[i]).collect();
    -0.691 + 10.0 * pairwise_sum_f64(&s).log10()
}
