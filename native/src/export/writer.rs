//! 書き出しの保存（djmix/export/writer.py。design_phase3.md §6）。WAV（44.1kHz 16bit ステレオ）に少しずつ書き、MP3 なら最後に ffmpeg で変換する。
//! 曲目リスト（開始時刻つき）も同じ名前の .txt に書く。
//!
//! /ws/record（再生しながら録音）も同じ WavSink を使う: 画面から届いた 16bit の PCM を write() で足していき、最後に close()。
use crate::audio::decode::ffmpeg_bin;
use anyhow::{bail, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 形式 → MP3 のビットレート（WAV は None）
pub const FORMATS: [(&str, Option<&str>); 3] = [("wav", None), ("mp3_320", Some("320k")), ("mp3_192", Some("192k"))];

pub fn is_format(fmt: &str) -> bool {
    FORMATS.iter().any(|(k, _)| *k == fmt)
}

/// 形式の名前を確かめる（知らない名前は wav）
pub fn normalize_format(fmt: Option<&str>) -> &'static str {
    FORMATS.iter().map(|(k, _)| *k).find(|k| Some(*k) == fmt).unwrap_or("wav")
}

pub fn bitrate(fmt: &str) -> Option<&'static str> {
    FORMATS.iter().find(|(k, _)| *k == fmt).and_then(|(_, b)| *b)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// 既定の保存先: ~/Music/DJ Mixer
pub fn default_dir() -> PathBuf {
    home().join("Music").join("DJ Mixer")
}

fn expand_user(p: &str) -> PathBuf {
    if p == "~" {
        home()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

static BAD_CHARS: Lazy<Regex> = Lazy::new(|| Regex::new(r#"[\\/:*?"<>|\x00-\x1f]"#).unwrap());

/// 保存先。フォルダが無ければ作る。同じ名前があれば (2)・(3)… を付ける。
pub fn target_path(folder: Option<&str>, name: Option<&str>, fmt: &str) -> Result<PathBuf> {
    let d = match folder.filter(|f| !f.is_empty()) {
        Some(f) => expand_user(f),
        None => default_dir(),
    };
    std::fs::create_dir_all(&d)?;
    // ファイル名に使えない文字（\ と / を含む）を _ に。先頭の . も取る（フォルダの外へ出る名前にしない）
    let default_name = chrono::Local::now().format("%Y-%m-%d %H%M ミックス").to_string();
    let raw = name.filter(|n| !n.is_empty()).map(|s| s.to_string()).unwrap_or(default_name);
    let cleaned = BAD_CHARS.replace_all(raw.trim(), "_").to_string();
    let mut base = cleaned.trim_start_matches(['.', ' ']).to_string();
    if base.is_empty() {
        base = "ミックス".into();
    }
    let ext = if fmt == "wav" { ".wav" } else { ".mp3" };
    let mut p = d.join(format!("{base}{ext}"));
    let mut n = 2;
    while p.exists() {
        p = d.join(format!("{base} ({n}){ext}"));
        n += 1;
    }
    Ok(p)
}

/// 16bit ステレオの PCM（バイト列）を受け取って WAV に書く。MP3 の指定なら一時 WAV に書いて最後に変換する。
pub struct WavSink {
    pub out: PathBuf,
    pub fmt: String,
    pub sr: u32,
    pub wav_path: PathBuf,
    w: Option<BufWriter<File>>,
    bytes: u64,
    pub frames: u64,
}

const HEADER: usize = 44;

fn header(sr: u32, data_bytes: u32) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    let riff = 36u32.wrapping_add(data_bytes + (data_bytes & 1));
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&riff.to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&2u16.to_le_bytes()); // ステレオ
    h[24..28].copy_from_slice(&sr.to_le_bytes());
    h[28..32].copy_from_slice(&(sr * 4).to_le_bytes());
    h[32..34].copy_from_slice(&4u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    h
}

impl WavSink {
    pub fn new(out: &Path, fmt: &str, sample_rate: u32) -> Result<WavSink> {
        let wav_path = if fmt == "wav" { out.to_path_buf() } else { out.with_extension("tmp.wav") };
        if let Some(p) = wav_path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&wav_path)?);
        w.write_all(&header(sample_rate, 0))?;
        Ok(WavSink { out: out.to_path_buf(), fmt: fmt.to_string(), sr: sample_rate, wav_path, w: Some(w), bytes: 0, frames: 0 })
    }

    /// 16bit ステレオ（リトルエンディアン、L R L R …）を足す
    pub fn write(&mut self, pcm16: &[u8]) -> std::io::Result<()> {
        if let Some(w) = self.w.as_mut() {
            w.write_all(pcm16)?;
            self.bytes += pcm16.len() as u64;
            self.frames += pcm16.len() as u64 / 4;
        }
        Ok(())
    }

    pub fn seconds(&self) -> f64 {
        self.frames as f64 / self.sr as f64
    }

    /// WAV の長さを書き込んで閉じる
    fn finish_wav(&mut self) -> Result<()> {
        if let Some(mut w) = self.w.take() {
            if self.bytes & 1 == 1 {
                w.write_all(&[0])?;
            }
            let data = self.bytes.min(u32::MAX as u64 - 64) as u32;
            w.flush()?;
            let mut f = w.into_inner().map_err(|e| e.into_error())?;
            f.seek(SeekFrom::Start(0))?;
            f.write_all(&header(self.sr, data))?;
            f.flush()?;
        }
        Ok(())
    }

    /// 閉じる（MP3 なら変換）。曲目リストがあれば .txt も書く。戻り値 {path, tracklist, seconds}
    pub fn close(mut self, tracklist: Option<&[Value]>) -> Result<Value> {
        self.finish_wav()?;
        if self.fmt != "wav" {
            let r = Command::new(ffmpeg_bin())
                .args(["-v", "error", "-nostdin", "-y", "-i"])
                .arg(&self.wav_path)
                .args(["-codec:a", "libmp3lame", "-b:a", bitrate(&self.fmt).unwrap_or("320k")])
                .arg(&self.out)
                .stdin(Stdio::null())
                .output();
            let r = match r {
                Ok(r) => r,
                Err(e) => bail!("MP3 への変換に失敗しました: {e}"),
            };
            if !r.status.success() {
                let err = String::from_utf8_lossy(&r.stderr);
                let tail: String = err.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
                bail!("MP3 への変換に失敗しました: {tail}");
            }
            let _ = std::fs::remove_file(&self.wav_path);
        }
        let mut txt = None;
        if let Some(tl) = tracklist.filter(|t| !t.is_empty()) {
            let p = self.out.with_extension("txt");
            std::fs::write(&p, tracklist_text(tl))?;
            txt = Some(p.to_string_lossy().into_owned());
        }
        Ok(json!({
            "path": self.out.to_string_lossy(),
            "tracklist": txt,
            "seconds": crate::util::round(self.seconds(), 1),
        }))
    }

    /// 書きかけを消す（失敗・中止のとき）
    pub fn abort(mut self) {
        self.w = None;
        let _ = std::fs::remove_file(&self.wav_path);
    }
}

impl Drop for WavSink {
    fn drop(&mut self) {
        // 閉じずに捨てられた（接続が切れた等）: そこまでの分を読める WAV にしておく
        let _ = self.finish_wav();
    }
}

fn value_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        Some(x) => x.to_string(),
    }
}

fn truthy_str(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) if n.as_f64() != Some(0.0) => Some(n.to_string()),
        _ => None,
    }
}

/// 曲目リストの文（"MM:SS  曲名 - アーティスト 『アルバム』"。1 時間を超えたら H:MM:SS）
pub fn tracklist_text(items: &[Value]) -> String {
    let mut lines = vec![];
    for it in items {
        let t = it.get("t").and_then(|v| v.as_f64()).unwrap_or(0.0).trunc().max(0.0) as i64;
        let stamp = if t >= 3600 { format!("{}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60) } else { format!("{:02}:{:02}", t / 60, t % 60) };
        let who = truthy_str(it.get("artist")).map(|a| format!(" - {a}")).unwrap_or_default();
        let album = truthy_str(it.get("album")).map(|a| format!(" 『{a}』")).unwrap_or_default(); // アルバム名（使ってみての改善）
        lines.push(format!("{stamp}  {}{who}{album}", value_str(it.get("title"))));
    }
    lines.join("\n") + "\n"
}
