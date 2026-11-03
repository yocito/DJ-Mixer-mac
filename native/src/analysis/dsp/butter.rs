//! Butterworth の IIR フィルタ（scipy.signal.butter(N, Wn, btype, fs=…, output="sos") と sosfilt の移植）。
//!
//! scipy と同じ手順・同じ演算順で設計する: buttap → lp2lp/lp2hp/lp2bp（プリワープ）→ bilinear → zpk2sos（pairing="nearest"）。
//! 係数は scipy と最後の 1 ulp まで一致するのが目標（BLAS の複素内積を使う np.poly の部分だけ 1 ulp ずれうる）。
use super::complex::{rmul, C};
use std::f64::consts::PI;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BType {
    Lowpass,
    Highpass,
    Bandpass,
}

/// 2 次セクション [b0, b1, b2, a0, a1, a2]
pub type Sos = [f64; 6];

/// 逐次の複素数の積（np.prod。初期値 1）
fn cprod(v: &[C]) -> C {
    let mut acc = C::real(1.0);
    for &x in v {
        acc = acc * x;
    }
    acc
}

/// scipy.signal.butter(n, wn, btype, fs=fs, output="sos")
pub fn butter_sos(n: usize, wn: &[f64], btype: BType, fs: f64) -> Vec<Sos> {
    // Wn = Wn / (fs/2)
    let wn: Vec<f64> = wn.iter().map(|w| w / (fs / 2.0)).collect();
    // buttap: p = -exp(1j*pi*m/(2N))（複素数の演算として計算する）
    let mut p: Vec<C> = (0..n)
        .map(|i| {
            let m = -(n as f64) + 1.0 + 2.0 * i as f64;
            let arg = (C::new(0.0, PI) * C::real(m)).div(C::real((2 * n) as f64));
            // cexp: exp(re)·(cos im, sin im)。re は ±0 なので exp(re)=1
            let e = arg.re.exp();
            -C::new(e * arg.im.cos(), e * arg.im.sin())
        })
        .collect();
    let mut z: Vec<C> = vec![];
    let mut k = 1.0f64;
    // デジタル: fs=2 に正規化して tan でプリワープ
    let warped: Vec<f64> = wn.iter().map(|w| (2.0 * 2.0) * (PI * w / 2.0).tan()).collect();
    match btype {
        BType::Lowpass => {
            let wo = warped[0];
            let degree = (p.len() - z.len()) as f64;
            z = z.iter().map(|&x| rmul(wo, x)).collect();
            p = p.iter().map(|&x| rmul(wo, x)).collect();
            k *= wo.powf(degree);
        }
        BType::Highpass => {
            let wo = warped[0];
            let degree = p.len() - z.len();
            let kz = C::real(1.0); // prod(-z)。z は空
            let kp = cprod(&p.iter().map(|&x| -x).collect::<Vec<_>>());
            k *= kz.div(kp).re;
            z = z.iter().map(|&x| C::real(wo).div(x)).collect();
            p = p.iter().map(|&x| C::real(wo).div(x)).collect();
            z.extend(std::iter::repeat(C::real(0.0)).take(degree));
        }
        BType::Bandpass => {
            let bw = warped[1] - warped[0];
            let wo = (warped[0] * warped[1]).sqrt();
            let degree = p.len() - z.len();
            let half = |x: C| (x * C::real(bw)).div(C::real(2.0));
            let wo2 = wo * wo;
            let shift = |v: &[C]| -> Vec<C> {
                let lp: Vec<C> = v.iter().map(|&x| half(x)).collect();
                let mut out: Vec<C> = lp
                    .iter()
                    .map(|&x| {
                        let s = x * x;
                        x + C::new(s.re - wo2, s.im).sqrt()
                    })
                    .collect();
                out.extend(lp.iter().map(|&x| {
                    let s = x * x;
                    x - C::new(s.re - wo2, s.im).sqrt()
                }));
                out
            };
            let zb = shift(&z);
            p = shift(&p);
            z = zb;
            z.extend(std::iter::repeat(C::real(0.0)).take(degree));
            k *= bw.powf(degree as f64);
        }
    }
    // bilinear_zpk(z, p, k, fs=2)
    let fs2 = C::real(4.0);
    let degree = p.len() - z.len();
    let num = cprod(&z.iter().map(|&x| fs2 - x).collect::<Vec<_>>());
    let den = cprod(&p.iter().map(|&x| fs2 - x).collect::<Vec<_>>());
    let kz = k * num.div(den).re;
    let mut zz: Vec<C> = z.iter().map(|&x| (fs2 + x).div(fs2 - x)).collect();
    zz.extend(std::iter::repeat(C::real(-1.0)).take(degree));
    let pz: Vec<C> = p.iter().map(|&x| (fs2 + x).div(fs2 - x)).collect();
    zpk2sos(zz, pz, kz)
}

/// scipy の _cplxreal: (複素数の片側（虚部 > 0 と共役の平均）, 実数)
fn cplxreal(z: &[C]) -> (Vec<C>, Vec<f64>) {
    if z.is_empty() {
        return (vec![], vec![]);
    }
    let tol = 100.0 * f64::EPSILON;
    let mut z = z.to_vec();
    // lexsort((abs(imag), real)): 実部で並べ、同じなら |虚部|（安定）
    z.sort_by(|a, b| a.re.partial_cmp(&b.re).unwrap().then(a.im.abs().partial_cmp(&b.im.abs()).unwrap()));
    let is_real: Vec<bool> = z.iter().map(|x| x.im.abs() <= tol * x.abs()).collect();
    let zr: Vec<f64> = z.iter().zip(&is_real).filter(|(_, &r)| r).map(|(x, _)| x.re).collect();
    if zr.len() == z.len() {
        return (vec![], zr);
    }
    let zc0: Vec<C> = z.iter().zip(&is_real).filter(|(_, &r)| !r).map(|(x, _)| *x).collect();
    let mut zp: Vec<C> = zc0.iter().copied().filter(|x| x.im > 0.0).collect();
    let mut zn: Vec<C> = zc0.iter().copied().filter(|x| x.im < 0.0).collect();
    assert_eq!(zp.len(), zn.len(), "共役のない複素数");
    // 実部が同じ並びは |虚部| で並べ直す
    let same_real: Vec<bool> = zp.windows(2).map(|w| w[1].re - w[0].re <= tol * w[0].abs()).collect();
    let mut i = 0;
    while i < same_real.len() {
        if same_real[i] {
            let start = i;
            while i < same_real.len() && same_real[i] {
                i += 1;
            }
            let stop = i + 1;
            zp[start..stop].sort_by(|a, b| a.im.abs().partial_cmp(&b.im.abs()).unwrap());
            zn[start..stop].sort_by(|a, b| a.im.abs().partial_cmp(&b.im.abs()).unwrap());
        } else {
            i += 1;
        }
    }
    let zc: Vec<C> = zp.iter().zip(&zn).map(|(&a, &b)| (a + b.conj()).div(C::real(2.0))).collect();
    (zc, zr)
}

fn argmin_by(v: &[C], f: impl Fn(C) -> f64) -> usize {
    // np.argmin: 最初の最小
    let mut best = 0;
    let mut bv = f64::INFINITY;
    for (i, &x) in v.iter().enumerate() {
        let d = f(x);
        if i == 0 || d < bv {
            bv = d;
            best = i;
        }
    }
    best
}

/// scipy の _nearest_real_complex_idx（argsort は小さい配列では安定な挿入ソートなので、同じ距離なら先の要素）
fn nearest_idx(fro: &[C], to: C, which: &str) -> usize {
    let d: Vec<f64> = fro.iter().map(|&x| (x - to).abs()).collect();
    let mut order: Vec<usize> = (0..fro.len()).collect();
    order.sort_by(|&a, &b| d[a].partial_cmp(&d[b]).unwrap());
    match which {
        "any" => order[0],
        "real" => *order.iter().find(|&&i| fro[i].is_real()).unwrap(),
        _ => *order.iter().find(|&&i| !fro[i].is_real()).unwrap(),
    }
}

/// np.poly（実数の係数。共役の対は実数になる）
fn poly(roots: &[C]) -> Vec<f64> {
    let mut a = vec![C::real(1.0)];
    for &r in roots {
        // convolve(a, [1, -r])
        let v = [C::real(1.0), -r];
        let mut out = vec![C::real(0.0); a.len() + 1];
        for (kk, o) in out.iter_mut().enumerate() {
            let mut acc: Option<C> = None;
            for (i, &ai) in a.iter().enumerate() {
                if kk >= i && kk - i < 2 {
                    let t = ai * v[kk - i];
                    acc = Some(match acc {
                        None => t,
                        Some(s) => s + t,
                    });
                }
            }
            *o = acc.unwrap();
        }
        a = out;
    }
    a.iter().map(|x| x.re).collect()
}

fn single_sos(z: &[C], p: &[C]) -> Sos {
    let mut s = [0.0; 6];
    let b = poly(z);
    let a = poly(p);
    s[3 - b.len()..3].copy_from_slice(&b);
    s[6 - a.len()..6].copy_from_slice(&a);
    s
}

/// scipy.signal.zpk2sos(z, p, k, pairing="nearest")（デジタル）
pub fn zpk2sos(z: Vec<C>, p: Vec<C>, k: f64) -> Vec<Sos> {
    if z.is_empty() && p.is_empty() {
        return vec![[k, 0.0, 0.0, 1.0, 0.0, 0.0]];
    }
    let mut p = p;
    let mut z = z;
    while p.len() < z.len() {
        p.push(C::real(0.0));
    }
    while z.len() < p.len() {
        z.push(C::real(0.0));
    }
    let n_sections = (p.len().max(z.len()) + 1) / 2;
    if p.len() % 2 == 1 {
        p.push(C::real(0.0));
        z.push(C::real(0.0));
    }
    let (zc, zr) = cplxreal(&z);
    let mut z: Vec<C> = zc.into_iter().chain(zr.into_iter().map(C::real)).collect();
    let (pc, pr) = cplxreal(&p);
    let mut p: Vec<C> = pc.into_iter().chain(pr.into_iter().map(C::real)).collect();
    let idx_worst = |v: &[C]| argmin_by(v, |x| (1.0 - x.abs()).abs());
    let mut sos = vec![[0.0f64; 6]; n_sections];
    for si in (0..n_sections).rev() {
        let i1 = idx_worst(&p);
        let p1 = p.remove(i1);
        let n_real_p = p.iter().filter(|x| x.is_real()).count();
        if p1.is_real() && n_real_p == 0 {
            // 最後に残った実数の極
            let iz = nearest_idx(&z, p1, "real");
            let z1 = z.remove(iz);
            sos[si] = single_sos(&[z1, C::real(0.0)], &[p1, C::real(0.0)]);
        } else if p.len() + 1 == z.len()
            && !p1.is_real()
            && n_real_p == 1
            && z.iter().filter(|x| x.is_real()).count() == 1
        {
            let iz = nearest_idx(&z, p1, "complex");
            let z1 = z.remove(iz);
            sos[si] = single_sos(&[z1, z1.conj()], &[p1, p1.conj()]);
        } else {
            let p2 = if p1.is_real() {
                let reals: Vec<usize> = (0..p.len()).filter(|&i| p[i].is_real()).collect();
                let sub: Vec<C> = reals.iter().map(|&i| p[i]).collect();
                let i2 = reals[idx_worst(&sub)];
                p.remove(i2)
            } else {
                p1.conj()
            };
            if !z.is_empty() {
                let iz = nearest_idx(&z, p1, "any");
                let z1 = z.remove(iz);
                if !z1.is_real() {
                    sos[si] = single_sos(&[z1, z1.conj()], &[p1, p2]);
                } else if !z.is_empty() {
                    let iz2 = nearest_idx(&z, p1, "real");
                    let z2 = z.remove(iz2);
                    sos[si] = single_sos(&[z1, z2], &[p1, p2]);
                } else {
                    sos[si] = single_sos(&[z1], &[p1, p2]);
                }
            } else {
                sos[si] = single_sos(&[], &[p1, p2]);
            }
        }
    }
    for v in sos[0][..3].iter_mut() {
        *v *= k;
    }
    sos
}

/// scipy.signal.sosfilt（初期状態 0、float64 で計算。scipy の _sosfilt と同じ式の順）
pub fn sosfilt(sos: &[Sos], x: &[f32]) -> Vec<f64> {
    let mut out = Vec::with_capacity(x.len());
    let mut zi = vec![[0.0f64; 2]; sos.len()];
    for &xv in x {
        let mut x_cur = xv as f64;
        for (s, z) in sos.iter().zip(zi.iter_mut()) {
            let x_new = s[0] * x_cur + z[0];
            z[0] = s[1] * x_cur - s[4] * x_new + z[1];
            z[1] = s[2] * x_cur - s[5] * x_new;
            x_cur = x_new;
        }
        out.push(x_cur);
    }
    out
}

/// sosfilt → abs → np.maximum.reduceat(x[:edges[-1]], edges[:-1]) をまとめて行う（出力の配列を作らない）。
/// edges は狭義単調増加であること。
pub fn sosfilt_absmax(sos: &[Sos], x: &[f32], edges: &[usize]) -> Vec<f64> {
    // セクション数を定数にして展開（式の順は sosfilt と同じ）
    match sos.len() {
        1 => absmax_n::<1>(sos.try_into().unwrap(), x, edges),
        2 => absmax_n::<2>(sos.try_into().unwrap(), x, edges),
        3 => absmax_n::<3>(sos.try_into().unwrap(), x, edges),
        4 => absmax_n::<4>(sos.try_into().unwrap(), x, edges),
        _ => {
            let y = sosfilt(sos, &x[..edges.last().copied().unwrap_or(0).min(x.len())]);
            edges.windows(2).map(|w| y[w[0]..w[1]].iter().fold(f64::NEG_INFINITY, |m, &v| np_max(m, v.abs()))).collect()
        }
    }
}

/// np.maximum（NaN を伝える）
#[inline]
fn np_max(m: f64, a: f64) -> f64 {
    if m.is_nan() || a.is_nan() {
        f64::NAN
    } else if a > m {
        a
    } else {
        m
    }
}

fn absmax_n<const N: usize>(sos: &[Sos; N], x: &[f32], edges: &[usize]) -> Vec<f64> {
    if edges.len() < 2 {
        return vec![];
    }
    let mut out = Vec::with_capacity(edges.len() - 1);
    let mut zi = [[0.0f64; 2]; N];
    let end = (*edges.last().unwrap()).min(x.len());
    for w in edges.windows(2) {
        let (a, b) = (w[0], w[1].min(end));
        if a >= b {
            break;
        }
        let mut cur = f64::NEG_INFINITY;
        for &xv in &x[a..b] {
            let mut x_cur = xv as f64;
            for k in 0..N {
                let s = &sos[k];
                let z = &mut zi[k];
                let x_new = s[0] * x_cur + z[0];
                z[0] = s[1] * x_cur - s[4] * x_new + z[1];
                z[1] = s[2] * x_cur - s[5] * x_new;
                x_cur = x_new;
            }
            cur = np_max(cur, x_cur.abs());
        }
        out.push(cur);
    }
    out
}
