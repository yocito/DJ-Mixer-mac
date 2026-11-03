//! 音の解析に使う信号処理（Python 版が使っていた numpy / scipy / librosa / soxr / pyloudnorm の該当部分の移植）。
//!
//! 方針: Python 版と同じ数値になること（解析結果のキャッシュや画面の表示が Windows 版と食い違わないように）。
//! - float32 で計算していた所（librosa の特徴量）は、分岐（しきい値の比較・ヒストグラム・ピーク選び）に効く所だけ
//!   float32 の演算順まで真似る。それ以外は float64 で計算して float32 に丸める（差は 1e-7 程度）
//! - 和は numpy の pairwise 和（npy::pairwise_sum_*）。丸めは numpy の偶数丸め・Python の round と同じもの
pub mod butter;
pub mod chroma;
pub mod complex;
pub mod loudness;
pub mod mel;
pub mod npy;
pub mod onset;
pub mod resample;
pub mod stft;
