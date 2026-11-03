//! 1 曲の解析（djmix/analysis/pipeline.py）。
use super::types::{Analysis, BarFeats};
use anyhow::{bail, Result};

/// 戻り値: (analysis.json の内容, 波形 (3,n) uint8 を帯域ごとに並べたもの, 小節特徴量, ダウンビート確率 50fps)。
pub fn analyze_file(path: &str) -> Result<(Analysis, Vec<u8>, BarFeats, Vec<f32>)> {
    bail!("解析はまだ組み込まれていません: {path}")
}
