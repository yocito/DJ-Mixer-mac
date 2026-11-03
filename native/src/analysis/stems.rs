//! 音源分離で小節ごとのパートの量を求める（djmix/analysis/stems.py）。
//!
//! Demucs（htdemucs）で ボーカル・ドラム・ベース・その他 に分け、小節ごとの音量（RMS。曲の中で一番大きいパートの値を 1 とする）を
//! 解析キャッシュ `stems.f32`（小節 × 4、列の順は SOURCES）に保存する。
//! パートごとに鳴らす繋ぎに使う曲だけ、分離した音も保存する（8ch = 4 パート × ステレオ）。
use super::cache;
use anyhow::Result;
use serde_json::json;
use std::path::PathBuf;

pub const SOURCES: [&str; 4] = ["vocals", "drums", "bass", "other"];
pub const STEMS_VERSION: u32 = 1;
/// これ以上を「ボーカルがある小節」とみなす
pub const VOCAL_ON: f32 = 0.25;

/// 小節ごとのパートの量（行 = 小節、列 = SOURCES）
pub type StemLevels = Vec<[f32; 4]>;

pub fn save(track_id: i64, arr: &StemLevels) -> Result<()> {
    let d = cache::track_dir(track_id);
    std::fs::create_dir_all(&d)?;
    let mut b = Vec::with_capacity(arr.len() * 16);
    for row in arr {
        for v in row {
            b.extend_from_slice(&v.to_le_bytes());
        }
    }
    std::fs::write(d.join("stems.f32"), b)?;
    std::fs::write(
        d.join("stems.json"),
        serde_json::to_vec(&json!({"version": STEMS_VERSION, "sources": SOURCES, "bars": arr.len()}))?,
    )?;
    Ok(())
}

pub fn read(track_id: i64) -> Option<StemLevels> {
    let d = cache::track_dir(track_id);
    let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(d.join("stems.json")).ok()?).ok()?;
    if meta.get("version").and_then(|v| v.as_u64()) != Some(STEMS_VERSION as u64) {
        return None;
    }
    let b = std::fs::read(d.join("stems.f32")).ok()?;
    Some(
        b.chunks_exact(16)
            .map(|r| {
                let mut o = [0f32; 4];
                for (i, c) in r.chunks_exact(4).enumerate() {
                    o[i] = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                }
                o
            })
            .collect(),
    )
}

/// 分離した音 (4 パート × 2ch × n) → 小節ごとのパートの量 (小節数, 4)。
/// sep は planar: sep[2*s + ch][i]（s = SOURCES の番号）
pub fn levels(sep: &[Vec<f32>], sr: u32, bar_times: &[f64], duration: f64) -> StemLevels {
    let n = sep.first().map(|v| v.len()).unwrap_or(0);
    let mut edges: Vec<f64> = bar_times.to_vec();
    edges.push(duration);
    let mut res: StemLevels = vec![[0f32; 4]; bar_times.len()];
    for b in 0..bar_times.len() {
        let a0 = ((edges[b] * sr as f64) as i64).max(0) as usize;
        let a1 = (((edges[b + 1] * sr as f64) as i64).max(a0 as i64 + 1)) as usize;
        let (a0, a1) = (a0.min(n), a1.min(n));
        for s in 0..4 {
            if a1 <= a0 {
                res[b][s] = 0.0;
                continue;
            }
            let (l, r) = (&sep[2 * s], &sep[2 * s + 1]);
            let mut acc = 0f64;
            for i in a0..a1 {
                let m = (l[i] as f64 + r[i] as f64) * 0.5;
                acc += m * m;
            }
            res[b][s] = (acc / (a1 - a0) as f64).sqrt() as f32;
        }
    }
    let mx = res.iter().flat_map(|r| r.iter()).copied().fold(0f32, f32::max);
    let mx = if mx > 0.0 { mx } else { 1.0 };
    for r in res.iter_mut() {
        for v in r.iter_mut() {
            *v /= mx;
        }
    }
    res
}

// ---- 分離した音（Phase 5 B2）。保存の形式と分離の本体は ML の移植で埋める ----
pub const AUDIO_VERSION: u32 = 1;

pub fn audio_meta_path(track_id: i64) -> PathBuf {
    cache::track_dir(track_id).join("stems_audio.json")
}

// ---- STUB (export agent) ----
// 分離した音（8ch = 4 パート × ステレオ）の読み書きは音源分離の移植で入れる。それまでの仮のもの（いつも「無い」）。

/// 分離した音が保存してあるか
pub fn audio_ready(_track_id: i64) -> bool {
    false
}

/// 分離した音の start フレーム目から frames フレーム（8ch の planar）と、曲全体のフレーム数
pub fn read_audio(_track_id: i64, _start: usize, _frames: usize) -> anyhow::Result<(Vec<Vec<f32>>, usize)> {
    anyhow::bail!("分離した音がありません")
}

/// 曲を分離して、分離した音と小節ごとのパートの量を保存する
pub fn compute_audio_for_track(_track_id: i64, _path: &str) -> anyhow::Result<Option<StemLevels>> {
    anyhow::bail!("音源分離はまだ使えません")
}
// ---- END STUB ----
