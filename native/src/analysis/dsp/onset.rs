//! オンセット（librosa.onset.onset_strength / onset_detect / util.peak_pick の移植。既定の引数のみ）。
//!
//! onset_strength: メルスペクトログラム（128 本）→ power_to_db → 1 フレーム差の正の部分 → メル方向の平均 → 先頭に lag + n_fft/(2·hop) 個の 0。
//! onset_detect: 0〜1 に正規化 → peak_pick（numba 版と同じ逐次の判定。平均は float32 で足して float64 で割る）。
//! しきい値（平均 + 0.07）との比較で数が決まるので、float32 の演算順を librosa に合わせる。
use super::mel::{mel_filter, melspec, power_to_db};
use super::npy::TINY_F32;
use super::stft::Spec;

/// librosa.onset.onset_strength(S=None, y=…, sr, hop_length) を power スペクトル（n_fft = 2·(ビン数−1)）から
pub fn onset_strength(power: &Spec, sr: f64, hop: usize) -> Vec<f32> {
    let n_fft = 2 * (power.n_bins - 1);
    let bank = mel_filter(sr, n_fft, 128, 0.0, 0.5 * sr);
    let mut s = melspec(power, &bank);
    power_to_db(&mut s);
    let t_len = s.len();
    let lag = 1usize;
    let pad = lag + n_fft / (2 * hop);
    let n_mels = bank.len();
    let mut env = vec![0f32; t_len];
    for (t, e) in env.iter_mut().enumerate() {
        if t < pad {
            continue;
        }
        let j = t - pad; // onset_env[j] = S[j+lag] − S[j]
        if j + lag >= t_len {
            break;
        }
        // np.mean(axis=-2): C 配列の 0 軸なので行ごとに逐次に足す（float32）
        let mut acc = 0f32;
        for m in 0..n_mels {
            acc += (s[j + lag][m] - s[j][m]).max(0.0);
        }
        *e = acc / n_mels as f32;
    }
    env
}

/// numba の np.mean（float32 で逐次に足し、個数で割ると float64）
fn nb_mean(x: &[f32]) -> f64 {
    let mut c = 0f32;
    for &v in x {
        c += v;
    }
    c as f64 / x.len() as f64
}

/// librosa.util.peak_pick（sparse=True）。戻り値はピークのフレーム番号
pub fn peak_pick(
    x: &[f32],
    pre_max: usize,
    post_max: usize,
    pre_avg: usize,
    post_avg: usize,
    delta: f64,
    wait: usize,
) -> Vec<usize> {
    let n = x.len();
    let mut peaks = vec![];
    if n == 0 {
        return peaks;
    }
    let maxof = |a: usize, b: usize| x[a..b].iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let p0 = x[0] >= maxof(0, post_max.min(n)) && (x[0] as f64) >= nb_mean(&x[..post_avg.min(n)]) + delta;
    let mut i = if p0 {
        peaks.push(0);
        wait + 1
    } else {
        1
    };
    while i < n {
        let maxn = maxof(i.saturating_sub(pre_max), (i + post_max).min(n));
        if x[i] != maxn {
            i += 1;
            continue;
        }
        let avgn = nb_mean(&x[i.saturating_sub(pre_avg)..(i + post_avg).min(n)]);
        if !((x[i] as f64) >= avgn + delta) {
            i += 1;
            continue;
        }
        peaks.push(i);
        i += wait + 1;
    }
    peaks
}

/// librosa.onset.onset_detect(y=…, sr=sr, hop_length=hop)（units="frames"）
pub fn onset_detect(power: &Spec, sr: f64, hop: usize) -> Vec<usize> {
    let env = onset_strength(power, sr, hop);
    onset_detect_env(&env, sr, hop)
}

/// onset_detect の正規化と peak_pick（オンセット強度から）
pub fn onset_detect_env(env: &[f32], sr: f64, hop: usize) -> Vec<usize> {
    if env.is_empty() {
        return vec![];
    }
    let mn = env.iter().cloned().fold(f32::INFINITY, f32::min);
    let mut e: Vec<f32> = env.iter().map(|&v| v - mn).collect();
    let mx = e.iter().cloned().fold(f32::NEG_INFINITY, f32::max) + TINY_F32;
    for v in e.iter_mut() {
        *v /= mx;
    }
    if !e.iter().any(|&v| v != 0.0) || !e.iter().all(|v| v.is_finite()) {
        return vec![];
    }
    let h = hop as f64;
    // Python: 0.03 * sr // hop_length など（float の切り捨て除算）→ valid_int(cast=np.ceil)
    let fl = |x: f64| (x / h).floor();
    let pre_max = fl(0.03 * sr).ceil() as usize;
    let post_max = (fl(0.00 * sr) + 1.0).ceil() as usize;
    let pre_avg = fl(0.10 * sr).ceil() as usize;
    let post_avg = (fl(0.10 * sr) + 1.0).ceil() as usize;
    let wait = fl(0.03 * sr).ceil() as usize;
    peak_pick(&e, pre_max, post_max, pre_avg, post_avg, 0.07, wait)
}
