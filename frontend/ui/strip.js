// 再生ストリップ（上部固定）。曲情報行・拡大波形・全体波形（design_phase1.md §5.1、§5.4）。
// 拡大波形の横軸: P1-4 では時間軸。SYNC 中の拍軸表示は P1-7 で追加する。
import { fitCanvas, drawOverview, sampleAt, smooth3, fillEnvelope, BAND_LAYERS } from './waveform.js';

const fmt = s => { s = Math.max(0, s); return `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, '0')}`; };
const ZOOM_SEC = 5;     // 時間軸表示のとき、中央から左右に何秒見せるか

export function renderFlags(deck, { master = false, sync = false, keylock = true } = {}) {
  const f = (name, on) => `<div class="flag ${on ? 'on' : ''}">${name}</div>`;
  document.getElementById(`flags${deck}`).innerHTML = f('MASTER', master) + f('SYNC', sync) + f('KEYLOCK', keylock);
}

export function setTrackInfo(deck, t) {
  const $ = id => document.getElementById(id + deck);
  if (!t) {
    $('t').innerHTML = '<span class="empty">曲が読み込まれていません</span>';
    $('ar').textContent = '';
    $('k').textContent = '—'; $('b').textContent = '—'; $('p').textContent = ''; $('r').textContent = '—';
    return;
  }
  $('t').textContent = t.title;
  $('ar').textContent = t.artist ?? '';
  $('k').textContent = t.camelot ?? '—';
}

/** 毎フレーム更新する数値（BPM・テンポ変化率・残り時間）。 */
export function updateDeckNumbers(deck, d) {
  const $ = id => document.getElementById(id + deck);
  if (!d.track) return;
  const pos = d.position();
  const native = localBpm(d, pos);
  $('b').textContent = native ? (native * d.rate).toFixed(1) : '—';
  const pct = (d.rate - 1) * 100;
  $('p').textContent = `${pct >= 0 ? '+' : ''}${pct.toFixed(1)}%`;
  $('r').textContent = `-${fmt(d.duration - pos)}`;
}

/** 曲のその位置での元のテンポ（解析のテンポ曲線から）。 */
export function localBpm(d, pos) {
  const tc = d.analysis?.tempo_curve;
  if (!tc?.t?.length) return d.analysis?.bpm ?? d.track?.bpm ?? null;
  let lo = 0, hi = tc.t.length - 1;
  while (hi - lo > 1) { const m = (lo + hi) >> 1; if (tc.t[m] <= pos) lo = m; else hi = m; }
  return tc.bpm[lo] || d.analysis.bpm;
}

/** 1 デッキ分のレーン。bm（BeatMap）を渡すと拍軸（SYNC 中。2 曲の小節線が縦にそろう）、無ければ時間軸。 */
const WIN_BEATS = 8;   // 拍軸表示のとき、中央から左右に何拍見せるか
function drawLane(g, W, y0, h, d, deck, colors, flip, bm) {
  const pos = deck.position(), cx = W / 2, mid = y0 + h / 2;
  // x（実ピクセル）→ 入力時刻（秒）
  let tAt, xAt;
  if (bm) {
    const pc = bm.pos(pos), ppb = W / (2 * WIN_BEATS);
    tAt = x => bm.time(pc + (x - cx) / ppb);
    xAt = t => cx + (bm.pos(t) - pc) * ppb;
  } else {
    const pps = W / (2 * ZOOM_SEC);
    tAt = x => pos + (x - cx) / pps;
    xAt = t => cx + (t - pos) * pps;
  }
  g.save();
  g.beginPath(); g.rect(0, y0, W, h); g.clip();
  const a = deck.analysis;
  const t0 = tAt(0), t1 = tAt(W);
  // 拍なし区間の斜線
  for (const [s, e] of a?.no_beat ?? []) {
    if (e < t0 || s > t1) continue;
    const x0 = xAt(Math.max(s, t0 - 1)), x1 = xAt(Math.min(e, t1 + 1));
    g.fillStyle = 'rgba(255,255,255,.04)'; g.fillRect(x0, y0, x1 - x0, h);
    g.strokeStyle = 'rgba(255,255,255,.08)'; g.lineWidth = d;
    for (let x = x0 - h; x < x1; x += 8 * d) { g.beginPath(); g.moveTo(x, y0 + h); g.lineTo(x + h, y0); g.stroke(); }
  }
  // ループ区間
  if (deck.loop) {
    const x0 = xAt(deck.loop.start), x1 = xAt(deck.loop.end);
    g.fillStyle = deck.loop.active ? 'rgba(34,197,94,.14)' : 'rgba(255,255,255,.05)';
    g.fillRect(x0, y0, x1 - x0, h);
  }
  if (deck.wave) {
    const rate = a.wave.rate;
    const amp = (h / 2 - 3 * d) / 255;
    const tx = new Float64Array(W + 1);
    for (let x = 0; x <= W; x++) tx[x] = tAt(x) * rate;
    for (const [band, cssVar, sc] of BAND_LAYERS) {
      const arr = deck.wave[band], v = new Float32Array(W);
      for (let x = 0; x < W; x++) v[x] = sampleAt(arr, tx[x], tx[x + 1]);
      fillEnvelope(g, smooth3(v), mid, amp * sc, colors[cssVar]);
    }
  } else if (deck.track) {
    g.fillStyle = colors['--faint'];
    g.font = `${12 * d}px "Hiragino Sans", "Yu Gothic UI", sans-serif`;
    g.fillText('解析中…（再生はできます）', 12 * d, mid + 4 * d);
  }
  // 拍線・小節線
  if (a?.beats) {
    const bars = new Map((a.bars ?? []).map((b, i) => [b.beat, { n: i + 1, conf: b.conf }]));
    let i = lowerBound(a.beats, t0 - 1);
    for (; i < a.beats.length && a.beats[i] < t1 + 1; i++) {
      const x = Math.round(xAt(a.beats[i])), bar = bars.get(i);
      g.fillStyle = bar ? 'rgba(255,255,255,.6)' : 'rgba(255,255,255,.16)';
      g.fillRect(x, y0, bar ? 2 * d : d, h);
      if (bar) {
        g.fillStyle = bar.conf < 0.3 ? colors['--faint'] : colors['--dim'];
        g.font = `${11 * d}px Consolas`;
        g.fillText(bar.n + (bar.conf < 0.3 ? '?' : ''), x + 4 * d, flip ? y0 + h - 5 * d : y0 + 13 * d);
      }
    }
  }
  // キュー位置とホットキュー
  if (deck.track) {
    const mark = (t, color, label) => {
      if (t == null || t < t0 - 1 || t > t1 + 1) return;
      const x = Math.round(xAt(t));
      g.fillStyle = color;
      g.beginPath(); g.moveTo(x, flip ? y0 + h : y0); g.lineTo(x + 6 * d, flip ? y0 + h - 8 * d : y0 + 8 * d);
      g.lineTo(x - 6 * d, flip ? y0 + h - 8 * d : y0 + 8 * d); g.closePath(); g.fill();
      if (label) { g.font = `${10 * d}px Consolas`; g.fillText(label, x + 7 * d, flip ? y0 + h - 2 * d : y0 + 9 * d); }
    };
    mark(deck.cue, colors['--warn'] || '#eab308');
    (deck.hotcues ?? []).forEach((t, i) => mark(t, colors[deck.id === 'A' ? '--deck-a' : '--deck-b'], String(i + 1)));
  }
  g.restore();
}

function lowerBound(arr, v) {
  let lo = 0, hi = arr.length;
  while (lo < hi) { const m = (lo + hi) >> 1; if (arr[m] < v) lo = m + 1; else hi = m; }
  return lo;
}

/** phase: {beat, len}（マスターの小節内の拍位置）, text（位相誤差などの表示）, warn（追従を止めている理由） */
export function drawZoom(decks, colors, phase) {
  const [g, W, H, d] = fitCanvas(document.getElementById('zoom'));
  g.clearRect(0, 0, W, H);
  const band = 22 * d, lh = (H - band) / 2;
  drawLane(g, W, 0, lh, d, decks.A, colors, false, phase?.beatAxis ? phase.bm.A : null);
  drawLane(g, W, lh + band, lh, d, decks.B, colors, true, phase?.beatAxis ? phase.bm.B : null);
  g.fillStyle = '#0d0f12';
  g.fillRect(0, lh, W, band);
  g.fillStyle = colors['--line'];
  g.fillRect(0, lh, W, d);
  g.fillRect(0, lh + band - d, W, d);
  // 位相メーター: マスターの小節内の拍を升目で示す
  if (phase?.len) {
    const n = Math.round(phase.len), cell = Math.min(90 * d, W * 0.08), x0 = W / 2 - n * cell / 2;
    for (let k = 0; k < n; k++) {
      g.fillStyle = Math.floor(phase.beat) === k ? 'rgba(34,197,94,.85)' : '#23272d';
      g.fillRect(Math.round(x0 + k * cell + 2 * d), Math.round(lh + 7 * d), Math.round(cell - 4 * d), Math.round(8 * d));
    }
  }
  g.font = `${11 * d}px Consolas`;
  g.fillStyle = phase?.warn ? colors['--warn'] : colors['--dim'];
  g.fillText(phase?.text ?? '時間軸表示', 12 * d, lh + 15 * d);
  g.fillStyle = colors['--active'];
  g.fillRect(Math.round(W / 2 - d), 0, 2 * d, H);
}

export function drawOverviews(decks, colors) {
  for (const id of ['A', 'B']) {
    const dk = decks[id];
    drawOverview(document.getElementById(`ov${id}`), dk.wave, dk.duration ? dk.position() / dk.duration : 0, colors,
                 colors[id === 'A' ? '--deck-a' : '--deck-b']);
  }
}
