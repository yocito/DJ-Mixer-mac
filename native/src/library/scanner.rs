//! 音楽フォルダのタグ走査（djmix/library/scanner.py）。拍・キー等の解析はしない（使う曲だけ解析する）。
//!
//! 変更検出は (path, size, mtime)。変わっていない曲はタグを読み直さない。消えた曲は DB から削除する。
//! タグは lofty で読む（読めない形式は ffprobe で補う）。読めなければファイル名・フォルダ名で補う。
use super::textfix::fix_mojibake;
use crate::{config, db, util};
use anyhow::{bail, Result};
use parking_lot::Mutex;
use rayon::prelude::*;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const AUDIO_EXT: &[&str] = &["mp3", "m4a", "mp4", "aac", "flac", "wav", "aif", "aiff", "ogg", "opus", "wma", "alac", "caf"];

#[derive(Debug, Clone, Default)]
pub struct TagInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub year: Option<String>,
    pub duration: Option<f64>,
}

fn clean(v: Option<String>) -> Option<String> {
    let s = v?.trim().to_string();
    if s.is_empty() {
        return None;
    }
    Some(fix_mojibake(&s).unwrap_or(s)).filter(|x| !x.is_empty())
}

fn read_lofty(path: &Path) -> Result<TagInfo> {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::prelude::{Accessor, ItemKey};
    let tf = lofty::probe::Probe::open(path)?.guess_file_type()?.read()?;
    let mut info = TagInfo::default();
    let d = tf.properties().duration().as_secs_f64();
    if d > 0.0 {
        info.duration = Some(d);
    }
    if let Some(t) = tf.primary_tag().or_else(|| tf.first_tag()) {
        info.title = clean(t.title().map(|s| s.into_owned()));
        info.artist = clean(t.artist().map(|s| s.into_owned()))
            .or_else(|| clean(t.get_string(&ItemKey::AlbumArtist).map(|s| s.to_string())));
        info.album = clean(t.album().map(|s| s.into_owned()));
        info.genre = clean(t.genre().map(|s| s.into_owned()));
        info.year = clean(t.get_string(&ItemKey::RecordingDate).map(|s| s.to_string()))
            .or_else(|| clean(t.get_string(&ItemKey::Year).map(|s| s.to_string())))
            .or_else(|| t.year().map(|y| y.to_string()));
    }
    Ok(info)
}

/// lofty で読めない形式（WMA など）は ffprobe で。
fn read_ffprobe(path: &Path) -> Result<TagInfo> {
    let bin = crate::audio::decode::ffmpeg_bin().replace("ffmpeg", "ffprobe");
    let out = std::process::Command::new(bin)
        .args(["-v", "error", "-print_format", "json", "-show_format"])
        .arg(path)
        .output()?;
    if !out.status.success() {
        bail!("ffprobe failed");
    }
    let v: Value = serde_json::from_slice(&out.stdout)?;
    let f = &v["format"];
    let tags = f["tags"].as_object().cloned().unwrap_or_default();
    let get = |keys: &[&str]| -> Option<String> {
        for k in keys {
            for (tk, tv) in &tags {
                if tk.eq_ignore_ascii_case(k) {
                    if let Some(s) = tv.as_str() {
                        if !s.trim().is_empty() {
                            return clean(Some(s.to_string()));
                        }
                    }
                }
            }
        }
        None
    };
    Ok(TagInfo {
        title: get(&["title"]),
        artist: get(&["artist", "album_artist"]),
        album: get(&["album"]),
        genre: get(&["genre"]),
        year: get(&["date", "year"]),
        duration: f["duration"].as_str().and_then(|s| s.parse().ok()),
    })
}

/// タグと長さを読む。読めなければファイル名・フォルダ名で補う。
pub fn read_tags(path: &Path) -> TagInfo {
    let mut info = match read_lofty(path) {
        Ok(i) => i,
        Err(e) => {
            log::debug!("tag read failed {}: {e}", path.display());
            read_ffprobe(path).unwrap_or_default()
        }
    };
    if info.duration.is_none() {
        if let Ok(i) = read_ffprobe(path) {
            info.duration = i.duration;
        }
    }
    if info.title.is_none() {
        info.title = path.file_stem().map(|s| s.to_string_lossy().into_owned());
    }
    if info.album.is_none() {
        info.album = path.parent().and_then(|p| p.file_name()).map(|s| s.to_string_lossy().into_owned());
    }
    info
}

#[derive(Clone)]
pub struct Scanner {
    inner: Arc<Mutex<State>>,
    roots: Arc<Mutex<Vec<PathBuf>>>,
}

#[derive(Clone, Default)]
struct State {
    running: bool,
    found: usize,
    done: usize,
    added: usize,
    updated: usize,
    removed: usize,
    started: Option<f64>,
    finished: Option<f64>,
    error: Option<String>,
    offline: Vec<String>,
}

fn mtime_of(md: &std::fs::Metadata) -> f64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl Scanner {
    pub fn new(roots: Vec<String>) -> Scanner {
        let s = Scanner { inner: Arc::new(Mutex::new(State::default())), roots: Arc::new(Mutex::new(vec![])) };
        s.set_roots(roots);
        s
    }

    pub fn set_roots(&self, roots: Vec<String>) {
        let mut r: Vec<PathBuf> = roots.into_iter().map(PathBuf::from).collect();
        if r.is_empty() {
            if let Some(m) = &config::paths().music_root {
                r.push(m.clone());
            }
        }
        *self.roots.lock() = r;
    }

    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots.lock().clone()
    }

    pub fn root(&self) -> Option<PathBuf> {
        self.roots.lock().first().cloned().or_else(|| config::paths().music_root.clone())
    }

    pub fn status(&self) -> Value {
        let s = self.inner.lock().clone();
        json!({"running": s.running, "found": s.found, "done": s.done, "added": s.added, "updated": s.updated,
               "removed": s.removed, "started": s.started, "finished": s.finished, "error": s.error, "offline": s.offline})
    }

    pub fn start(&self) -> bool {
        {
            let mut s = self.inner.lock();
            if s.running {
                return false;
            }
            *s = State { running: true, started: Some(util::now()), ..Default::default() };
        }
        let me = self.clone();
        std::thread::Builder::new()
            .name("scanner".into())
            .spawn(move || {
                if let Err(e) = me.scan() {
                    log::error!("scan failed: {e:#}");
                    me.inner.lock().error = Some(e.to_string());
                }
                let mut s = me.inner.lock();
                s.running = false;
                s.finished = Some(util::now());
            })
            .ok();
        true
    }

    /// (音楽フォルダ, パス, 大きさ, 更新時刻) の一覧と、見つからなかった音楽フォルダ。
    fn list_files(&self) -> (Vec<(PathBuf, PathBuf, i64, f64)>, Vec<String>) {
        let mut files = vec![];
        let mut offline = vec![];
        let mut seen = HashSet::new();
        for root in self.roots() {
            if !root.exists() {
                offline.push(root.to_string_lossy().into_owned());
                continue;
            }
            // os.walk と同じ順（各フォルダでファイルを先に、次にサブフォルダへ）
            let mut stack = vec![root.clone()];
            while let Some(dir) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                let mut subdirs = vec![];
                for e in rd.flatten() {
                    let p = e.path();
                    let Ok(md) = std::fs::metadata(&p) else { continue }; // シンボリックリンクは先をたどる
                    if md.is_dir() {
                        subdirs.push(p);
                        continue;
                    }
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.starts_with("._") {
                        continue; // macOS のリソースフォーク（外付けドライブなど）
                    }
                    let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
                    if !AUDIO_EXT.contains(&ext.as_str()) || !seen.insert(p.clone()) {
                        continue;
                    }
                    files.push((root.clone(), p, md.len() as i64, mtime_of(&md)));
                }
                stack.extend(subdirs.into_iter().rev());
            }
        }
        (files, offline)
    }

    pub fn scan(&self) -> Result<()> {
        let t0 = util::now();
        let (files, offline) = self.list_files();
        {
            let mut s = self.inner.lock();
            s.found = files.len();
            s.offline = offline.clone();
        }
        if !offline.is_empty() {
            log::warn!("music folders not found (skipped, their tracks are kept): {offline:?}");
        }
        let roots = self.roots();
        if files.is_empty() && offline.len() == roots.len() && !roots.is_empty() {
            bail!("音楽フォルダが見つかりません: {}", offline.join(", "));
        }
        let scanned: HashSet<String> =
            roots.iter().map(|r| r.to_string_lossy().into_owned()).filter(|r| !offline.contains(r)).collect();
        type Known = HashMap<String, (Option<i64>, Option<f64>, Option<String>)>;
        let known: Known = db::with(|c| {
            let mut st = c.prepare("SELECT path, size, mtime, root FROM tracks")?;
            let m = st
                .query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?
                .flatten()
                .collect();
            Ok(m)
        })?;
        let todo: Vec<&(PathBuf, PathBuf, i64, f64)> = files
            .iter()
            .filter(|(root, p, s, m)| {
                let k = p.to_string_lossy();
                match known.get(k.as_ref()) {
                    Some((ks, km, kr)) => {
                        *ks != Some(*s) || *km != Some(*m) || kr.as_deref() != Some(root.to_string_lossy().as_ref())
                    }
                    None => true,
                }
            })
            .collect();
        self.inner.lock().done += files.len() - todo.len();
        for chunk in todo.chunks(200) {
            // タグ読みは I/O 待ちが主なので並列にする
            let infos: Vec<Option<TagInfo>> = chunk
                .par_iter()
                .map(|(_, p, s, m)| {
                    let k = p.to_string_lossy();
                    let same = matches!(known.get(k.as_ref()), Some((ks, km, _)) if *ks == Some(*s) && *km == Some(*m));
                    if same {
                        None
                    } else {
                        Some(read_tags(p))
                    }
                })
                .collect();
            let mut upd = 0usize;
            let mut add = 0usize;
            db::tx(|c| {
                for ((root, p, s, m), info) in chunk.iter().zip(infos) {
                    let rel = p.strip_prefix(root).unwrap_or(p);
                    let parts: Vec<_> = rel.components().collect();
                    let folder = if parts.len() > 1 { parts[0].as_os_str().to_string_lossy().into_owned() } else { String::new() };
                    let ps = p.to_string_lossy().into_owned();
                    let rs = root.to_string_lossy().into_owned();
                    match info {
                        None => {
                            // ファイルは同じで、音楽フォルダだけ変わった（記録を付け直す）
                            c.execute("UPDATE tracks SET root = ?, folder = ? WHERE path = ?", rusqlite::params![rs, folder, ps])?;
                        }
                        Some(i) => {
                            c.execute(
                                "INSERT INTO tracks(path, folder, size, mtime, title, artist, album, genre, year, duration, root)
                                 VALUES(?,?,?,?,?,?,?,?,?,?,?)
                                 ON CONFLICT(path) DO UPDATE SET folder=excluded.folder, size=excluded.size, mtime=excluded.mtime, root=excluded.root,
                                   title=excluded.title, artist=excluded.artist, album=excluded.album, genre=excluded.genre,
                                   year=excluded.year, duration=excluded.duration,
                                   analysis_status='none', analysis_version=NULL, bpm=NULL, camelot=NULL, lufs=NULL, beat_class=NULL",
                                rusqlite::params![ps, folder, s, m, i.title, i.artist, i.album, i.genre, i.year, i.duration, rs],
                            )?;
                            if known.contains_key(&ps) {
                                upd += 1;
                            } else {
                                add += 1;
                            }
                        }
                    }
                }
                Ok(())
            })?;
            let mut st = self.inner.lock();
            st.updated += upd;
            st.added += add;
            st.done += chunk.len();
        }
        // 消すのは、走査した音楽フォルダの曲でファイルが無くなったものだけ
        let present: HashSet<String> = files.iter().map(|(_, p, _, _)| p.to_string_lossy().into_owned()).collect();
        let gone: Vec<&String> = known
            .iter()
            .filter(|(k, (_, _, r))| !present.contains(*k) && r.as_ref().map(|r| scanned.contains(r)).unwrap_or(false))
            .map(|(k, _)| k)
            .collect();
        db::tx(|c| {
            for g in &gone {
                c.execute("DELETE FROM tracks WHERE path = ?", [g])?;
            }
            Ok(())
        })?;
        self.inner.lock().removed += gone.len();
        log::info!(
            "scan finished: {} files, {} new/changed, {} removed, {:.1}s",
            files.len(),
            todo.len(),
            gone.len(),
            util::now() - t0
        );
        Ok(())
    }
}
