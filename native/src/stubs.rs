//! 統合前の仮の関数（並行して移植している部分。統合したら消す）
#![allow(dead_code, unused_variables)]
pub mod stems {
    use crate::analysis::stems::StemLevels;
    pub fn available() -> bool {
        false
    }
    pub fn request(
        track_id: i64,
        path: String,
        on_done: Option<Box<dyn Fn(i64, &StemLevels) + Send + Sync>>,
        audio: bool,
    ) -> &'static str {
        "pending"
    }
    pub fn compute_for_track(track_id: i64, path: &str) -> anyhow::Result<Option<StemLevels>> {
        Ok(None)
    }
    pub fn audio_ready(track_id: i64) -> bool {
        false
    }
    pub fn read_audio(track_id: i64, start: usize, frames: usize) -> anyhow::Result<(Vec<Vec<f32>>, usize)> {
        anyhow::bail!("not ready")
    }
}
