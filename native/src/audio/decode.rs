//! ffmpeg による音声デコード（djmix/audio/decode.py）。解析と再生で同じデコードを使い、拍時刻とサンプル位置を一致させる
//! （MP3/AAC はデコーダごとにエンコーダ遅延の扱いが違うため）。
use anyhow::{bail, Result};
use std::path::Path;
use std::process::{Command, Stdio};

pub const SR: u32 = 44100;

/// ffmpeg の実行ファイル。アプリから起動すると PATH に Homebrew が入っていないので、よくある場所も探す。
pub fn ffmpeg_bin() -> String {
    if let Ok(p) = std::env::var("DJMIX_FFMPEG") {
        return p;
    }
    for c in ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/opt/local/bin/ffmpeg"] {
        if Path::new(c).exists() {
            return c.to_string();
        }
    }
    "ffmpeg".to_string()
}

/// ステレオの音（インターリーブ: L R L R …）。
#[derive(Clone, Default)]
pub struct Stereo {
    pub data: Vec<f32>,
}

impl Stereo {
    pub fn frames(&self) -> usize {
        self.data.len() / 2
    }
    pub fn seconds(&self) -> f64 {
        self.frames() as f64 / SR as f64
    }
    /// (L+R)/2
    pub fn mono(&self) -> Vec<f32> {
        self.data.chunks_exact(2).map(|c| (c[0] + c[1]) * 0.5).collect()
    }
    pub fn channel(&self, ch: usize) -> Vec<f32> {
        self.data.chunks_exact(2).map(|c| c[ch]).collect()
    }
}

/// 曲全体を float32 ステレオ 44.1kHz に展開する。
pub fn decode(path: impl AsRef<Path>) -> Result<Stereo> {
    decode_sr(path, SR)
}

pub fn decode_sr(path: impl AsRef<Path>, sr: u32) -> Result<Stereo> {
    let path = path.as_ref();
    let out = Command::new(ffmpeg_bin())
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-f", "f32le", "-ac", "2", "-ar", &sr.to_string(), "-"])
        .stdin(Stdio::null())
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => bail!("ffmpeg を起動できません（Homebrew で ffmpeg を入れてください）: {e}"),
    };
    if !out.status.success() || out.stdout.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: String = err.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
        bail!("ffmpeg でデコードできません: {}: {}", path.display(), tail);
    }
    let n = out.stdout.len() / 8 * 2;
    let mut data = Vec::with_capacity(n);
    for c in out.stdout[..n * 4].chunks_exact(4) {
        data.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
    }
    Ok(Stereo { data })
}
