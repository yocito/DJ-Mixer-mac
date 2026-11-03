"""おまかせ（planner）の Rust 版の照合用の期待値を、Windows 版（Python）の本体で作る。

合成のライブラリ（解析キャッシュ analysis.json / bars.f32 / stems.f32 と DB）は、Rust 側（synth.rs）と同じ式・同じ乱数で作る
（整数の乱数 splitmix64 と四則演算だけなので、両方の言語でビット単位まで同じ値になる）。
コミットするのは、この台本・synth.rs・Python の結果（expected_*.json.gz）だけ。

使い方（リポジトリの直下で）:
    DJMIX_DATA=$(mktemp -d) PYTHONPATH=. python native/tests/fixtures/planner/gen.py small native/tests/fixtures/planner/expected_small.json.gz
    DJMIX_DATA=$(mktemp -d) PYTHONPATH=. python native/tests/fixtures/planner/gen.py large native/tests/fixtures/planner/expected_large.json.gz
"""
from __future__ import annotations

import gzip
import json
import sys
import time

import numpy as np

MASK = (1 << 64) - 1
KEYS = [f"{n}{l}" for n in range(1, 13) for l in "AB"]
GENRES = ["Jazz", "Smooth Jazz", "Rock", "Pop", "Soul", "J-Pop", "House", None, "Bossa Nova", "Classical", "Funk", "Soundtrack"]
LEVELS = [0.12, 0.3, 0.55, 0.8]


class Rng:
    """splitmix64（synth.rs と同じ）"""

    def __init__(self, seed: int):
        self.s = seed & MASK

    def next(self) -> int:
        self.s = (self.s + 0x9E3779B97F4A7C15) & MASK
        z = self.s
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & MASK
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & MASK
        return z ^ (z >> 31)

    def u(self) -> float:
        return (self.next() >> 11) * (1.0 / 9007199254740992.0)

    def int(self, lo: int, hi: int) -> int:
        return lo + self.next() % (hi - lo + 1)


def track(k: int, lib: int):
    """曲 k の解析結果・小節ごとの特徴量・パートの量（無ければ None）・タグ。"""
    r = Rng(k * 7919 + lib * 104729 + 17)
    tk = r.int(0, 9)
    bar_ms = r.int(1700, 2400)
    if tk == 0:
        bar_ms *= 2
    elif tk == 1:
        bar_ms = bar_ms * 2 // 3
    bpm = 240000.0 / bar_ms
    nbars = r.int(60, 180)
    if k % 31 == 0:
        nbars = 12                                   # 小節が少なすぎる（要約を作らない）
    off = r.int(500, 25000) if r.int(0, 3) == 0 else r.int(0, 400)
    tail = r.int(0, 9000)
    times = [(off + i * bar_ms) / 1000.0 for i in range(nbars)]
    duration = (off + nbars * bar_ms + tail) / 1000.0
    nb = []
    kind = r.int(0, 6)
    if kind in (1, 4):
        j = r.int(2, 12)
        nb.append([0.0, (off + j * bar_ms - 300) / 1000.0])
    if kind in (2, 4):
        j = nbars - r.int(2, 14)
        nb.append([(off + j * bar_ms) / 1000.0, duration])
    if kind == 3:
        j = r.int(20, max(21, nbars - 30))
        e = j + r.int(2, 10)
        nb.append([(off + j * bar_ms) / 1000.0, (off + e * bar_ms) / 1000.0])
    key = KEYS[r.int(0, 23)]
    head = key if r.int(0, 3) < 2 else KEYS[r.int(0, 23)]
    tailk = key if r.int(0, 3) < 2 else KEYS[r.int(0, 23)]
    extra = {"centroid": 700.0 + r.int(0, 25000) / 10.0, "low_ratio": r.int(500, 4500) / 10000.0,
             "onset_rate": r.int(800, 7000) / 1000.0}
    seclen = r.int(8, 24)
    phase = r.int(0, 3)
    harm_base = r.int(2, 60) / 100.0
    chroma_base = [r.int(0, 500) / 1000.0 for _ in range(12)]
    feats = []
    for i in range(nbars):
        lvl = LEVELS[(i // seclen + phase) % 4]
        row = [lvl + (r.u() - 0.5) * 0.06]
        row.append(lvl * 0.7 + r.u() * 0.1)
        for c in range(12):
            row.append(chroma_base[c] + r.u() * 0.15)
        row.append(harm_base + r.u() * 0.1)
        feats.append(row)
    stems = None
    if r.int(0, 2) == 0:
        vstyle = r.int(0, 2)
        stems = []
        for i in range(nbars):
            lvl = LEVELS[(i // seclen + phase) % 4]
            if vstyle == 0:
                sing = lvl >= 0.3
            elif vstyle == 1:
                sing = (i // 8) % 2 == 0
            else:
                sing = r.u() < 0.5
            v = 0.5 + r.u() * 0.45 if sing else r.u() * 0.2
            row = [v]
            for _ in range(3):
                row.append(r.u())
            stems.append(row)
    a = {"duration": duration, "sample_rate": 44100, "bpm": bpm, "beats": [], "meter": 4,
         "bars": [{"beat": 4 * i, "time": t, "conf": 0.9} for i, t in enumerate(times)],
         "no_beat": nb, "lufs": -14.0,
         "key": {"key": None, "camelot": key, "corr": 0.5},
         "key_head30": {"key": None, "camelot": head, "corr": 0.5},
         "key_tail30": {"key": None, "camelot": tailk, "corr": 0.5},
         "beat_class": "A"}
    if k % 37 != 0:
        a["extra"] = extra                            # extra の無い古い解析（要約を作らない）
    g = r.int(0, len(GENRES) - 1)
    artist = f"Artist {k % 17}"
    if k % 17 == 3 and k % 2 == 0:
        artist = "artist 3 "                          # 表記の揺れ（大文字小文字・空白）
    if k % 23 == 0:
        artist = None
    meta = {"title": f"Track {k:03d}", "artist": artist,
            "album": f"Album {chr(65 + k % 9)}" if k % 5 != 0 else None, "genre": GENRES[g],
            "status": "none" if k % 29 == 0 else "done"}
    return a, feats, stems, meta


def prefs_of(k: int):
    """曲 k の ★・再生回数・iTunes の ★（無ければ None）、最近かけたか、プレイリスト 1 に入るか。"""
    rating = 1 if k % 11 == 0 else 5 if k % 7 == 0 else 4 if k % 5 == 0 else 2 if k % 13 == 0 else 0
    plays = (k * 7) % 23 if k % 3 == 0 else 0
    it = 3 if k % 19 == 0 else 0
    return rating, plays, it, k % 10 == 4, k % 4 == 1


def build(n: int, lib: int):
    from djmix import db
    from djmix.analysis import cache, stems as stems_mod
    c = db.connect()
    now = time.time()
    c.execute("INSERT INTO itunes_playlists(id, pid, name) VALUES(1, 'P1', 'PL1')")
    pos = 0
    for k in range(1, n + 1):
        a, feats, st, meta = track(k, lib)
        c.execute("INSERT INTO tracks(id, path, folder, title, artist, album, genre, analysis_status) VALUES(?,?,?,?,?,?,?,?)",
                  (k, f"/music/t{k}.mp3", "/music", meta["title"], meta["artist"], meta["album"], meta["genre"], meta["status"]))
        cache.write(k, a, np.zeros((3, 10), np.uint8), np.array(feats, dtype=np.float32), np.zeros(10, np.float16))
        if st is not None:
            stems_mod.save(k, np.array(st, dtype=np.float32))
        rating, plays, it, recent, inpl = prefs_of(k)
        if rating or plays or it:
            c.execute("INSERT INTO track_prefs(track_id, rating, play_count, itunes_rating) VALUES(?,?,?,?)", (k, rating, plays, it))
        if recent:
            c.execute("INSERT INTO plays(track_id, at, source) VALUES(?,?,?)", (k, now - 3 * 86400, "deck"))
        if inpl:
            c.execute("INSERT INTO itunes_playlist_items(playlist_id, pos, track_id) VALUES(1,?,?)", (pos, k))
            pos += 1
    c.commit()


def scenarios_small(ids: list[int]):
    S = "settings"
    s1, s2, s3 = ids[0], ids[7], ids[20]
    q = [ids[3], ids[9], ids[14], ids[2], ids[25], ids[30], ids[11], ids[3], 9999]
    out = [
        ("candidate_facets", {}),
        ("plan_library", dict(seed=s1, groups=[], minutes=30.0, flow="up", prefs=S)),
        ("plan_library", dict(seed=s2, groups=["ジャズ・フュージョン", "ポップス", "ロック"], minutes=20.0, flow="peak",
                              avoid=[ids[5], ids[6]], prefs=S)),
        ("plan_library", dict(seed=s1, groups=[], minutes=25.0, flow="custom", curve=[0.2, 0.9, 0.3], prefs=S)),
        ("plan_library", dict(seed=s3, groups=[], minutes=20.0, flow="down", prefs=S, join_mode="cut")),
        ("plan_library", dict(seed=s1, groups=[], minutes=20.0, flow="keep", join_mode="mix",
                              prefs={"skip_rating1": False, "avoid_recent_days": 0, "prefer_rating": False, "prefer_plays": False,
                                     "auto_parts": False})),
        ("plan_library", dict(seed=s2, groups=[], minutes=12.0, flow="up", prefs=S, track_len=60.0, track_pos="auto")),
        ("plan_library", dict(seed=s1, groups=[], minutes=30.0, flow="peak", prefs=S, track_len=180.0, track_pos="peak")),
        ("plan_library", dict(seed=s3, groups=[], minutes=20.0, flow="up", prefs=S, playlists=[1])),
        ("plan_library", dict(seed=s2, groups=[], minutes=20.0, flow="keep", prefs=S, track_pos="tail")),
        ("plan_library", dict(seed=s1, groups=[], minutes=20.0, flow="up", prefs=S, track_pos="intro", track_len=120.0)),
        ("plan_library", dict(seed=s3, groups=[], minutes=25.0, flow="down", prefs=S, track_pos="full")),
        ("plan_library", dict(seed=s1, groups=[], minutes=20.0, flow="sideways", prefs=S)),
        ("plan_library", dict(seed=99999, groups=[], minutes=20.0, flow="up", prefs=S)),
        ("plan_library", dict(seed=s1, groups=["クラシック"], minutes=20.0, flow="up", prefs=S)),
        ("plan_albums", dict(albums=["Album A", "Album B", "Album C", "Album D"], minutes=30.0, flow="up", prefs=S)),
        ("plan_albums", dict(albums=["Album B", "Album E", "Album F", "Album Z"], minutes=20.0, flow="keep", seed=ids[1], prefs=S)),
        ("plan_albums", dict(albums=["Album A", "Album C", "Album G"], minutes=5.0, flow="down", use_all=True, prefs=S)),
        ("plan_albums", dict(albums=["Album A", "Album C", "Album G", "Album H"], minutes=600.0, flow="custom", curve=[0.8, 0.2],
                             prefs=S, join_mode="cut")),
        ("plan_albums", dict(albums=["Album D", "Album E", "Album I"], minutes=15.0, flow="peak", prefs=S, track_len=45.0)),
        ("plan_albums", dict(albums=["Album A", "Album B"], minutes=30.0, flow="up", prefs=S)),
        ("plan_albums", dict(albums=["Album A", "Album B", "Album Z"], minutes=30.0, flow="up", prefs=S)),
        ("plan_queue", dict(ids=q)),
        ("plan_queue", dict(ids=q[:6], join_mode="cut")),
        ("plan_queue", dict(ids=[ids[4], ids[8], ids[12], ids[16]], join_mode="mix")),
        ("plan_queue", dict(ids=[ids[4], 9999])),
        ("next_candidates", dict(prev=s1, nxt=None, groups=[], exclude=[ids[1], ids[2]], n=8, prefs=S)),
        ("next_candidates", dict(prev=s2, nxt=ids[12], groups=[], exclude=[], n=6, prefs=S)),
        ("next_candidates", dict(prev=None, nxt=ids[12], groups=["ロック"], exclude=[], n=5, prefs=S)),
        ("next_candidates", dict(prev=s3, nxt=None, groups=[], exclude=[], n=20, prefs=S, artists=["ARTIST 3", "artist 5 "])),
        ("next_candidates", dict(prev=99999, nxt=None, groups=[], exclude=[], n=5, prefs=S)),
        ("rejoin", dict(spec=[{"id": ids[0]}, {"id": ids[5], "len": 60.0, "pos": "auto"}, {"id": ids[9], "pos": "peak", "len": 120.0, "jm": "cut"},
                              {"id": ids[13], "pos": "custom", "t0": 31.0, "t1": 150.0}, {"id": ids[18], "pos": "tail", "jm": "mix"},
                              {"id": ids[22], "pos": "full"}, {"id": 9999}, {"id": ids[26], "pos": "intro", "len": 90.0}],
                        prefs=S)),
        ("rejoin", dict(spec=[{"id": ids[2]}, {"id": ids[2]}, {"id": ids[6], "jm": "cut"}, {"id": ids[10]}], prefs=S, join_mode="cut")),
        ("rejoin", dict(spec=[{"id": 9999}], prefs=S)),
    ]
    for k in (0, 3, 7, 11, 20, 33):
        for ln in (None, 30.0, 60.0, 180.0, 240.0):
            out.append(("segment_choices", dict(tid=ids[k], length=ln)))
    out.append(("segment_choices", dict(tid=99999, length=None)))
    out.append(("plan_library", dict(seed=s1, groups=[], minutes=30.0, flow="up", prefs=S, join_mode="cut")))
    return out


def scenarios_large(ids: list[int]):
    S = "settings"
    return [
        ("plan_library", dict(seed=ids[0], groups=[], minutes=40.0, flow="up", prefs=S)),
        ("plan_library", dict(seed=ids[100], groups=[], minutes=60.0, flow="peak", prefs=S, join_mode="cut")),
        ("next_candidates", dict(prev=ids[50], nxt=ids[51], groups=[], exclude=[], n=10, prefs=S)),
    ]


def run(sc):
    from djmix import settings
    from djmix.planner import engine
    out = []
    for fn, args in sc:
        a = dict(args)
        p = a.pop("prefs", None)
        prefs = settings.load() if p == "settings" else p
        try:
            if fn == "candidate_facets":
                r = engine.candidate_facets()
            elif fn == "plan_library":
                r = engine.plan_library(a["seed"], a["groups"], a["minutes"], a["flow"], a.get("avoid", []), None, a.get("curve"), prefs,
                                        a.get("playlists"), a.get("track_len"), a.get("track_pos", "auto"), a.get("join_mode", "auto"))
            elif fn == "plan_albums":
                r = engine.plan_albums(a["albums"], a["minutes"], a["flow"], a.get("seed"), a.get("use_all", False), a.get("avoid", []),
                                       None, a.get("curve"), prefs, a.get("track_len"), a.get("track_pos", "auto"),
                                       a.get("join_mode", "auto"))
            elif fn == "plan_queue":
                r = engine.plan_queue(a["ids"], None, a.get("join_mode", "auto"))
            elif fn == "next_candidates":
                r = engine.next_candidates(a["prev"], a["nxt"], a["groups"], a["exclude"], a["n"], None, prefs, a.get("artists"))
            elif fn == "rejoin":
                spec = [{"id": s["id"], "len": s.get("len"), "pos": s.get("pos") or "auto", "t0": s.get("t0"), "t1": s.get("t1"),
                         "jm": s.get("jm")} for s in a["spec"]]
                r = engine.rejoin(spec, None, prefs, a.get("join_mode", "auto"))
            elif fn == "segment_choices":
                r = engine.segment_choices(a["tid"], a["length"])
            else:
                raise RuntimeError(fn)
            if isinstance(r, dict):
                r.pop("sec", None)
            out.append({"fn": fn, "args": args, "result": r})
        except (ValueError, KeyError) as e:
            out.append({"fn": fn, "args": args, "error": str(e)})
    return out


def main():
    which, path = sys.argv[1], sys.argv[2]
    n, lib = (64, 1) if which == "small" else (1000, 2)
    build(n, lib)
    from djmix import db
    from djmix.planner import desc
    desc.backfill()
    c = db.connect()
    descs = {str(r[0]): json.loads(r[1]) for r in c.execute("SELECT track_id, data FROM track_desc ORDER BY track_id")}
    ids = [int(k) for k in descs]
    sc = scenarios_small(ids) if which == "small" else scenarios_large(ids)
    t0 = time.time()
    res = run(sc)
    print(f"{len(res)} scenarios in {time.time() - t0:.2f}s", file=sys.stderr)
    out = {"n": n, "lib": lib, "scenarios": res}
    if which == "small":
        out["descs"] = descs
    data = json.dumps(out, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    with gzip.GzipFile(path, "wb", compresslevel=9, mtime=0) as f:
        f.write(data)


if __name__ == "__main__":
    main()
