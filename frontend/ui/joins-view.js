// つなぎ目タブ（design_phase2.md §6）。案A: 曲ごとの範囲選択。案B（つなぎ目エディタ）は editor-view.js。
// モックアップ mockup/phase2/ui_p2_A.html を本番用にしたもの。範囲・名前・つなぎはキューに保存する。
import { api } from '../api.js';
import * as queue from '../queue.js';
import { fitCanvas, sampleAt, smooth3, fillEnvelope, BAND_LAYERS } from './waveform.js';
import { joinOf, TEMPLATES } from '../engine/transition.js';
import { stemsOf, STEM_COLORS, STEM_NAMES, VOCAL_ON } from './vocals.js';

const SEC_COLOR = { '静か': '#2dd4bf', '中くらい': '#94a3b8', '盛り上がり': '#fb923c' };
const secKind = s => s.tags.find(t => SEC_COLOR[t]) ?? '中くらい';
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
export const fmt = (s, dec = 1) => {
  s = Math.max(0, s);
  const m = Math.floor(s / 60), r = s - m * 60;
  return `${m}:${r.toFixed(dec).padStart(dec ? 3 + dec : 2, '0')}`;
};

// ---- 曲のデータ（解析・波形・区間の候補）。一度読んだら使い回す ----
const store = new Map();
export async function trackData(id) {
  if (store.has(id)) return store.get(id);
  const p = (async () => {
    const [a, w, sec, cp] = await Promise.all([api.analysis(id), api.wave(id), api.sections(id),
      api.cutpoints(id).catch(() => ({ bars: [], outs: [], ins: [] }))]);
    const n = a.wave.length;
    // cut: カットインの切り替えポイントの候補と小節ごとの音量（design_cutin.md）
    return { a, barTimes: a.bars.map(b => b.time), sections: sec.sections, cut: cp,
             wave: { rate: a.wave.rate, low: w.subarray(0, n), mid: w.subarray(n, 2 * n), high: w.subarray(2 * n, 3 * n) } };
  })();
  store.set(id, p);
  try { return await p; } catch (e) { store.delete(id); throw e; }
}
/** 拍子・1 拍目の修正などで解析結果が変わったら呼ぶ。 */
export function forgetTrack(id) { store.delete(id); }

function nearestIdx(arr, v) {
  let lo = 0, hi = arr.length - 1;
  if (v <= arr[0]) return 0;
  if (v >= arr[hi]) return hi;
  while (hi - lo > 1) { const m = (lo + hi) >> 1; if (arr[m] <= v) lo = m; else hi = m; }
  return v - arr[lo] < arr[hi] - v ? lo : hi;
}
export function snapTime(d, t, mode) {
  if (mode === 'bar' && d.barTimes.length) return d.barTimes[nearestIdx(d.barTimes, t)];
  if (mode === 'beat' && d.a.beats.length) return d.a.beats[nearestIdx(d.a.beats, t)];
  return Math.round(t * 10) / 10;
}
export const barIdx = (d, t) => nearestIdx(d.barTimes, t);

/** 曲の [ta, tb] 秒を、実ピクセル x0..x1、縦 y0..y0+h に描く。 */
export function drawWave(g, d, ta, tb, x0, x1, y0, h, colors, alpha = 1) {
  const W = Math.max(1, Math.round(x1 - x0)), rate = d.wave.rate, mid = y0 + h / 2;
  g.save();
  g.globalAlpha = alpha;
  for (const [band, cssVar, sc] of BAND_LAYERS) {
    const arr = d.wave[band], v = new Float32Array(W);
    for (let x = 0; x < W; x++) v[x] = sampleAt(arr, (ta + (tb - ta) * x / W) * rate, (ta + (tb - ta) * (x + 1) / W) * rate);
    g.save(); g.translate(Math.round(x0), 0);
    fillEnvelope(g, smooth3(v), mid, (h / 2 - 2) / 255 * sc, colors[cssVar]);
    g.restore();
  }
  g.restore();
}

/** キューの曲の使う範囲（秒）。未指定なら曲全体。 */
export const useOf = (it, d) => ({ t0: it.use_start ?? 0, t1: it.use_end ?? d.a.duration });

export function initJoinsView({ root, colors, onPreview, onEditJoin }) {
  let cur = -1, mode = 'bar', drag = null;

  root.innerHTML = `
    <div class="p2card"><div class="p2title"><div class="btn" data-nav="-1">◀ 前の曲</div><b data-r="ttl"></b><span data-r="meta"></span>
      <div class="sp"></div><div class="btn" data-nav="1">次の曲 ▶</div></div></div>
    <div data-r="body"></div>`;
  const body = root.querySelector('[data-r=body]');
  root.querySelectorAll('[data-nav]').forEach(b => b.addEventListener('click', () => show(cur + +b.dataset.nav)));

  function emptyMsg(html) { body.innerHTML = `<div class="p2card jempty">${html}</div>`; }

  async function show(i) {
    const items = queue.getItems();
    if (!items.length) {
      cur = -1;
      root.querySelector('[data-r=ttl]').textContent = '';
      root.querySelector('[data-r=meta]').textContent = '';
      emptyMsg('ミックスキューが空です。<br>ライブラリで「＋キュー」を押して曲を入れると、ここで曲ごとに使う範囲を選べます。');
      return;
    }
    cur = Math.max(0, Math.min(items.length - 1, i));
    queue.setEditing({ item: cur });
    const it = items[cur], t = it.track;
    root.querySelector('[data-r=ttl]').textContent = `${cur + 1}. ${t.title}`;
    root.querySelector('[data-r=meta]').textContent = `${t.artist ?? ''}`;
    if (t.analysis_status !== 'done') { emptyMsg('この曲はまだ解析中です。解析が終わると範囲を選べます。'); return; }
    let d;
    try { d = await trackData(t.id); } catch { emptyMsg('解析結果を読み込めませんでした。'); return; }
    if (cur !== i && queue.getItems()[cur] !== it) return;
    root.querySelector('[data-r=meta]').textContent =
      `${t.artist ?? ''} ・ ${d.a.bpm_display?.toFixed(1) ?? d.a.bpm.toFixed(1)} BPM ・ ${d.a.key?.camelot ?? '—'} ・ ${d.a.meter}拍子 ・ ${fmt(d.a.duration, 0)}`;
    body.innerHTML = `
      <div class="p2card">
        <div class="lbl2" style="margin-bottom:6px">波形の上をドラッグして使う範囲を選びます。両端の取っ手で調整できます。</div>
        <canvas class="cv" data-r="ruler" style="height:26px"></canvas>
        <canvas class="cv" data-r="wave" style="height:260px;cursor:text"></canvas>
        <div class="stemlanes" data-r="lanes"></div>
        <div class="lbl2" style="margin:8px 0 4px">区間の候補（クリックで選択、Shift＋クリックで広げる）</div>
        <div class="secrow" data-r="secs"></div>
      </div>
      <div class="p2card selbar">
        <div><div class="lbl2">使う範囲</div><div class="big" data-r="range"></div></div>
        <div class="len" data-r="rlen"></div>
        <div><div class="lbl2">端の吸着</div><div class="seg2" data-r="snap">
          ${[['bar', '小節'], ['beat', '拍'], ['free', '自由（秒）']].map(([m, l]) => `<div class="${m === mode ? 'on' : ''}" data-m="${m}">${l}</div>`).join('')}</div></div>
        <div class="btn" data-pv="head">▶ 範囲の頭から</div><div class="btn" data-pv="tail">▶ 終わりの少し前から</div><div class="btn" data-r="reset">曲全体に戻す</div>
        <div><div class="lbl2">区間の名前</div><input data-r="name" placeholder="例: テーマ〜ギターソロ" value="${esc(it.use_label ?? '')}"></div>
      </div>
      <div class="joins"><div class="p2card join" data-r="jprev"></div><div class="p2card join" data-r="jnext"></div></div>`;
    bind(d);
    redraw(d);
  }

  function setUse(d, t0, t1) {
    const it = queue.getItems()[cur];
    if (t1 - t0 < 2) { t0 = 0; t1 = d.a.duration; }         // ほぼクリックだけなら曲全体に戻す
    const full = t0 <= 0.05 && t1 >= d.a.duration - 0.05;
    queue.patch(cur, { use_start: full ? null : t0, use_end: full ? null : t1 });
    return it;
  }

  function bind(d) {
    const cv = body.querySelector('[data-r=wave]');
    const timeAt = ev => { const r = cv.getBoundingClientRect(); return Math.max(0, Math.min(1, (ev.clientX - r.left) / r.width)) * d.a.duration; };
    const near = (ev, tm) => { const r = cv.getBoundingClientRect(); return Math.abs(timeAt(ev) - tm) * r.width / d.a.duration < 10; };
    cv.addEventListener('pointerdown', ev => {
      cv.setPointerCapture(ev.pointerId);
      const u = useOf(queue.getItems()[cur], d);
      if (near(ev, u.t0)) drag = { edge: 't0', u: { ...u } };
      else if (near(ev, u.t1)) drag = { edge: 't1', u: { ...u } };
      else { const s = snapTime(d, timeAt(ev), mode); drag = { anchor: s, u: { t0: s, t1: s } }; }
    });
    cv.addEventListener('pointermove', ev => {
      const u = useOf(queue.getItems()[cur], d);
      cv.style.cursor = drag?.edge || near(ev, u.t0) || near(ev, u.t1) ? 'ew-resize' : 'text';
      if (!drag) return;
      const s = snapTime(d, timeAt(ev), mode);
      if (drag.anchor !== undefined) drag.u = { t0: Math.min(drag.anchor, s), t1: Math.max(drag.anchor, s) };
      else { drag.u[drag.edge] = s; if (drag.u.t0 > drag.u.t1) [drag.u.t0, drag.u.t1] = [drag.u.t1, drag.u.t0]; }
      redraw(d, drag.u);
    });
    cv.addEventListener('pointerup', () => {
      if (!drag) return;
      setUse(d, drag.u.t0, drag.u.t1);
      drag = null;
      redraw(d);
    });
    body.querySelector('[data-r=secs]').addEventListener('click', e => {
      const b = e.target.closest('[data-i]');
      if (!b) return;
      const s = d.sections[+b.dataset.i], u = useOf(queue.getItems()[cur], d);
      const t0 = snapTime(d, s.t0, 'bar'), t1 = s.t1 >= d.a.duration - 0.05 ? d.a.duration : snapTime(d, s.t1, 'bar');
      if (e.shiftKey) setUse(d, Math.min(u.t0, t0), Math.max(u.t1, t1)); else setUse(d, t0, t1);
      redraw(d);
    });
    body.querySelector('[data-r=snap]').addEventListener('click', e => {
      const m = e.target.closest('[data-m]');
      if (!m) return;
      mode = m.dataset.m;
      body.querySelectorAll('[data-r=snap] [data-m]').forEach(x => x.classList.toggle('on', x === m));
    });
    body.querySelector('[data-r=reset]').addEventListener('click', () => { setUse(d, 0, d.a.duration); redraw(d); });
    body.querySelector('[data-r=name]').addEventListener('input', e => queue.patch(cur, { use_label: e.target.value.trim() || null }));
    body.querySelectorAll('[data-pv]').forEach(b => b.addEventListener('click', () => onPreview(b.dataset.pv, cur)));
  }

  function redraw(d, tmpUse) {
    const it = queue.getItems()[cur];
    const u = tmpUse ?? useOf(it, d);
    // パートの量（Phase 4 B1）。まだなら裏で分離が始まり、届いたら描き直す
    d.stems ??= stemsOf(it.track.id, r => { d.stems = r; if (queue.getItems()[cur]?.track.id === it.track.id) redraw(d); });
    drawMain(d, u);
    drawLanes(d);
    renderSections(d, u);
    body.querySelector('[data-r=range]').textContent = `${fmt(u.t0)} – ${fmt(u.t1)}`;
    body.querySelector('[data-r=rlen]').textContent = `${fmt(u.t1 - u.t0)} ・ ${barIdx(d, u.t1) - barIdx(d, u.t0)} 小節`;
    if (!tmpUse) renderJoins();
  }

  function drawMain(d, u) {
    const cv = body.querySelector('[data-r=wave]');
    const [g, W, H, dp] = fitCanvas(cv);
    const X = t => t / d.a.duration * W;
    g.fillStyle = '#050607'; g.fillRect(0, 0, W, H);
    for (const [s, e] of d.a.no_beat) { g.fillStyle = 'rgba(255,255,255,.05)'; g.fillRect(X(s), 0, X(e) - X(s), H); }
    // ボーカルの無い小節（繋ぎに向く）を薄い緑で（パートの分析が済んだ曲だけ）
    if (d.stems) d.stems.bars.forEach((r, i) => { if (r[0] < VOCAL_ON) { g.fillStyle = 'rgba(34,197,94,.10)'; g.fillRect(X(d.barTimes[i]), 0, X(d.barTimes[i + 1] ?? d.a.duration) - X(d.barTimes[i]), H); } });
    drawWave(g, d, 0, d.a.duration, 0, W, 0, H, colors, 0.28);
    const x0 = X(u.t0), x1 = X(u.t1);
    g.save(); g.beginPath(); g.rect(x0, 0, x1 - x0, H); g.clip();
    drawWave(g, d, 0, d.a.duration, 0, W, 0, H, colors, 1);
    g.restore();
    for (let i = 0; i < d.barTimes.length; i += 1) {
      g.fillStyle = i % 16 === 0 ? 'rgba(255,255,255,.22)' : i % 4 === 0 ? 'rgba(255,255,255,.07)' : 'rgba(0,0,0,0)';
      g.fillRect(Math.round(X(d.barTimes[i])), 0, dp, H);
    }
    g.strokeStyle = '#fff'; g.lineWidth = 2 * dp; g.strokeRect(x0, dp, x1 - x0, H - 2 * dp);
    for (const x of [x0, x1]) {
      g.fillStyle = '#fff'; g.fillRect(Math.round(x - 3 * dp), H / 2 - 22 * dp, 6 * dp, 44 * dp);
      g.fillStyle = '#0b0c0e'; g.fillRect(Math.round(x - dp), H / 2 - 12 * dp, 2 * dp, 24 * dp);
    }
    const [r, RW, RH] = fitCanvas(body.querySelector('[data-r=ruler]'));
    r.clearRect(0, 0, RW, RH);
    r.font = `${11 * dp}px Consolas`;
    const RX = t => t / d.a.duration * RW;
    const stepSec = d.a.duration > 480 ? 30 : 15;
    for (let s = 0; s <= d.a.duration; s += stepSec) {
      const x = Math.round(RX(s));
      r.fillStyle = 'rgba(255,255,255,.3)'; r.fillRect(x, RH - (s % 60 === 0 ? 10 : 5) * dp, dp, (s % 60 === 0 ? 10 : 5) * dp);
      if (s % (stepSec * 2) === 0) { r.fillStyle = colors['--dim']; r.fillText(fmt(s, 0), x + 3 * dp, 12 * dp); }
    }
    for (let i = 0; i < d.barTimes.length; i += 16) { r.fillStyle = colors['--faint']; r.fillText(`${i + 1}小節`, Math.round(RX(d.barTimes[i])) + 3 * dp, RH - 3 * dp); }
  }

  /** パートの量の帯（ボーカル・ドラム・ベース・その他）。 */
  function drawLanes(d) {
    const el = body.querySelector('[data-r=lanes]');
    if (!el) return;
    if (!d.stems) { el.innerHTML = '<div class="lbl2">パート（ボーカル・ドラム・ベース・その他）を分析しています…</div>'; return; }
    const src = d.stems.sources;
    if (el.childElementCount !== src.length) el.innerHTML = src.map(k => `<div class="lane"><span>${STEM_NAMES[k] ?? k}</span><canvas></canvas></div>`).join('');
    el.querySelectorAll('canvas').forEach((cv, c) => {
      const [g, W, H] = fitCanvas(cv);
      g.fillStyle = '#050607'; g.fillRect(0, 0, W, H);
      d.stems.bars.forEach((r, i) => {
        const x0 = d.barTimes[i] / d.a.duration * W, x1 = (d.barTimes[i + 1] ?? d.a.duration) / d.a.duration * W;
        g.globalAlpha = Math.min(1, r[c] * 1.4); g.fillStyle = STEM_COLORS[src[c]]; g.fillRect(x0, 1, x1 - x0 + 0.5, H - 2);
      });
      g.globalAlpha = 1;
    });
  }

  function renderSections(d, u) {
    body.querySelector('[data-r=secs]').innerHTML = d.sections.map((s, i) => {
      const k = secKind(s), inside = s.t0 >= u.t0 - 0.3 && s.t1 <= u.t1 + 0.3;
      const tags = s.tags.filter(x => x !== k).join('・');
      return `<div class="secblk ${inside ? 'sel' : ''}" data-i="${i}" style="left:${s.t0 / d.a.duration * 100}%;width:calc(${(s.t1 - s.t0) / d.a.duration * 100}% - 2px);background:${SEC_COLOR[k]}"
        title="${fmt(s.t0)}–${fmt(s.t1)}  ${s.bars}小節  ${esc(s.tags.join('・'))}"><b>${k}</b>${tags ? ' ' + esc(tags) : ''}<small>${fmt(s.t0, 0)} ・ ${s.bars}小節</small></div>`;
    }).join('');
  }

  async function renderJoins() {
    const items = queue.getItems();
    for (const [key, i] of [['jprev', cur - 1], ['jnext', cur]]) {
      const el = body.querySelector(`[data-r=${key}]`);
      if (!el) return;
      if (i < 0 || i + 1 >= items.length) {
        el.innerHTML = `<h4>${i < 0 ? '前の曲はありません（ミックスの最初の曲）' : '次の曲はありません（ミックスの最後の曲）'}</h4>`;
        continue;
      }
      const A = items[i], B = items[i + 1], j = joinOf(A);
      el.innerHTML = `<h4>${i === cur ? '次の曲' : '前の曲'}とのつなぎ <span>${esc(A.track.title)} → ${esc(B.track.title)}</span></h4>
        <canvas class="cv" style="height:84px"></canvas>
        <div class="row2"><div class="lbl2">重ねる長さ</div><div class="seg2">${[[8, '短め 8小節'], [16, '標準 16小節'], [32, '長め 32小節']].map(([v, l]) =>
          `<div class="${v === j.bars ? 'on' : ''}" data-n="${v}">${l}</div>`).join('')}</div>
          <div style="flex:1"></div><div class="btn" data-play>▶ つなぎ目だけ試聴</div><div class="btn" data-edit>細かく直す</div></div>
        <div class="lbl2" style="margin-top:6px">繋ぎ方: ${esc(TEMPLATES[j.tpl].label)}</div>`;
      el.querySelectorAll('[data-n]').forEach(x => x.addEventListener('click', () => {
        queue.patch(i, { join_bars: +x.dataset.n, join_tpl: j.tpl === 'echo' || j.tpl === 'cut' ? 'long' : j.tpl });
        renderJoins();
      }));
      el.querySelector('[data-play]').addEventListener('click', () => onPreview('join', i));
      el.querySelector('[data-edit]').addEventListener('click', () => onEditJoin(i));
      if (A.track.analysis_status !== 'done' || B.track.analysis_status !== 'done') continue;
      Promise.all([trackData(A.track.id), trackData(B.track.id)]).then(([da, db]) => drawJoinMini(el.querySelector('canvas'), A, B, da, db, joinOf(A).bars));
    }
  }

  function drawJoinMini(cv, A, B, da, db, n) {
    const [g, W, H, dp] = fitCanvas(cv);
    g.fillStyle = '#050607'; g.fillRect(0, 0, W, H);
    const ua = useOf(A, da), ub = useOf(B, db), N = Math.max(n, 1);
    const ai = barIdx(da, ua.t1), aStart = da.barTimes[Math.max(0, ai - N - 8)], aOvl = da.barTimes[Math.max(0, ai - N)];
    const bi = barIdx(db, ub.t0), bEnd = db.barTimes[Math.min(db.barTimes.length - 1, bi + N + 8)], bOvl = db.barTimes[Math.min(db.barTimes.length - 1, bi + N)];
    const span = (ua.t1 - aStart) + (bEnd - bOvl), pps = W / span;
    const ax1 = (ua.t1 - aStart) * pps, ovX0 = (aOvl - aStart) * pps, lh = (H - 6 * dp) / 2;
    g.fillStyle = 'rgba(34,197,94,.16)'; g.fillRect(ovX0, 0, ax1 - ovX0, H);
    drawWave(g, da, aStart, ua.t1, 0, ax1, 0, lh, colors, 0.95);
    drawWave(g, db, ub.t0, bEnd, ovX0, W, lh + 6 * dp, lh, colors, 0.95);
    g.fillStyle = colors['--deck-a']; g.fillRect(0, 0, 3 * dp, lh);
    g.fillStyle = colors['--deck-b']; g.fillRect(ovX0, lh + 6 * dp, 3 * dp, lh);
    g.fillStyle = '#e8e8e8'; g.font = `${11 * dp}px "Hiragino Sans", "Yu Gothic UI", sans-serif`;
    g.fillText(n ? `重なり ${n} 小節` : '重ねずに切り替え', ovX0 + 6 * dp, H / 2 + 4 * dp);
  }

  addEventListener('resize', () => { if (cur >= 0 && root.offsetParent) show(cur); });
  return { show, get current() { return cur; } };
}
