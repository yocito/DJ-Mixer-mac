//! 再生用 PCM の配信（djmix/audio/pcm.py。/api/tracks/{id}/pcm）。
//!
//! 解析と同じ ffmpeg のデコード結果を使うので、解析した拍時刻とサンプル位置が一致する。
//! デコード結果は直近 MAX_TRACKS 曲をメモリに保持する（9 分のステレオ float32 で約 190MB）。
use super::decode::{decode, Stereo};
use anyhow::{anyhow, Result};
use parking_lot::{Condvar, Mutex};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

pub const MAX_TRACKS: usize = 4;

#[derive(Default)]
struct Inner {
    data: VecDeque<(i64, Arc<Stereo>)>,
    loading: HashSet<i64>,
    errors: Vec<(i64, String)>,
}

pub struct PcmStore {
    inner: Mutex<Inner>,
    cv: Condvar,
    max: usize,
}

impl Default for PcmStore {
    fn default() -> Self {
        PcmStore::new(MAX_TRACKS)
    }
}

impl PcmStore {
    pub fn new(max: usize) -> PcmStore {
        PcmStore { inner: Mutex::new(Inner::default()), cv: Condvar::new(), max }
    }

    /// 同じ曲の同時要求は 1 回だけデコードする。
    pub fn get(&self, track_id: i64, path: &str) -> Result<Arc<Stereo>> {
        let mut g = self.inner.lock();
        loop {
            if let Some(pos) = g.data.iter().position(|(id, _)| *id == track_id) {
                let item = g.data.remove(pos).unwrap();
                let a = item.1.clone();
                g.data.push_back(item);
                return Ok(a);
            }
            if !g.loading.contains(&track_id) {
                break;
            }
            self.cv.wait(&mut g);
            if let Some(i) = g.errors.iter().position(|(id, _)| *id == track_id) {
                let (_, e) = g.errors.remove(i);
                return Err(anyhow!(e));
            }
        }
        g.loading.insert(track_id);
        drop(g);
        let r = decode(path);
        let mut g = self.inner.lock();
        g.loading.remove(&track_id);
        let out = match r {
            Ok(a) => {
                let a = Arc::new(a);
                g.data.push_back((track_id, a.clone()));
                while g.data.len() > self.max {
                    g.data.pop_front();
                }
                Ok(a)
            }
            Err(e) => {
                g.errors.push((track_id, e.to_string()));
                if g.errors.len() > 16 {
                    g.errors.remove(0);
                }
                Err(e)
            }
        };
        self.cv.notify_all();
        out
    }

    /// offset から frames サンプル分を「L 全部→R 全部」の planar float32 で返す。戻り値: (バイト列, 曲の総サンプル数)。
    pub fn chunk(&self, track_id: i64, path: &str, offset: usize, frames: usize) -> Result<(Vec<u8>, usize)> {
        let a = self.get(track_id, path)?;
        let total = a.frames();
        let s = offset.min(total);
        let e = (offset.saturating_add(frames)).min(total);
        let n = e - s;
        let mut out = Vec::with_capacity(n * 8);
        for ch in 0..2 {
            for i in s..e {
                out.extend_from_slice(&a.data[i * 2 + ch].to_le_bytes());
            }
        }
        Ok((out, total))
    }
}
