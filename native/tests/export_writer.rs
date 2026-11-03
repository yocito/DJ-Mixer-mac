//! 書き出しの保存（writer.rs）: 保存先の名前、WAV に少しずつ書く、MP3 への変換、曲目リスト。
mod export_common;
use djmix::export::writer::{default_dir, normalize_format, target_path, WavSink, FORMATS};
use export_common::tmpdir;
use serde_json::json;

#[test]
fn target_path_sanitizes_and_numbers() {
    let dir = tmpdir();
    let d = dir.path().join("sub").join("dir");
    let ds = d.to_string_lossy().into_owned();
    let p = target_path(Some(&ds), Some(" a/b:c?*<x>| "), "wav").unwrap();
    assert!(d.is_dir(), "フォルダを作る");
    assert_eq!(p, d.join("a_b_c___x__.wav"));
    std::fs::write(&p, b"").unwrap();
    assert_eq!(target_path(Some(&ds), Some(" a/b:c?*<x>| "), "wav").unwrap(), d.join("a_b_c___x__ (2).wav"));
    std::fs::write(d.join("a_b_c___x__ (2).wav"), b"").unwrap();
    assert_eq!(target_path(Some(&ds), Some("a/b:c?*<x>|"), "wav").unwrap(), d.join("a_b_c___x__ (3).wav"));
    // 先頭の . と空白は取る（フォルダの外へ出る名前にしない）。何も残らなければ「ミックス」
    assert_eq!(target_path(Some(&ds), Some("../../etc"), "mp3_320").unwrap(), d.join("_.._etc.mp3"));
    assert_eq!(target_path(Some(&ds), Some(" . .x"), "mp3_192").unwrap(), d.join("x.mp3"));
    assert_eq!(target_path(Some(&ds), Some("   "), "wav").unwrap(), d.join("ミックス.wav"));
    // 名前が無ければ日時
    let auto = target_path(Some(&ds), None, "wav").unwrap();
    let name = auto.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.ends_with(" ミックス.wav") && name.len() > 20, "{name}");
    assert!(default_dir().ends_with("Music/DJ Mixer"));
    assert_eq!(FORMATS.len(), 3);
    assert_eq!(normalize_format(Some("mp3_192")), "mp3_192");
    assert_eq!(normalize_format(Some("flac")), "wav");
    assert_eq!(normalize_format(None), "wav");
}

fn pcm(frames: usize, phase: usize) -> Vec<u8> {
    let mut b = vec![];
    for i in 0..frames {
        let v = ((((i + phase) as f64) * 0.05).sin() * 12000.0) as i16;
        b.extend_from_slice(&v.to_le_bytes());
        b.extend_from_slice(&(-v).to_le_bytes());
    }
    b
}

#[test]
fn wav_sink_incremental_and_tracklist() {
    let dir = tmpdir();
    let out = dir.path().join("rec.wav");
    let mut s = WavSink::new(&out, "wav", 44100).unwrap();
    let mut total = 0;
    for k in 0..37 {
        let n = 1000 + k * 77; // まちまちの長さで届く（/ws/record）
        s.write(&pcm(n, total)).unwrap();
        total += n;
    }
    let tl = vec![json!({"t": 0.0, "title": "A", "artist": "x", "album": ""}), json!({"t": 61.9, "title": "B", "artist": ""})];
    let res = s.close(Some(&tl)).unwrap();
    assert_eq!(res["path"], out.to_string_lossy().as_ref());
    assert_eq!(res["seconds"].as_f64().unwrap(), djmix::util::round(total as f64 / 44100.0, 1));
    let txt = dir.path().join("rec.txt");
    assert_eq!(res["tracklist"], txt.to_string_lossy().as_ref());
    assert_eq!(std::fs::read_to_string(&txt).unwrap(), "00:00  A - x\n01:01  B\n");
    let mut r = hound::WavReader::open(&out).unwrap();
    assert_eq!(r.spec().channels, 2);
    assert_eq!(r.spec().bits_per_sample, 16);
    assert_eq!(r.duration() as usize, total);
    let samples: Vec<i16> = r.samples::<i16>().map(|v| v.unwrap()).collect();
    let want = pcm(total, 0);
    for (i, v) in samples.iter().enumerate() {
        assert_eq!(*v, i16::from_le_bytes([want[2 * i], want[2 * i + 1]]));
    }
}

#[test]
fn wav_sink_dropped_without_close_is_readable() {
    // 録音の途中で接続が切れた: そこまでの分が読める WAV になっている
    let dir = tmpdir();
    let out = dir.path().join("cut.wav");
    {
        let mut s = WavSink::new(&out, "wav", 44100).unwrap();
        s.write(&pcm(5000, 0)).unwrap();
    }
    let r = hound::WavReader::open(&out).unwrap();
    assert_eq!(r.duration(), 5000);
}

#[test]
fn wav_sink_mp3() {
    let dir = tmpdir();
    let out = dir.path().join("m.mp3");
    let mut s = WavSink::new(&out, "mp3_192", 44100).unwrap();
    s.write(&pcm(44100 * 3, 0)).unwrap();
    let res = s.close(None).unwrap();
    assert!(res["tracklist"].is_null());
    assert_eq!(res["seconds"].as_f64().unwrap(), 3.0);
    assert!(out.exists() && std::fs::metadata(&out).unwrap().len() > 10_000);
    assert!(!dir.path().join("m.tmp.wav").exists(), "一時 WAV は消す");
    let d = djmix::audio::decode::decode(&out).unwrap();
    assert!((d.seconds() - 3.0).abs() < 0.1, "{}", d.seconds());
}

#[test]
fn wav_sink_abort_removes_file() {
    let dir = tmpdir();
    let out = dir.path().join("a.mp3");
    let mut s = WavSink::new(&out, "mp3_320", 44100).unwrap();
    s.write(&pcm(100, 0)).unwrap();
    s.abort();
    assert!(!dir.path().join("a.tmp.wav").exists() && !out.exists());
}
