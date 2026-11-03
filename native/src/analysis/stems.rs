//! 音源分離で小節ごとのパートの量を求める（djmix/analysis/stems.py）。
//!
//! Demucs（htdemucs）で ボーカル・ドラム・ベース・その他 に分け、小節ごとの音量（RMS。曲の中で一番大きいパートの値を 1 とする）を
//! 解析キャッシュ `stems.f32`（小節 × 4、列の順は SOURCES）に保存する。
//! パートごとに鳴らす繋ぎに使う曲だけ、分離した音も保存する（8ch = 4 パート × ステレオ）。
//!
//! 分離（Demucs の `apply_model(model, mix, shifts=0, split=True, overlap=0.25)`）は Rust で行い、網の部分だけを ONNX
//! （tools/export_onnx.py が書き出す htdemucs.onnx）で動かす。STFT・iSTFT・複素数のマスク（cac）・区間の重ね合わせは Rust。
//!
//! 分離した音の保存は Python 版と同じ 8ch の Ogg Vorbis（`stems.ogg`、品質 0.4 = libsndfile の compression_level 0.6、
//! 5 分の曲で約 20MB）。書き込みは vorbis_rs（同梱の libvorbis を cc でビルド）、読み込みは libvorbisfile の
//! `ov_pcm_seek`（サンプル単位で正確なシーク）。
use super::cache;
use crate::audio::decode::{decode, Stereo, SR};
use crate::config::model_file;
use crate::ml;
use anyhow::{bail, Context, Result};
use once_cell::sync::{Lazy, OnceCell};
use parking_lot::Mutex;
use realfft::RealFftPlanner;
use serde_json::json;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

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

// ---- 分離した音（Phase 5 B2） ----
pub const AUDIO_VERSION: u32 = 1;

pub fn audio_meta_path(track_id: i64) -> PathBuf {
    cache::track_dir(track_id).join("stems_audio.json")
}

pub fn audio_path(track_id: i64) -> PathBuf {
    cache::track_dir(track_id).join("stems.ogg")
}

pub fn audio_ready(track_id: i64) -> bool {
    let d = cache::track_dir(track_id);
    let Ok(b) = std::fs::read(d.join("stems_audio.json")) else {
        return false;
    };
    let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&b) else {
        return false;
    };
    meta.get("version").and_then(|v| v.as_u64()) == Some(AUDIO_VERSION as u64) && d.join("stems.ogg").exists()
}

/// vorbis_rs の品質（-0.1〜1.0）。Python 版の libsndfile の compression_level 0.6 は品質 1 - 0.6 = 0.4
const VORBIS_QUALITY: f32 = 0.4;

/// 分離した音（8 本の planar。SOURCES の順に L, R）→ stems.ogg（8ch）。書いてから名前を変える（途中で落ちても壊れたファイルを残さない）。
pub fn save_audio(track_id: i64, sep: &[Vec<f32>], sr: u32) -> Result<()> {
    let d = cache::track_dir(track_id);
    std::fs::create_dir_all(&d)?;
    let tmp = d.join("stems.ogg.tmp");
    let serial = (track_id as i32).wrapping_mul(-1640531535) ^ 0x5354_454d;
    write_ogg(&tmp, sep, sr, serial)?;
    std::fs::rename(&tmp, d.join("stems.ogg"))?;
    std::fs::write(
        audio_meta_path(track_id),
        serde_json::to_vec(&json!({"version": AUDIO_VERSION, "sources": SOURCES, "sr": sr, "frames": sep[0].len()}))?,
    )?;
    Ok(())
}

/// planar の音 → Ogg Vorbis（品質 0.4）。
pub fn write_ogg(path: &std::path::Path, chans: &[Vec<f32>], sr: u32, serial: i32) -> Result<()> {
    use std::num::{NonZeroU32, NonZeroU8};
    use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoderBuilder};
    if chans.is_empty() || chans.len() > 255 {
        bail!("チャンネル数が不正です（{}）", chans.len());
    }
    let n = chans[0].len();
    if chans.iter().any(|c| c.len() != n) {
        bail!("チャンネルの長さが揃っていません");
    }
    let f = std::io::BufWriter::new(std::fs::File::create(path).with_context(|| path.display().to_string())?);
    let mut b = VorbisEncoderBuilder::new_with_serial(
        NonZeroU32::new(sr).context("sr")?,
        NonZeroU8::new(chans.len() as u8).unwrap(),
        f,
        serial,
    );
    b.bitrate_management_strategy(VorbisBitrateManagementStrategy::QualityVbr { target_quality: VORBIS_QUALITY });
    let mut enc = b.build().map_err(|e| anyhow::anyhow!("Vorbis の書き込みを始められません: {e}"))?;
    const BLOCK: usize = 8192;
    let mut k = 0;
    while k < n {
        let e = (k + BLOCK).min(n);
        let block: Vec<&[f32]> = chans.iter().map(|c| &c[k..e]).collect();
        enc.encode_audio_block(&block).map_err(|e| anyhow::anyhow!("Vorbis の書き込み: {e}"))?;
        k = e;
    }
    let mut f = enc.finish().map_err(|e| anyhow::anyhow!("Vorbis の書き込み: {e}"))?;
    std::io::Write::flush(&mut f)?;
    Ok(())
}

/// 分離した音の一部（8 本の planar、各 frames 個まで）と曲全体のフレーム数。
pub fn read_audio(track_id: i64, start: usize, frames: usize) -> Result<(Vec<Vec<f32>>, usize)> {
    read_ogg(&audio_path(track_id), start, frames)
}

/// Ogg Vorbis の一部を読む（libvorbisfile。ov_pcm_seek でサンプル単位に正確にシークする）。
pub fn read_ogg(path: &std::path::Path, start: usize, frames: usize) -> Result<(Vec<Vec<f32>>, usize)> {
    let mut vf = vorbisfile::File::open(path)?;
    let total = vf.total()?;
    let ch = vf.channels();
    let start = start.min(total);
    let want = frames.min(total - start);
    let mut out: Vec<Vec<f32>> = (0..ch).map(|_| Vec::with_capacity(want)).collect();
    if want == 0 {
        return Ok((out, total));
    }
    vf.seek(start)?;
    while out[0].len() < want {
        let got = vf.read(want - out[0].len(), &mut out)?;
        if got == 0 {
            break;
        }
    }
    Ok((out, total))
}

/// libvorbisfile の薄い包み（ファイルを Rust の File で読む）。
mod vorbisfile {
    use anyhow::{bail, Result};
    use aotuv_lancer_vorbis_sys::{
        ov_callbacks, ov_clear, ov_info, ov_open_callbacks, ov_pcm_seek, ov_pcm_total, ov_read_float, OggVorbis_File,
    };
    use std::ffi::c_void;
    use std::io::{Read, Seek, SeekFrom};
    use std::mem::MaybeUninit;
    use std::os::raw::{c_int, c_long};

    type Src = std::io::BufReader<std::fs::File>;

    unsafe extern "C" fn read_func(ptr: *mut c_void, size: usize, nmemb: usize, ds: *mut c_void) -> usize {
        let src = unsafe { &mut *(ds as *mut Src) };
        let len = size * nmemb;
        if len == 0 {
            return 0;
        }
        let buf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, len) };
        let mut got = 0;
        while got < len {
            match src.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        got / size
    }

    unsafe extern "C" fn seek_func(ds: *mut c_void, offset: i64, whence: c_int) -> c_int {
        let src = unsafe { &mut *(ds as *mut Src) };
        let pos = match whence {
            0 => SeekFrom::Start(offset as u64),
            1 => SeekFrom::Current(offset),
            2 => SeekFrom::End(offset),
            _ => return -1,
        };
        if src.seek(pos).is_ok() {
            0
        } else {
            -1
        }
    }

    unsafe extern "C" fn tell_func(ds: *mut c_void) -> c_long {
        let src = unsafe { &mut *(ds as *mut Src) };
        src.stream_position().map(|p| p as c_long).unwrap_or(-1)
    }

    unsafe extern "C" fn close_func(ds: *mut c_void) -> c_int {
        drop(unsafe { Box::from_raw(ds as *mut Src) });
        0
    }

    pub struct File {
        vf: Box<OggVorbis_File>,
        channels: usize,
    }

    impl File {
        pub fn open(path: &std::path::Path) -> Result<File> {
            let f = std::fs::File::open(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            let src: *mut Src = Box::into_raw(Box::new(std::io::BufReader::with_capacity(1 << 16, f)));
            let mut vf: Box<MaybeUninit<OggVorbis_File>> = Box::new_uninit();
            let cb = ov_callbacks {
                read_func: Some(read_func),
                seek_func: Some(seek_func),
                close_func: Some(close_func),
                tell_func: Some(tell_func),
            };
            let r = unsafe { ov_open_callbacks(src as *mut c_void, vf.as_mut_ptr(), std::ptr::null(), 0, cb) };
            if r != 0 {
                // 開けなかったときは datasource は閉じられない（libvorbisfile の決まり）ので自分で閉じる
                drop(unsafe { Box::from_raw(src) });
                bail!("Ogg Vorbis を開けません（{r}）: {}", path.display());
            }
            let mut vf = unsafe { vf.assume_init() };
            let info = unsafe { ov_info(&mut *vf, -1) };
            if info.is_null() {
                unsafe { ov_clear(&mut *vf) };
                bail!("Ogg Vorbis の情報がありません: {}", path.display());
            }
            let channels = unsafe { (*info).channels } as usize;
            Ok(File { vf, channels })
        }

        pub fn channels(&self) -> usize {
            self.channels
        }

        pub fn total(&mut self) -> Result<usize> {
            let t = unsafe { ov_pcm_total(&mut *self.vf, -1) };
            if t < 0 {
                bail!("長さが分かりません（{t}）");
            }
            Ok(t as usize)
        }

        pub fn seek(&mut self, pos: usize) -> Result<()> {
            let r = unsafe { ov_pcm_seek(&mut *self.vf, pos as i64) };
            if r != 0 {
                bail!("シークできません（{r}）");
            }
            Ok(())
        }

        /// 最大 max フレームを out の各チャンネルの後ろに足す。戻り値: 読んだフレーム数（0 なら終わり）
        pub fn read(&mut self, max: usize, out: &mut [Vec<f32>]) -> Result<usize> {
            let mut pcm: *mut *mut f32 = std::ptr::null_mut();
            let mut bs: c_int = 0;
            let n = unsafe { ov_read_float(&mut *self.vf, &mut pcm, max.min(1 << 16) as c_int, &mut bs) };
            if n < 0 {
                bail!("Ogg Vorbis を読めません（{n}）");
            }
            let n = n as usize;
            for (c, o) in out.iter_mut().enumerate().take(self.channels) {
                let p = unsafe { *pcm.add(c) };
                o.extend_from_slice(unsafe { std::slice::from_raw_parts(p, n) });
            }
            Ok(n)
        }
    }

    impl Drop for File {
        fn drop(&mut self) {
            unsafe { ov_clear(&mut *self.vf) };
        }
    }
}

// ---- 音源分離（htdemucs） ----

pub const MODEL_FILE: &str = "htdemucs.onnx";

/// htdemucs.onnx と htdemucs.json（export_onnx.py が書く）の値
struct Demucs {
    model: ml::Model,
    /// モデルの出力の順（["drums", "bass", "other", "vocals"]）
    sources: Vec<String>,
    samplerate: u32,
    /// 学習時の区間の長さ（int(segment × samplerate) = 343980）
    seg: usize,
    nfft: usize,
    hop: usize,
    bins: usize,
    frames: usize,
    overlap: f64,
}

static DEMUCS: OnceCell<std::result::Result<Arc<Demucs>, String>> = OnceCell::new();

fn demucs() -> Result<Arc<Demucs>> {
    DEMUCS.get_or_init(|| load_demucs().map(Arc::new).map_err(|e| format!("{e:#}"))).clone().map_err(|e| anyhow::anyhow!(e))
}

fn load_demucs() -> Result<Demucs> {
    let path = model_file(MODEL_FILE).with_context(|| format!("{MODEL_FILE} が見つかりません（tools/export_onnx.py で作る）"))?;
    let meta = ml::read_meta(&path)?;
    let u = |k: &str| -> Result<usize> {
        meta.get(k).and_then(|v| v.as_u64()).map(|v| v as usize).with_context(|| format!("htdemucs.json: {k}"))
    };
    let sources: Vec<String> = meta
        .get("sources")
        .and_then(|v| v.as_array())
        .context("htdemucs.json: sources")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    for s in SOURCES {
        if !sources.iter().any(|x| x == s) {
            bail!("htdemucs.json の sources に {s} がありません: {sources:?}");
        }
    }
    if meta.get("cac").and_then(|v| v.as_bool()) != Some(true) || u("audio_channels")? != 2 {
        bail!("htdemucs.json: cac で 2ch のモデルだけ扱える");
    }
    let (nfft, hop) = (u("nfft")?, u("hop_length")?);
    if hop * 4 != nfft {
        bail!("htdemucs.json: hop_length は nfft / 4 のはず");
    }
    let overlap = meta.pointer("/apply/overlap").and_then(|v| v.as_f64()).unwrap_or(0.25);
    let (seg, bins, frames) = (u("segment_length")?, u("freq_bins")?, u("frames")?);
    if bins != nfft / 2 || frames != seg.div_ceil(hop) {
        bail!("htdemucs.json: freq_bins / frames が合いません");
    }
    Ok(Demucs {
        model: ml::Model::load(&path)?,
        sources,
        samplerate: u("samplerate")? as u32,
        seg,
        nfft,
        hop,
        bins,
        frames,
        overlap,
    })
}

/// 音源分離を使えるか（ML 入りのビルドで、モデルのファイルがあり、ONNX Runtime を読める）。
pub fn available() -> bool {
    ml::compiled() && model_file(MODEL_FILE).is_some() && ml::ready()
}

/// 周期的なハン窓（torch.hann_window）
fn hann(n: usize) -> Vec<f64> {
    (0..n).map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos()).collect()
}

type Pair = [Vec<f32>; 2];

impl Demucs {
    /// HTDemucs.forward（長さ = 学習時の区間の長さ seg）。mix: [L, R] → [ソース] の [L, R]（モデルの出力の順）
    fn forward(&self, mix: &Pair) -> Result<Vec<Pair>> {
        let (l, hl, nfft, bins, fr) = (self.seg, self.hop, self.nfft, self.bins, self.frames);
        let mut planner = RealFftPlanner::<f64>::new();
        // _spec: reflect で前に pad、後ろに pad + le*hl - L 足してから STFT（normalized=True → 1/sqrt(nfft)）。
        // torch.stft の center の分（前後 nfft/2）を含めたフレーム t + 2 が、足した列の [t*hl, t*hl + nfft) になるので
        // center の reflect は使われない。最後の周波数ビンは落とす
        let pad = hl / 2 * 3;
        let win = hann(nfft);
        let fwd = planner.plan_fft_forward(nfft);
        let norm = 1.0 / (nfft as f64).sqrt();
        let mut mag = vec![0f32; 4 * bins * fr];
        let mut buf = vec![0f64; nfft];
        let mut spec = fwd.make_output_vec();
        for (c, x) in mix.iter().enumerate() {
            let n = l as i64;
            let at = |i: i64| -> f64 {
                let mut j = i - pad as i64;
                if j < 0 {
                    j = -j;
                }
                if j >= n {
                    j = 2 * (n - 1) - j;
                }
                x[j as usize] as f64
            };
            for t in 0..fr {
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = at((t * hl + i) as i64) * win[i];
                }
                fwd.process(&mut buf, &mut spec).unwrap();
                // _magnitude（cac）: (左の実部, 左の虚部, 右の実部, 右の虚部) をチャンネルに
                for f in 0..bins {
                    let z = spec[f] * norm;
                    mag[((2 * c) * bins + f) * fr + t] = z.re as f32;
                    mag[((2 * c + 1) * bins + f) * fr + t] = z.im as f32;
                }
            }
        }
        let mix_flat: Vec<f32> = mix[0].iter().chain(mix[1].iter()).copied().collect();
        let t_run = std::time::Instant::now();
        let out = self.model.run(&[
            ml::Input { name: "mag", shape: vec![1, 4, bins, fr], data: &mag },
            ml::Input { name: "mix", shape: vec![1, 2, l], data: &mix_flat },
        ])?;
        log::debug!("htdemucs segment: onnx {:.2}s", t_run.elapsed().as_secs_f64());
        drop(mag);
        let ns = self.sources.len();
        if out.len() != 2 || out[0].data.len() != ns * 4 * bins * fr || out[1].data.len() != ns * 2 * l {
            bail!("htdemucs の出力の形が違います: {:?}", out.iter().map(|o| o.shape.clone()).collect::<Vec<_>>());
        }
        let (freq, time) = (&out[0].data, &out[1].data);
        // _mask（cac）+ _ispec: 周波数に 0 のビンを 1 つ、前後に 0 のフレームを 2 つずつ足して iSTFT
        // （normalized → ×sqrt(nfft)、irfft の 1/nfft、窓をかけて重ね、窓の 2 乗の和で割る、center → 前の nfft/2 を捨てる）、
        // さらに前の pad を捨てて、時間側の出力に足す
        let inv = planner.plan_fft_inverse(nfft);
        let n_frames = fr + 4;
        let ola_len = nfft + hl * (n_frames - 1);
        let mut env = vec![0f64; ola_len];
        for k in 0..n_frames {
            for i in 0..nfft {
                env[k * hl + i] += win[i] * win[i];
            }
        }
        let off = nfft / 2 + pad;
        let scale = 1.0 / (nfft as f64).sqrt();
        let mut res: Vec<Pair> = Vec::with_capacity(ns);
        let mut cspec = inv.make_input_vec();
        let mut frame = inv.make_output_vec();
        let mut ola = vec![0f64; ola_len];
        for s in 0..ns {
            let mut pair: Pair = [vec![0f32; l], vec![0f32; l]];
            for (c, dst) in pair.iter_mut().enumerate() {
                ola.iter_mut().for_each(|v| *v = 0.0);
                let re = &freq[(s * 4 + 2 * c) * bins * fr..(s * 4 + 2 * c + 1) * bins * fr];
                let im = &freq[(s * 4 + 2 * c + 1) * bins * fr..(s * 4 + 2 * c + 2) * bins * fr];
                for t in 0..fr {
                    for f in 0..bins {
                        cspec[f] = realfft::num_complex::Complex::new(re[f * fr + t] as f64, im[f * fr + t] as f64);
                    }
                    // 直流とナイキストの虚部は使われない（torch の irfft と同じ）
                    cspec[0].im = 0.0;
                    cspec[bins] = realfft::num_complex::Complex::new(0.0, 0.0);
                    inv.process(&mut cspec, &mut frame).unwrap();
                    let k0 = (t + 2) * hl;
                    for i in 0..nfft {
                        ola[k0 + i] += frame[i] * scale * win[i];
                    }
                }
                let tb = &time[(s * 2 + c) * l..(s * 2 + c + 1) * l];
                for (p, d) in dst.iter_mut().enumerate() {
                    let q = p + off;
                    *d = (ola[q] / env[q]) as f32 + tb[p];
                }
            }
            res.push(pair);
        }
        Ok(res)
    }

    /// demucs.apply.apply_model(model, mix, shifts=0, split=True, overlap) と同じ。戻り値: [ソース] の [L, R]（モデルの順）
    fn apply(&self, mix: &Pair) -> Result<Vec<Pair>> {
        let length = mix[0].len();
        let seg = self.seg;
        let stride = ((1.0 - self.overlap) * seg as f64) as usize;
        // 真ん中が一番大きい三角の重み（arange(1, seg//2 + 1) と arange(seg - seg//2, 0, -1) をつないで最大値で割る）
        let half = seg / 2;
        let wmax = half.max(seg - half) as f32;
        let weight: Vec<f32> = (0..seg).map(|i| if i < half { (i + 1) as f32 } else { (seg - i) as f32 } / wmax).collect();
        let ns = self.sources.len();
        let mut out: Vec<Pair> = (0..ns).map(|_| [vec![0f32; length], vec![0f32; length]]).collect();
        let mut sum_w = vec![0f32; length];
        let mut offset = 0;
        while offset < length {
            let clen = seg.min(length - offset);
            // TensorChunk.padded(seg): 足りない分は前後の実際の音（曲の外は 0）で、区間を真ん中に置く
            let delta = seg - clen;
            let start = offset as i64 - (delta / 2) as i64;
            let chunk: Pair = [0, 1].map(|c| {
                (0..seg as i64)
                    .map(|i| {
                        let j = start + i;
                        if j >= 0 && (j as usize) < length {
                            mix[c][j as usize]
                        } else {
                            0.0
                        }
                    })
                    .collect()
            });
            let y = self.forward(&chunk)?;
            // center_trim
            let trim = delta / 2;
            for (s, pair) in y.iter().enumerate() {
                for c in 0..2 {
                    let src = &pair[c][trim..trim + clen];
                    for ((d, w), v) in out[s][c][offset..offset + clen].iter_mut().zip(weight.iter()).zip(src.iter()) {
                        *d += *w * *v;
                    }
                }
            }
            for (d, w) in sum_w[offset..offset + clen].iter_mut().zip(weight.iter()) {
                *d += *w;
            }
            offset += stride;
        }
        for pair in out.iter_mut() {
            for ch in pair.iter_mut() {
                for (v, w) in ch.iter_mut().zip(sum_w.iter()) {
                    *v /= *w;
                }
            }
        }
        Ok(out)
    }
}

/// 音源分離（djmix/analysis/stems.py の separate）。audio: ステレオ 44.1kHz →
/// 8 本の planar（ボーカル L, R、ドラム L, R、ベース L, R、その他 L, R。SOURCES の順）。
pub fn separate(audio: &Stereo) -> Result<Vec<Vec<f32>>> {
    let dm = demucs()?;
    if dm.samplerate != SR {
        bail!("htdemucs のサンプリング周波数が {} です（{} のはず）", dm.samplerate, SR);
    }
    let n = audio.frames();
    if n == 0 {
        bail!("音がありません");
    }
    let wav = [audio.channel(0), audio.channel(1)];
    // ref = 左右の平均。(wav - ref.mean()) / (ref.std() + 1e-8)（torch.std は不偏分散）、出力は逆に戻す
    let r: Vec<f64> = wav[0].iter().zip(wav[1].iter()).map(|(a, b)| ((a + b) * 0.5) as f64).collect();
    let mean = r.iter().sum::<f64>() / n as f64;
    let var = if n > 1 { r.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / (n - 1) as f64 } else { 0.0 };
    drop(r);
    let (m, s) = (mean as f32, var.sqrt() as f32 + 1e-8);
    let x: Pair = [0, 1].map(|c| wav[c].iter().map(|v| (v - m) / s).collect());
    drop(wav);
    let t0 = std::time::Instant::now();
    let y = dm.apply(&x)?;
    drop(x);
    log::info!("separated {:.1}s of audio in {:.1}s", n as f64 / SR as f64, t0.elapsed().as_secs_f64());
    let mut y: Vec<Option<Pair>> = y.into_iter().map(Some).collect();
    let mut res = Vec::with_capacity(8);
    for k in SOURCES {
        let i = dm.sources.iter().position(|x| x == k).unwrap();
        let [mut l, mut r] = y[i].take().unwrap();
        for v in l.iter_mut().chain(r.iter_mut()) {
            *v = *v * s + m;
        }
        res.push(l);
        res.push(r);
    }
    Ok(res)
}

/// audio: ステレオ。戻り値: 小節ごとのパートの量（SOURCES の順）。
pub fn compute(audio: &Stereo, sr: u32, bar_times: &[f64], duration: f64) -> Result<StemLevels> {
    Ok(levels(&separate(audio)?, sr, bar_times, duration))
}

fn bar_times(a: &super::types::Analysis) -> Vec<f64> {
    a.bars.iter().map(|b| b.time).collect()
}

/// 解析済みの曲のパートの量を求めて保存する（解析キャッシュの小節を使う）。解析されていなければ None。
pub fn compute_for_track(track_id: i64, path: &str) -> Result<Option<StemLevels>> {
    let Some(a) = cache::read(track_id) else {
        return Ok(None);
    };
    let arr = compute(&decode(path)?, SR, &bar_times(&a), a.duration)?;
    save(track_id, &arr)?;
    Ok(Some(arr))
}

/// 分離して、小節ごとの量と分離した音の両方を保存する。解析されていなければ None。
pub fn compute_audio_for_track(track_id: i64, path: &str) -> Result<Option<StemLevels>> {
    let Some(a) = cache::read(track_id) else {
        return Ok(None);
    };
    let sep = separate(&decode(path)?)?;
    let arr = levels(&sep, SR, &bar_times(&a), a.duration);
    save(track_id, &arr)?;
    save_audio(track_id, &sep, SR)?;
    Ok(Some(arr))
}

// ---- 使う曲だけ、裏で 1 曲ずつ（画面から頼まれたとき・解析の直後） ----

pub type OnDone = Box<dyn Fn(i64, &StemLevels) + Send + Sync>;

struct Item {
    track_id: i64,
    path: String,
    on_done: Option<OnDone>,
    audio: bool,
}

#[derive(Default)]
struct Queue {
    pending: HashSet<(i64, bool)>,
    items: VecDeque<Item>,
    running: bool,
}

static QUEUE: Lazy<Mutex<Queue>> = Lazy::new(|| Mutex::new(Queue::default()));

/// まだならパートの量（audio=true なら分離した音も）を求めるよう頼む。戻り値: "done" / "pending"。
/// 分離した音の依頼は、量だけの依頼より先に扱う（繋ぎに使う曲なので）。処理は裏の 1 本のスレッドで 1 曲ずつ。
pub fn request(track_id: i64, path: String, on_done: Option<OnDone>, audio: bool) -> &'static str {
    let done = if audio { audio_ready(track_id) } else { read(track_id).is_some() };
    if done {
        return "done";
    }
    let mut q = QUEUE.lock();
    if q.pending.insert((track_id, audio)) {
        let item = Item { track_id, path, on_done, audio };
        if audio {
            let pos = q.items.iter().filter(|i| i.audio).count();
            q.items.insert(pos, item);
        } else {
            q.items.push_back(item);
        }
    }
    if !q.running {
        match std::thread::Builder::new().name("stems".into()).spawn(run_queue) {
            Ok(_) => q.running = true,
            Err(e) => log::error!("stems thread: {e}"),
        }
    }
    "pending"
}

/// 待っている依頼（(track_id, audio)。扱う順）
pub fn queued() -> Vec<(i64, bool)> {
    QUEUE.lock().items.iter().map(|i| (i.track_id, i.audio)).collect()
}

/// 依頼中（待っている・処理中）の曲か
pub fn is_pending(track_id: i64, audio: bool) -> bool {
    QUEUE.lock().pending.contains(&(track_id, audio))
}

fn run_queue() {
    loop {
        let item = {
            let mut q = QUEUE.lock();
            match q.items.pop_front() {
                Some(i) => i,
                None => {
                    q.running = false;
                    return;
                }
            }
        };
        let Item { track_id, path, on_done, audio } = item;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let arr = if audio { compute_audio_for_track(track_id, &path) } else { compute_for_track(track_id, &path) }?;
            if let (Some(arr), Some(cb)) = (&arr, &on_done) {
                cb(track_id, arr);
            }
            anyhow::Ok(arr)
        }));
        match r {
            Ok(Ok(arr)) => log::info!("stems done for {track_id} ({} bars)", arr.as_ref().map(|a| a.len()).unwrap_or(0)),
            Ok(Err(e)) => log::error!("stems failed for {track_id}: {e:#}"),
            Err(_) => log::error!("stems panicked for {track_id}"),
        }
        QUEUE.lock().pending.remove(&(track_id, audio));
    }
}
