"""Python 版（djmix/export/render.py）の結果を、Rust 版の一致の試験（tests/export_parity.rs）用に書き出す。

使い方（リポジトリの直下で）:
    PYTHONPATH=. <numpy・scipy の入った python> native/tests/fixtures/export/gen_fixtures.py

入力の音は両方の言語で同じ式から作る（gen_input）。出力は間引いて（every 個おきに）保存し、ファイルを小さくする。
"""
from __future__ import annotations

import json
import math
from pathlib import Path

import numpy as np

from djmix.export import render

OUT = Path(__file__).resolve().parent
EVERY = 97


def gen_input(n: int, seed: int, amp: float) -> np.ndarray:
    """(n, 2) の決まった音: 3 つの正弦波 + 擬似乱数（線形合同法）。Rust 側の gen_input と同じ式。"""
    x = np.zeros((n, 2))
    s = seed
    for i in range(n):
        s = (s * 1103515245 + 12345) % 2147483648
        r = s / 2147483648.0 - 0.5
        for c in range(2):
            v = (0.5 * math.sin(i * 0.0123 * (c + 1)) + 0.3 * math.sin(i * 0.31 + c) + 0.2 * math.sin(i * 1.7 + 2 * c)) + 0.4 * r
            x[i, c] = amp * v * (1.0 + 0.8 * math.sin(i * 0.0007))
    return x


def sample(y: np.ndarray) -> dict:
    y = np.asarray(y, dtype=np.float64)
    return {"n": int(len(y)), "every": EVERY, "pick": y[::EVERY].reshape(-1).tolist(),
            "sum": float(y.sum()), "sumsq": float((y ** 2).sum()), "absmax": float(np.abs(y).max()) if len(y) else 0.0}


def mixer_cases() -> dict:
    S = render.STEP
    out = {}
    # 係数
    out["rbj"] = {"lp": render._rbj("lowpass", 250.0, render.Q_B).tolist(), "hp": render._rbj("highpass", 1234.5, render.Q_F).tolist(),
                  "ap": render._rbj("allpass", 2500.0, render.Q_B).tolist(), "lp_nyq": render._rbj("lowpass", 30000.0, 1.0).tolist()}
    out["hp_freq"] = [render.hp_freq(v) for v in (0.0, 0.01, 0.05, 0.1, 0.3, 0.5, 0.9, 1.0)]
    xs = np.linspace(0, 1, 11)
    out["xfade"] = {c: [a.tolist() for a in render.xfade_gains(xs, c)] for c in ("smooth", "linear", "cut")}
    # Channel: 3 回に分けて（状態を引き継ぐ）。LOW を動かし、途中からハイパスを掛ける
    ch = render.Channel()
    steps = 40
    x = gen_input(3 * steps * S, 7, 0.4)
    ys = []
    for c in range(3):
        k = np.arange(c * steps, (c + 1) * steps)
        low = np.clip(1 - k / 80.0, 0, 1)
        hp = np.where(k >= 50, (k - 50) / 70.0, 0.0)
        ys.append(ch.process(x[c * steps * S:(c + 1) * steps * S], 1.3, low, hp))
    out["channel"] = sample(np.concatenate(ys))
    # Echo: 送りを途中で止め、遅延を変える。送り終わった後の余韻も
    e = render.Echo()
    x = gen_input(4 * steps * S, 11, 0.3)
    ys = []
    for c in range(4):
        k = np.arange(c * steps, (c + 1) * steps)
        send = np.where(k < 70, 1.0, 0.0)
        delay = 0.3 + 0.2 * np.sin(k / 9.0)
        src = x[c * steps * S:(c + 1) * steps * S] * np.repeat(send, S)[:, None]
        ys.append(e.process(src, delay))
    out["echo"] = sample(np.concatenate(ys))
    # Compressor・Limiter（大きい音で）
    comp = render.Compressor()
    x = gen_input(3 * steps * S, 3, 1.6)
    out["compressor"] = sample(np.concatenate([comp.process(x[c * steps * S:(c + 1) * steps * S]) for c in range(3)]))
    lim = render.Limiter(-0.3)
    parts = [lim.process(x[c * steps * S:(c + 1) * steps * S]) for c in range(3)] + [lim.flush()]
    out["limiter"] = sample(np.concatenate(parts))
    lim = render.Limiter(-1.0)
    parts = [lim.process(x[c * steps * S:(c + 1) * steps * S] * 0.5) for c in range(3)] + [lim.flush()]
    out["limiter_mp3"] = sample(np.concatenate(parts))
    return out


# ---------------------------------------------------------------- 操作の表
def tri(x: float) -> float:
    """三角波（−1..1。四則と floor だけなので、どの環境でも同じ値になる）"""
    f = x - math.floor(x)
    return 4.0 * f - 1.0 if f < 0.5 else 3.0 - 4.0 * f


def beats_of(bpm: float, seconds: float, first: float, wobble: float, per: float) -> list[float]:
    """拍の時刻（Rust 側の beats_of と同じ式）"""
    out, b, j = [], first, 0
    while b < seconds - 0.3:
        out.append(b)
        b += 60.0 / bpm * (1.0 + wobble * tri(j / per))
        j += 1
    return out


def track_spec(bpm, seconds, first=0.3, wobble=0.0, per=37.0, t0=5.0, t1=None, lufs=-14.0, item=None, meter=4,
               no_beat=None, cut_db=None, parts_ok=False):
    beats = beats_of(bpm, seconds, first, wobble, per)
    bars = beats[::meter]
    cut_bars = [{"t": t, "db": -12.0 + 6.0 * tri(i * 0.13) + (cut_db or 0)} for i, t in enumerate(bars)] if cut_db is not None else []
    spec = {"bpm": bpm, "seconds": seconds, "first": first, "wobble": wobble, "per": per, "meter": meter, "cut_db": cut_db}
    return {"spec": spec, "beats": beats, "bars": bars, "no_beat": no_beat or [], "t0": t0, "t1": t1 if t1 is not None else seconds - 2,
            "lufs": lufs, "item": item or {}, "cut_bars": cut_bars, "parts_ok": parts_ok}


def make_tracks(specs):
    out = []
    for i, s in enumerate(specs):
        a = {"beats": s["beats"], "meter": s["spec"]["meter"], "bars": [{"beat": k * s["spec"]["meter"], "time": t} for k, t in enumerate(s["bars"])],
             "no_beat": s["no_beat"], "duration": s["beats"][-1] + 1, "lufs": s["lufs"]}
        it = {"track_id": i + 1, "path": "", **s["item"]}
        t = render.Track(i, it, a, render.BeatMap(a), s["t0"], s["t1"], s["lufs"], f"t{i}", f"a{i}", album=f"al{i}" if i % 2 else "",
                         cut_bars=s["cut_bars"])
        t.parts_ok = s["parts_ok"]
        out.append(t)
    return out


PLAN_CASES = {
    "long_sync": ([track_spec(122, 200, wobble=0.01, t0=10, t1=150, item={"join_tpl": "long", "join_bars": 16}),
                   track_spec(126, 220, wobble=0.015, per=23, t0=6, t1=200)], {"tempo_range": 0.08, "tempo_return": True}),
    "ramp_loop_shift": ([track_spec(120, 200, t0=10, t1=120, item={"join_tpl": "long", "join_bars": 16,
                                                                  "join_params": {"tempo": "ramp", "loop": 4, "shift": 1}}),
                         track_spec(126, 200, t0=5, t1=160)], {"tempo_range": 0.08}),
    "chain3": ([track_spec(124, 180, wobble=0.01, t0=8, t1=120, item={"join_tpl": "bass", "join_bars": 8}),
                track_spec(128, 180, wobble=0.012, per=17, t0=4, t1=110, item={"join_tpl": "filter", "join_bars": 8}),
                track_spec(120, 160, t0=3, t1=100, item={"join_tpl": "short", "join_params": {"swapAt": 0.25, "xfStart": 0.2, "xfEnd": 0.7}}),
                track_spec(118, 160, t0=2, t1=60)], {"tempo_range": 0.06, "tempo_return": True}),
    "echo_and_cut": ([track_spec(120, 150, t0=10, t1=100, item={"join_tpl": "echo"}),
                      track_spec(121, 150, t0=5, t1=90, cut_db=0, item={"join_tpl": "cut"}),
                      track_spec(119, 150, t0=7, t1=70, cut_db=-5, lufs=-10.0)], {"tempo_range": 0.08}),
    "cut_fixed_trim": ([track_spec(120, 150, t0=10, t1=100, cut_db=3, item={"join_tpl": "cut", "join_params": {"cutTrim": -2.5}}),
                        track_spec(140, 150, t0=5, t1=60, cut_db=0)], {"tempo_range": 0.08}),
    "mismatch_echo": ([track_spec(100, 150, t0=10, t1=100, item={"join_tpl": "long"}),
                       track_spec(150, 150, t0=5, t1=60)], {"tempo_range": 0.08, "tempo_return": False}),
    "nobeat_fallback": ([track_spec(120, 150, t0=10, t1=100, item={"join_tpl": "short"}),
                         track_spec(120, 150, t0=5, t1=60, no_beat=[[4.0, 9.0]])], {"tempo_range": 0.08}),
    "parts_swap": ([track_spec(120, 150, t0=10, t1=100, parts_ok=True, item={"join_tpl": "long", "join_bars": 8, "join_params": {"parts": "swap"}}),
                    track_spec(123, 150, t0=5, t1=80, parts_ok=True)], {"tempo_range": 0.08}),
    "half_time": ([track_spec(70, 150, t0=10, t1=100, item={"join_tpl": "short"}),
                   track_spec(138, 150, t0=5, t1=60)], {"tempo_range": 0.1}),
    "single": ([track_spec(120, 60, t0=3, t1=40)], {}),
}


def plan_cases() -> dict:
    out = {}
    for name, (specs, st) in PLAN_CASES.items():
        tracks = make_tracks(specs)
        p = render.build_plan(tracks, st)
        E = 211
        cols = {}
        w = (np.arange(p.n) % 7 + 1).astype(np.float64)
        for k in ("track", "pos", "low", "hp", "echo", "echo_delay", "pm", "jg", "pg"):
            for d in range(2):
                a = np.asarray(getattr(p, k)[d], dtype=np.float64)
                a2 = a.reshape(p.n, -1)
                cols[f"{k}{d}"] = {"pick": a[::E].reshape(-1).tolist() if k != "pg" else a2[::E].reshape(-1).tolist(),
                                   "sum": float(a2.sum()), "wsum": float((a2 * w[:, None]).sum())}
        xf = np.asarray(p.xf)
        cols["xf"] = {"pick": xf[::E].tolist(), "sum": float(xf.sum()), "wsum": float((xf * w).sum())}
        nt = [{k: v for k, v in sp.items() if k not in ("beats", "bars", "cut_bars")} for sp in specs]
        out[name] = {"tracks": nt, "settings": st, "n": p.n, "every": E, "cols": cols, "cuts": p.cuts, "marks": p.marks,
                     "shift": [t.shift for t in tracks]}
    return out


if __name__ == "__main__":
    (OUT / "mixer_py.json").write_text(json.dumps(mixer_cases()))
    (OUT / "plan_py.json").write_text(json.dumps(plan_cases()))
    for f in ("mixer_py.json", "plan_py.json"):
        print(f, (OUT / f).stat().st_size)
