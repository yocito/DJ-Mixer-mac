//! 解析キャッシュ <data>/cache/<track_id>/（djmix/analysis/cache.py と同じファイルの形）。
//!
//! - analysis.json: 解析結果（version 付き）
//! - wave.u8: 3 帯域の波形 (3, n) uint8（低→中→高の順に n 個ずつ）
//! - bars.f32: 小節ごとの特徴量 (小節, 15) float32（行ごと）
//! - downbeat.f16: 小節頭の確率（50fps）float16
//!
//! ANALYSIS_VERSION を上げると、古いキャッシュは次に使うとき再解析される。
use super::types::{Analysis, BarFeats, NCOLS};
use crate::config::paths;
use anyhow::Result;
use std::path::PathBuf;

/// 2: 波形の点の間隔を正確に 1/200 秒にした / 3: 曲全体の特徴量 extra（Phase 3 のおまかせ用）
pub const ANALYSIS_VERSION: u32 = 3;

pub fn track_dir(track_id: i64) -> PathBuf {
    paths().cache_dir.join(track_id.to_string())
}

/// wave: (3, n) を帯域ごとに並べたもの（長さ 3n）
pub fn write(track_id: i64, analysis: &Analysis, wave: &[u8], bars: &BarFeats, down_prob: &[f32]) -> Result<()> {
    let d = track_dir(track_id);
    let tmp = d.with_file_name(format!("{track_id}.tmp"));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)?;
    }
    std::fs::create_dir_all(&tmp)?;
    let mut a = analysis.clone();
    a.version = ANALYSIS_VERSION;
    std::fs::write(tmp.join("analysis.json"), serde_json::to_vec(&a)?)?;
    std::fs::write(tmp.join("wave.u8"), wave)?;
    let mut b = Vec::with_capacity(bars.len() * NCOLS * 4);
    for row in bars {
        for v in row {
            b.extend_from_slice(&v.to_le_bytes());
        }
    }
    std::fs::write(tmp.join("bars.f32"), b)?;
    let mut h = Vec::with_capacity(down_prob.len() * 2);
    for v in down_prob {
        h.extend_from_slice(&half::f16::from_f32(*v).to_le_bytes());
    }
    std::fs::write(tmp.join("downbeat.f16"), h)?;
    // 途中で落ちても壊れたキャッシュが残らないよう、一時フォルダに書いてから入れ替える
    // （小節が変わるので、分離の結果 stems.* も消える。Python 版と同じ）
    if d.exists() {
        std::fs::remove_dir_all(&d)?;
    }
    std::fs::rename(&tmp, &d)?;
    Ok(())
}

/// 解析済み（今の版）なら解析結果。
pub fn read(track_id: i64) -> Option<Analysis> {
    let p = track_dir(track_id).join("analysis.json");
    let bytes = std::fs::read(p).ok()?;
    let a: Analysis = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => {
            log::warn!("analysis.json of {track_id} is unreadable: {e}");
            return None;
        }
    };
    (a.version == ANALYSIS_VERSION).then_some(a)
}

/// analysis.json をそのまま（画面に返すとき、型に無いキーも落とさないように）
pub fn read_value(track_id: i64) -> Option<serde_json::Value> {
    let p = track_dir(track_id).join("analysis.json");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(p).ok()?).ok()?;
    (v.get("version").and_then(|x| x.as_u64()) == Some(ANALYSIS_VERSION as u64)).then_some(v)
}

pub fn wave_bytes(track_id: i64) -> Option<Vec<u8>> {
    std::fs::read(track_dir(track_id).join("wave.u8")).ok()
}

pub fn bars_array(track_id: i64) -> Option<BarFeats> {
    let b = std::fs::read(track_dir(track_id).join("bars.f32")).ok()?;
    Some(parse_bars(&b))
}

pub fn parse_bars(b: &[u8]) -> BarFeats {
    b.chunks_exact(NCOLS * 4)
        .map(|row| {
            let mut r = [0f32; NCOLS];
            for (i, c) in row.chunks_exact(4).enumerate() {
                r[i] = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            }
            r
        })
        .collect()
}

pub fn downbeat_prob(track_id: i64) -> Option<Vec<f32>> {
    let b = std::fs::read(track_dir(track_id).join("downbeat.f16")).ok()?;
    Some(b.chunks_exact(2).map(|c| half::f16::from_le_bytes([c[0], c[1]]).to_f32()).collect())
}
