"""Rust 版の DSP（native/src/analysis/{features,beats,bars}.rs, analysis/dsp/）と Python 版の一致を確かめるための fixture を作る。

使い方（リポジトリの直下で。numpy / scipy / librosa 0.11 / soxr / pyloudnorm が入った Python で）:
    PYTHONPATH=. python native/tests/fixtures/dsp/make_fixtures.py

音そのものは保存しない。合成のしかた（正弦波の回転の漸化式・xorshift の雑音。どちらも四則演算だけなので Rust でも同じ値になる）を
JSON に書き、Rust のテスト（tests/dsp_common/mod.rs の synth）で同じ音を作り直す。
浮動小数点の値は誤差なく受け渡すため、f64 はビット列（整数）、配列は little endian のバイト列を base64 にして書く。
"""
from __future__ import annotations

import base64
import json
import math
import struct
from pathlib import Path

import numpy as np

from djmix.analysis import bars, beats, features

OUT = Path(__file__).resolve().parent
MASK = (1 << 64) - 1


def bits(x: float) -> int:
    return struct.unpack("<Q", struct.pack("<d", float(x)))[0]


def b64(a, dtype) -> str:
    return base64.b64encode(np.ascontiguousarray(np.asarray(a, dtype=dtype)).tobytes()).decode()


def f64s(a):
    return {"f64": b64(a, "<f8")}


def f32s(a):
    return {"f32": b64(a, "<f4")}


def u8s(a):
    return {"u8": b64(a, "u1")}


def prob_u8(rng_vals):
    """0〜1 の値を k/255（float32 の割り算。Rust でも同じ値）に量子化した確率の列。fixture を小さくするため"""
    k = np.clip(np.round(np.asarray(rng_vals) * 255), 0, 255).astype(np.uint8)
    return k, k.astype(np.float32) / np.float32(255)


def probs(p):
    """確率の列の書き方: 一定なら {"const", "n"}、それ以外は k/255 の k"""
    p = np.asarray(p, dtype=np.float32)
    if len(p) and np.all(p == p[0]):
        return {"const": float(p[0]), "n": len(p)}
    k = np.clip(np.round(p.astype(np.float64) * 255), 0, 255).astype(np.uint8)
    assert np.array_equal(k.astype(np.float32) / np.float32(255), p)
    return {"u8p": b64(k, "u1")}


# ---------------------------------------------------------------- 合成音

def voice(sr, start_s, len_s, freq, amp, decay_per_s=0.0):
    w = 2 * math.pi * freq / sr
    return [int(round(start_s * sr)), int(round(len_s * sr)), bits(amp), bits(math.cos(w)), bits(math.sin(w)),
            bits(math.exp(-decay_per_s / sr))]


def synth(spec):
    """spec の音（モノラル float64 のリスト）。Rust の dsp_common::synth と同じ演算順"""
    n = spec["n"]
    buf = [0.0] * n
    unb = lambda b: struct.unpack("<d", struct.pack("<Q", b))[0]
    for start, ln, amp, cw, sw, dec in spec["voices"]:
        amp, cw, sw, dec = unb(amp), unb(cw), unb(sw), unb(dec)
        c, s, env = 1.0, 0.0, amp
        for j in range(start, min(n, start + ln)):
            buf[j] += env * s
            c, s = c * cw - s * sw, s * cw + c * sw
            env *= dec
    for start, ln, amp, seed in spec["noise"]:
        amp = unb(amp)
        x = seed
        for j in range(start, min(n, start + ln)):
            x ^= x >> 12
            x ^= (x << 25) & MASK
            x ^= x >> 27
            r = (x * 0x2545F4914F6CDD1D) & MASK
            u = (r >> 11) * (1.0 / 9007199254740992.0) * 2.0 - 1.0
            buf[j] += amp * u
    return buf


def stereo_of(spec):
    buf = np.array(synth(spec), dtype=np.float64)
    left = buf.astype(np.float32)
    right = (buf * struct.unpack("<d", struct.pack("<Q", spec["r_gain"]))[0]).astype(np.float32)
    return np.stack([left, right], axis=1)


def spec_clicks(sr):
    v, t, k = [], 0.0, 0
    while t < 20.0 - 0.05:
        down = k % 4 == 0
        v.append(voice(sr, t, 0.05, 1500 if down else 900, 0.9 if down else 0.4, 60))
        v.append(voice(sr, t, 0.05, 60, 0.9 if down else 0.3, 30))
        t += 60 / (120 if t < 10 else 128)
        k += 1
    n = int(20 * sr)
    return {"sr": sr, "n": n, "voices": v, "noise": [[0, n, bits(0.003), 12345]], "r_gain": bits(0.7)}


def spec_chords(sr, detune=0.2):
    chords = [[48, 60, 64, 67], [45, 57, 60, 64], [41, 57, 60, 65], [43, 55, 59, 62]]
    v = []
    for ci, notes in enumerate(chords):
        for m in notes:
            f = 440.0 * 2 ** ((m - 69 + detune) / 12)
            for h in (1, 2, 3):
                if f * h < sr / 2:
                    v.append(voice(sr, ci * 4.0, 4.0, f * h, 0.15 / h, 1.0))
    n = int(16 * sr)
    return {"sr": sr, "n": n, "voices": v, "noise": [[0, n, bits(0.002), 777]], "r_gain": bits(0.5)}


def spec_noise_silence(sr):
    n = int(12 * sr)
    return {"sr": sr, "n": n, "voices": [voice(sr, 7.0, 3.0, 440.0, 0.2, 0.0)],
            "noise": [[int(2 * sr), int(3 * sr), bits(0.3), 1], [int(7 * sr), int(3 * sr), bits(0.05), 2]],
            "r_gain": bits(1.0)}


def spec_tiny(sr):
    return {"sr": sr, "n": int(0.08 * sr), "voices": [voice(sr, 0.0, 0.08, 440.0, 0.5, 0.0)], "noise": [], "r_gain": bits(0.9)}


SIGNALS = {
    "clicks": (spec_clicks, lambda d: [round(x, 4) for x in list(np.arange(0, 10, 2.0)) + list(10 + np.arange(0, 10 - 1e-9, 4 * 60 / 128))]),
    "chords": (spec_chords, lambda d: [round(x, 4) for x in np.arange(0.25, 16, 2.0)]),
    "noise_silence": (spec_noise_silence, lambda d: [round(x, 4) for x in np.arange(0, 12, 1.5)]),
    "tiny": (spec_tiny, lambda d: [0.0]),
}


def frame_level(y):
    """features.bar_features の前半（22050Hz のとき）を分けて出す"""
    import librosa
    hop = 2048
    S = np.abs(librosa.stft(y, n_fft=4096, hop_length=hop)) ** 2
    tuning = librosa.estimate_tuning(S=S, sr=22050, bins_per_octave=12)
    chroma = librosa.feature.chroma_stft(y=y, sr=22050, hop_length=hop, n_fft=4096)
    spec = np.abs(librosa.stft(y, n_fft=4096, hop_length=hop))
    freqs = librosa.fft_frequencies(sr=22050, n_fft=4096)
    low = spec[freqs < 200].sum(axis=0)
    rms = librosa.feature.rms(y=y, frame_length=4096, hop_length=hop)[0]
    return {"tuning": float(tuning), "chroma": f32s(chroma.T), "low": f32s(low), "rms": f32s(rms)}


def onset_level(y):
    import librosa
    env = librosa.onset.onset_strength(y=y, sr=11025, hop_length=512)
    frames = librosa.onset.onset_detect(y=y, sr=11025, hop_length=512)
    return {"env": f32s(env), "frames": [int(f) for f in frames]}


def keyd(k):
    return {"key": k["key"], "camelot": k["camelot"], "corr": k["corr"]}


def audio_fixtures():
    import librosa
    out = {"signals": {}}
    for name, (mk, bars_of) in SIGNALS.items():
        print("signal", name)
        ent = {}
        for sr in (44100, 22050, 11025):
            spec = mk(sr)
            audio = stereo_of(spec)
            mono = audio.mean(axis=1)
            duration = len(mono) / sr
            bt = bars_of(duration)
            e = {"spec": spec, "bar_times": f64s(bt), "duration": duration}
            if sr == 44100:
                if len(mono) >= 0.4 * sr:
                    e["loudness"] = list(features.loudness(audio, sr))
                e["peak"] = float(20 * np.log10(max(1e-9, np.abs(audio).max())))   # features.loudness の 2 つ目
                e["wave"] = u8s(features.waveform(mono, sr))
                if name in ("clicks", "chords"):
                    for tsr in (22050, 11025):
                        z = librosa.resample(mono, orig_sr=sr, target_sr=tsr)
                        k = 256
                        mid = len(z) // 2
                        e[f"resample_{tsr}"] = {"len": len(z), "head": f32s(z[:k]), "mid_at": mid, "mid": f32s(z[mid:mid + k]),
                                               "tail": f32s(z[-k:]), "energy": float(np.sum(z.astype(np.float64) ** 2))}
            if sr in (44100, 22050):
                feats, keys = features.bar_features(mono, sr, bt, duration)
                e["bar_feats"] = f32s(feats)
                e["keys"] = {k: keyd(v) for k, v in keys.items()}
            if sr == 22050:
                e["frames"] = frame_level(mono)
            if sr in (44100, 11025):
                e["extra"] = features.track_extra(mono, sr)
            if sr == 11025:
                e["onset"] = onset_level(mono)
            ent[str(sr)] = e
        out["signals"][name] = ent
    from scipy.signal import butter
    out["sos"] = {}
    for fs in (44100, 48000, 22050):
        out["sos"][str(fs)] = [f64s(butter(4, 200, "lowpass", fs=fs, output="sos")),
                               f64s(butter(4, [200, 2500], "bandpass", fs=fs, output="sos")),
                               f64s(butter(4, 2500, "highpass", fs=fs, output="sos"))]
    # キーの推定だけ（クロマの平均を直接）
    rng = np.random.default_rng(5)
    out["keys"] = []
    for i in range(40):
        c = rng.random(12).astype(np.float32)
        if i == 0:
            c[:] = 0
        out["keys"].append({"chroma": f32s(c), "key": keyd(features.estimate_key(c))})
    return out


# ---------------------------------------------------------------- 拍の後処理

def beat_case(name, b, prob, downs=None, duration=None):
    b = np.asarray(b, dtype=np.float64)
    prob = np.asarray(prob, dtype=np.float32)
    duration = float(duration if duration is not None else b[-1] + 1.0)
    fx, nfix = beats.fix_octave(b, prob)
    sm = beats.smooth(fx)
    nb = beats.no_beat_regions(sm, prob, duration)
    c = {"name": name, "beats": f64s(b), "prob": probs(prob), "duration": duration,
         "local_tempo": f64s(beats.local_tempo(b)), "fix": f64s(fx), "nfix": nfix, "smooth": f64s(sm),
         "smooth_raw": f64s(beats.smooth(b)), "no_beat": [list(r) for r in nb],
         "no_beat_raw": [list(r) for r in beats.no_beat_regions(b, prob, duration)]}
    if downs is not None:
        m = beats.raw_metrics(b, np.asarray(downs, dtype=np.float64), prob)
        c["downs"] = f64s(downs)
        c["metrics"] = {k: bits(v) for k, v in m.items()}
    return c


def beat_fixtures():
    cases = []
    rng = np.random.default_rng(0)
    true = 0.013 + np.cumsum(np.r_[np.full(40, 60 / 123), np.full(40, 60 / 136)])
    q = np.round((true + rng.normal(0, 0.004, len(true))) * 50) / 50
    cases.append(beat_case("quantized_tempo_change", q, np.ones(3000), downs=q[::4], duration=40.0))

    b = np.r_[np.arange(0, 20, 0.5), np.arange(20, 30, 0.25), np.arange(30, 50, 0.5)]
    cases.append(beat_case("doubled_section", b, np.ones(3000)))

    # 先頭が 2 倍（直前の拍がないので拍確率で位相を選ぶ）
    rng = np.random.default_rng(1)
    b = np.r_[np.arange(0, 6, 0.25), np.arange(6, 40, 0.5)]
    _, p = prob_u8(rng.random(2500))
    cases.append(beat_case("doubled_at_start", b, p, downs=b[::4]))

    # 0.5 倍区間（拍の取りこぼし）。格子ちょうどの拍は |lt/ref − 1| がしきい値 0.25 ちょうどになり、
    # np.polyfit（lstsq）と Rust の閉じた式の 1e-15 の差で判定が割れるので、わずかに揺らす
    rng = np.random.default_rng(4)
    b = np.r_[np.arange(0, 20, 0.5), np.arange(20, 26, 1.0), np.arange(26, 50, 0.5)] + rng.normal(0, 0.003, 94)
    cases.append(beat_case("halved_section", b, np.ones(3000)))
    # 0.5 倍区間が 4 拍以上続く（補正される）
    b = np.r_[np.arange(0, 20, 0.5), np.arange(20, 30, 1.0), np.arange(30, 50, 0.5)] + rng.normal(0, 0.003, 90)
    cases.append(beat_case("halved_long", b, np.ones(3000)))

    # 拍の途切れ・拍確率の低い区間・1.5 倍のまま・冒頭と末尾の空白
    rng = np.random.default_rng(2)
    b = np.r_[np.arange(4, 30, 0.5), np.arange(36, 50, 0.5), 50 + np.cumsum(np.full(12, 0.5 / 1.5)), 54 + np.arange(0, 10, 0.5)]
    _, p = prob_u8(np.clip(0.9 + rng.normal(0, 0.05, 4000), 0, 1))
    p[int(40 * 50):int(46 * 50)] = np.float32(25) / np.float32(255)
    downs = b[::4] + rng.normal(0, 0.002, len(b[::4]))
    cases.append(beat_case("gaps_lowprob_offtempo", b, p, downs=np.sort(downs), duration=70.0))

    # 揺れの大きい拍（C 判定になる）
    rng = np.random.default_rng(3)
    b = np.cumsum(0.5 + rng.normal(0, 0.06, 120))
    _, p = prob_u8(0.5 + 0.5 * rng.random(4000))
    downs = b[::3]
    cases.append(beat_case("jittery", b, p, downs=downs))

    # 短い列
    for n in (0, 2, 4, 7, 12):
        b = np.arange(n) * 0.5 + 0.2
        cases.append(beat_case(f"short_{n}", b, np.full(200, 0.8), downs=b[::4] if n else np.array([]), duration=10.0))

    cls = []
    for m, nb, dr in [({"conf": 0.95, "unstable": 0.02, "local16_ms": 5.0, "downbeat_consistency": 0.95}, 3.0, 1.0),
                      ({"conf": 0.95, "unstable": 0.125, "local16_ms": 5.0, "downbeat_consistency": 0.845}, 20.4, -7.5),
                      ({"conf": 0.89, "unstable": 0.02, "local16_ms": 5.0, "downbeat_consistency": 0.95}, 0.0, 0.0),
                      ({"conf": 0.95, "unstable": 0.065, "local16_ms": 31.0, "downbeat_consistency": 0.95}, 0.0, 0.0),
                      ({"conf": 0.95, "unstable": 0.065, "local16_ms": 12.0, "downbeat_consistency": 0.5}, 15.5, 5.5)]:
        c, r = beats.classify(m, nb, dr)
        cls.append({"m": m, "no_beat_total": nb, "drift": dr, "class": c, "reasons": r})
    return {"cases": cases, "classify": cls}


# ---------------------------------------------------------------- 拍子

def strength_of(meters, noise=0.12, seed=0, hi=0.7, lo=0.15):
    rng = np.random.default_rng(seed)
    s = np.concatenate([[hi] + [lo] * (m - 1) for m in meters])
    return np.clip(s + rng.normal(0, noise, len(s)), 0, 1)


def compact_bars(bl):
    """[{beat, time, conf, meter?}] → 列ごとの配列"""
    out = {"beat": [b["beat"] for b in bl], "time": f64s([b["time"] for b in bl]), "conf": f64s([b["conf"] for b in bl])}
    if bl and "meter" in bl[0]:
        out["meter"] = [b["meter"] for b in bl]
    return out


def meter_case(name, s, beats_t=None, baseline=False):
    s = np.asarray(s, dtype=np.float32)        # パイプラインでは downbeat_strength が float32
    default_beats = beats_t is None
    if beats_t is None:
        beats_t = np.arange(len(s)) * 0.5 + 1.0   # Rust 側でも i*0.5+1.0 で作る
    beats_t = np.asarray(beats_t, dtype=np.float64)
    m, scores = bars.estimate_meter(s)
    path = bars.track_meter(s)
    bl = bars.bar_phases_varying(beats_t, s)
    c = {"name": name, "strength": f32s(s), "beats": None if default_beats else f64s(beats_t), "meter": m, "scores": scores,
         "path": [int(a) * 10 + int(b) for a, b in path], "varying": compact_bars(bl),
         "summary": {k: v for k, v in bars.meter_summary(bl).items()}, "bar_phases": compact_bars(bars.bar_phases(beats_t, s, m))}
    if baseline:
        c["baseline"] = f64s(bars.local_baseline(np.clip(s.astype(float), 1e-6, 1 - 1e-6)))
    return c


def meter_fixtures():
    cases = [meter_case("money", strength_of([7] * 12 + [4] * 16), baseline=True)]
    for seed in range(6):
        cases.append(meter_case(f"inserted3_{seed}", strength_of([4] * 12 + [3] + [4] * 12, seed=seed)))
    for seed in range(4):
        cases.append(meter_case(f"plain44_{seed}", strength_of([4] * 40, noise=0.18, seed=seed)))
    cases.append(meter_case("three", strength_of([3] * 32, seed=3), baseline=True))
    cases.append(meter_case("six", strength_of([6] * 20, seed=4, noise=0.05)))
    cases.append(meter_case("five", strength_of([5] * 24, seed=5, noise=0.08)))
    cases.append(meter_case("seven_four", strength_of([7] * 6 + [4] * 10, noise=0.05)))
    s = strength_of([4] * 10, noise=0.03)
    bt = np.arange(len(s)) * 0.5
    bt[20:] += 10.0
    cases.append(meter_case("gap", s, bt))
    rng = np.random.default_rng(9)
    cases.append(meter_case("flat_noise", rng.random(200) * 0.3, baseline=True))
    cases.append(meter_case("short", [0.9, 0.1], baseline=True))
    cases.append(meter_case("tiny3", [0.9, 0.1, 0.2]))
    return {"cases": cases}


def main():
    au = audio_fixtures()
    (OUT / "audio.json").write_text(json.dumps(au, ensure_ascii=False, separators=(",", ":")))
    bf = beat_fixtures()
    (OUT / "beats.json").write_text(json.dumps(bf, ensure_ascii=False, separators=(",", ":")))
    mf = meter_fixtures()
    (OUT / "meter.json").write_text(json.dumps(mf, ensure_ascii=False, separators=(",", ":")))
    for p in sorted(OUT.glob("*.json")):
        print(p.name, p.stat().st_size)


if __name__ == "__main__":
    main()
