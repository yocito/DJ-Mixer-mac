// つなぎ目エディタ（案B。design_phase2.md §6）。モックアップ mockup/phase2/ui_p2_B.html を本番用にしたもの。
// 2 曲の重なりを小節の目盛りで上下に並べ、重なりの長さ・繋ぎ方・低音を入れ替える位置・クロスフェードの区間を直す。
// ミキサーの動きの線は、実際の再生（runner.js）と同じ関数 transition.js の levels() で描く（見えている線どおりに鳴る）。
import * as queue from '../queue.js';
import { fitCanvas } from './waveform.js';
import { trackData, snapTime, barIdx, drawWave, useOf, fmt } from './joins-view.js';
import { tempoRel, keyRel } from './queue-view.js';
import { BeatMap } from '../engine/beatmap.js';
import { stemsOf, vocalsOf, bestVocalFit, VOCAL_ON, STEM_COLORS } from './vocals.js';
import { TEMPLATES, DEFAULT_PARAMS, joinOf, levels, overlapStart, judge, inStartBar, entersAtEnd, overlapBars, rampTarget, shiftCamelot, stableBounds, PARTS, usesParts, partGains,
  autoTrimDb, trimDb, levelDb, loudDb, LEVEL_BARS, TRIM_MAX_DB } from '../engine/transition.js';

// カットイン（design_cutin.md）: 切り替えポイントの候補の点数（analysis/cutpoints.py）。SHOW 以上を一覧に、GOOD 以上を★に
const CUT_SHOW = 2.5, CUT_GOOD = 3, CUT_GREAT = 4.5, CUT_NEAR = 24, CUT_MIN_USE = 60;

const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const clamp = (v, a, b) => Math.max(a, Math.min(b, v));
const HIT = 9;                  // 取っ手をつかめる距離（CSS ピクセル）

// 繋ぎ方ごとに、直せる値
const EDITABLE = { long: ['swap', 'xf'], short: ['swap', 'xf'], bass: ['swap'], filter: [], echo: [], cut: [] };
const FX_ROW = { filter: ['前の曲のハイパス', 'hpOut'], echo: ['前の曲のエコー', 'echoOut'] };

export function initJoinEditor({ root, colors, onPreview, onStop, onBack, playhead, isBusy, flash, lufsTarget = () => -14 }) {
  let cur = -1, ctx = null, drag = null, view = 48, raf = 0;

  root.innerHTML = `
    <div class="p2card"><div class="p2title"><div class="btn" data-nav="-1">◀ 前のつなぎ</div><b data-r="ttl"></b>
      <div class="sp"></div><div class="btn" data-back>範囲選択へ戻る</div><div class="btn" data-nav="1">次のつなぎ ▶</div></div>
      <div class="jinfo" data-r="info"></div><div class="jwarn" data-r="warn" hidden></div></div>
    <div data-r="body"></div>`;
  const body = root.querySelector('[data-r=body]');
  root.querySelectorAll('[data-nav]').forEach(b => b.addEventListener('click', () => show(cur + +b.dataset.nav)));
  root.querySelector('[data-back]').addEventListener('click', () => onBack(cur));

  const emptyMsg = html => { body.innerHTML = `<div class="p2card jempty">${html}</div>`; root.querySelector('[data-r=warn]').hidden = true; };

  async function show(i) {
    const items = queue.getItems();
    const ttl = root.querySelector('[data-r=ttl]'), info = root.querySelector('[data-r=info]');
    if (items.length < 2) {
      cur = -1; ctx = null; ttl.textContent = ''; info.textContent = '';
      emptyMsg('つなぎ目がありません。<br>ミックスキューに 2 曲以上入れると、曲と曲のつなぎ目を直せます。');
      return;
    }
    cur = clamp(i, 0, items.length - 2);
    queue.setEditing({ join: cur });
    const A = items[cur], B = items[cur + 1];
    ttl.textContent = `${cur + 1} → ${cur + 2}：${A.track.title} → ${B.track.title}`;
    if (A.track.analysis_status !== 'done' || B.track.analysis_status !== 'done') {
      ctx = null; info.textContent = '';
      emptyMsg('どちらかの曲がまだ解析中です。解析が終わるとつなぎ目を直せます。');
      return;
    }
    let da, db;
    try { [da, db] = await Promise.all([trackData(A.track.id), trackData(B.track.id)]); }
    catch { emptyMsg('解析結果を読み込めませんでした。'); return; }
    if (queue.getItems()[cur] !== A) return;             // 読み込み中に別のつなぎへ移った
    da.bm ??= new BeatMap(da.a);
    db.bm ??= new BeatMap(db.a);
    ctx = { i: cur, A, B, da, db };
    // パートの量（Phase 4 B1）。まだなら裏で分離が始まり、届いたら描き直す
    const want = (id, key) => stemsOf(id, r => { if (ctx && (key === 'sa' ? ctx.A : ctx.B).track_id === id) { ctx[key] = r; render(); } });
    ctx.sa = want(A.track_id, 'sa'); ctx.sb = want(B.track_id, 'sb');
    view = Math.max(48, joinOf(A).bars + 16);
    body.innerHTML = `
      <div class="p2card">
        <div class="lbl2" style="margin-bottom:4px">曲ごとの使う範囲（小さな波形の上をドラッグ。小節に吸着）</div>
        <div class="ovrow"><div class="who"><b style="color:${colors['--deck-a']}">前の曲</b>${esc(A.track.title)}</div><canvas class="cv" data-ov="A" style="height:44px;cursor:text"></canvas><div class="rng" data-rng="A"></div></div>
        <div class="ovrow"><div class="who"><b style="color:${colors['--deck-b']}">次の曲</b>${esc(B.track.title)}</div><canvas class="cv" data-ov="B" style="height:44px;cursor:text"></canvas><div class="rng" data-rng="B"></div></div>
      </div>
      <div class="p2card">
        <div class="lbl2" style="margin-bottom:6px">つなぎ目（小節の目盛りを共通にして上下に重ねています）。下の曲を左右にドラッグして重なりの長さを決めます。</div>
        <canvas class="cv" data-r="ed" style="height:300px;cursor:grab"></canvas>
        <div class="lbl2" style="margin:10px 0 4px" data-r="autohint"></div>
        <canvas class="cv" data-r="auto" style="height:200px"></canvas>
      </div>
      <div class="p2card selbar">
        <div><div class="lbl2">繋ぎ方</div><select class="tpl" data-r="tpl">${Object.entries(TEMPLATES).map(([k, t]) => `<option value="${k}">${esc(t.label)}</option>`).join('')}</select></div>
        <div><div class="lbl2">重なり</div><div class="seg2" data-r="bars"><div data-d="-4">−4</div><div data-d="-1">−1</div>
          <div data-r="nb" class="val"></div><div data-d="1">＋1</div><div data-d="4">＋4</div></div></div>
        <div data-r="pos" class="len"></div>
        <div class="sp" style="flex:1"></div>
        <div class="btn" data-r="reset">標準に戻す</div>
        <div class="btn" data-r="play">▶ つなぎ目だけ試聴</div>
      </div>
      <div class="p2card selbar" data-r="more">
        <div><div class="lbl2">テンポの合わせ方</div><div class="seg2" data-r="tempo"><div data-v="sync">次の曲を合わせる</div><div data-v="ramp">テンポランプ</div></div></div>
        <div><div class="lbl2">ループで延ばす（前の曲の最後を繰り返す）</div><div class="seg2" data-r="loop"><div data-v="0">なし</div><div data-v="4">4 小節</div><div data-v="8">8 小節</div></div></div>
        <div><div class="lbl2">キー合わせ（次の曲を半音ずらす）</div><div class="seg2" data-r="shift">${[-2, -1, 0, 1, 2].map(v => `<div data-v="${v}">${v ? (v > 0 ? '＋' : '−') + Math.abs(v) : 'そのまま'}</div>`).join('')}</div></div>
        <div><div class="lbl2">パートごとに鳴らす（音源分離）</div><div class="seg2" data-r="parts">${Object.entries(PARTS).map(([k, p]) => `<div data-v="${k}" title="${esc(p.hint ?? '')}">${esc(p.label)}</div>`).join('')}</div>
          <div class="lbl2" data-r="partsst" style="margin-top:4px"></div></div>
        <div class="len" data-r="morehint" style="flex:1;min-width:240px"></div>
        <div class="btn" data-r="vfix" title="前の曲の終わりと重なりの長さを変えて、両方の曲の歌が重ならないようにします">ボーカルが重ならない位置にずらす</div>
        <div class="btn" data-r="bfix" title="前の曲の終わり・次の曲の始まりを、拍の取れる所へずらします（拍を合わせて繋げるように）">拍の取れる所で繋ぐ</div>
      </div>
      <div class="p2card cutbox" data-r="cutbox" hidden>
        <div class="cutcol"><div class="lbl2">① タイミング</div><div class="cuttxt" data-r="cuttime"></div></div>
        <div class="cutcol"><div class="lbl2">② 前の曲の切り替えポイント（この小節の頭でスパッと切る）</div>
          <div class="cutnav"><div class="btn" data-cn="out:-1">◀ 前の候補</div><div class="btn" data-cn="out:1">次の候補 ▶</div></div>
          <div class="cutlist" data-r="couts"></div></div>
        <div class="cutcol"><div class="lbl2">② 次の曲の入り（この小節の 1 拍目から鳴らす）</div>
          <div class="cutnav"><div class="btn" data-cn="in:-1">◀ 前の候補</div><div class="btn" data-cn="in:1">次の候補 ▶</div></div>
          <div class="cutlist" data-r="cins"></div></div>
        <div class="cutcol"><div class="lbl2">③ 音量合わせ（TRIM。次の曲の入りに掛け、16 小節かけて元へ戻す）</div>
          <div class="seg2" data-r="trim"><div data-v="auto">自動</div><div data-v="off">なし</div><div data-tr="-0.5">−0.5</div><div class="val" data-r="trimv"></div><div data-tr="0.5">＋0.5</div></div>
          <div class="cuttxt" data-r="trimhint"></div></div>
      </div>`;
    bind();
    render();
    runnerChanged();
  }

  // ---- 計算 ----
  const J = () => joinOf(ctx.A);
  /** 画面の座標と、実際に鳴る繋ぎ（拍を合わせられないときはエコーアウトに変わる）。 */
  function geom() {
    const { A, B, da, db } = ctx, j = J();
    const ua = useOf(A, da), ub = useOf(B, db);
    const bIn = inStartBar(db.barTimes, ub.t0);
    const ov = overlapBars(da.barTimes, ua.t1, j);
    const ovEnd = da.barTimes[ov.endBar] ?? ua.t1;
    const jd = judge(da.bm, db.bm, ov.startTime, ovEnd, db.barTimes[bIn] ?? ub.t0);
    let eff = j, ovE = ov;
    if (jd.reason && j.tpl !== 'echo' && j.tpl !== 'cut') {
      eff = { ...j, tpl: 'echo', bars: 2 };
      ovE = overlapBars(da.barTimes, ua.t1, eff);
    }
    const loop = jd.reason || entersAtEnd(eff.tpl) ? 0 : ovE.loop;
    const n = ovE.endBar - ovE.startBar + loop;
    const pE = da.bm.pos(da.barTimes[ovE.endBar] ?? ua.t1);
    const loopBeats = loop ? pE - da.bm.pos(da.barTimes[ovE.loopStartBar]) : 0;
    const beats = Math.max(1, pE - da.bm.pos(ovE.startTime) + loopBeats);
    // テンポランプ: 重なりの終わりの前の曲の速度（次の曲がちょうど原速になる速度。runner と同じ式）
    const inTime = db.barTimes[bIn] ?? ub.t0;
    const rampTo = !jd.reason && !entersAtEnd(eff.tpl) && eff.params.tempo === 'ramp'
      ? rampTarget(da.bm, db.bm, da.barTimes[ovE.endBar] ?? ua.t1, db.bm.time(db.bm.pos(inTime) + jd.k * beats), jd.k) : null;
    // エコーアウト・カット・拍を合わせられない繋ぎは、次の曲が前の曲の終わりから入る（runner.js と同じ）
    const bOff = jd.reason || entersAtEnd(eff.tpl) ? n : 0;
    return { j, eff, jd, ua, ub, aEnd: ovE.endBar, aStart: ovE.startBar, loop, loopStartBar: ovE.loopStartBar, rampTo, bIn, n, bOff, beats,
             left: -Math.floor((view - n) / 2) };
  }

  /** ボーカルが重ならない重なり（おまかせと同じ探索）。前の曲は使う範囲の頭から重なりの終わりまで、次の曲は入り口から。 */
  function vocalFit(G) {
    if (!ctx.sa || !ctx.sb || !G) return { clash0: 0, clash: 0, bars: G?.j.bars, trim: 0 };
    const b0 = barIdx(ctx.da, G.ua.t0);
    return bestVocalFit(vocalsOf(ctx.sa, b0, G.aEnd), vocalsOf(ctx.sb, G.bIn, G.bIn + 64), G.j.bars);
  }

  /** パートの音（分離して保存した音）の状態。パートを選んでいれば、無い曲は分離を始める。 */
  let partsAsked = '';
  async function partsStatus(G, ok) {
    const el = body.querySelector('[data-r=partsst]');
    const want = ok && (G.j.params.parts || 'none') !== 'none';
    if (!want) { el.textContent = ok ? '' : '拍を合わせて重ねる繋ぎ方のときに使えます'; return; }
    const key = `${ctx.A.track_id}-${ctx.B.track_id}`;
    if (partsAsked === key + ':done') { el.textContent = 'パートの音: 用意できています'; return; }
    const st = await Promise.all([ctx.A.track_id, ctx.B.track_id].map(id => fetch(`/api/tracks/${id}/stems-audio`).then(r => r.json()).catch(() => ({ status: 'error' }))));
    if (`${ctx.A.track_id}-${ctx.B.track_id}` !== key) return;
    if (st.every(s => s.status === 'done')) { partsAsked = key + ':done'; el.textContent = 'パートの音: 用意できています'; return; }
    el.textContent = 'パートの音を用意しています（分離して保存。1 曲 20 秒ほど）…';
    clearTimeout(partsStatus.t);
    partsStatus.t = setTimeout(() => { if (ctx && `${ctx.A.track_id}-${ctx.B.track_id}` === key) render(); }, 3000);
  }

  /** 拍の取れない所で繋ごうとしているとき、拍の取れる所へずらす案（前の曲の終わり・次の曲の始まり）。無ければ null。 */
  function beatFix(G) {
    if (!G || !G.jd.reason || !/拍がありません/.test(G.jd.reason) || G.j.tpl === 'echo' || G.j.tpl === 'cut') return null;
    const { da, db } = ctx, R = Math.max(2, G.j.bars);
    const btA = [...da.barTimes, da.a.duration], btB = [...db.barTimes, db.a.duration];
    const a0 = barIdx(da, G.ua.t0), a1 = Math.min(btA.length - 1, G.aEnd);
    const b0 = barIdx(db, G.ub.t0), b1 = Math.min(btB.length - 1, barIdx(db, G.ub.t1));
    const fa = stableBounds(da.a.no_beat, btA, a0, a1, R, { start: false, tol: 0.05 });
    const fb = stableBounds(db.a.no_beat, btB, b0, b1, R, { end: false, tol: 0.05 });
    const out = {}, what = [];
    if (fa.b1 !== a1) { out.useEnd = btA[fa.b1]; what.push(`前の曲を ${a1 - fa.b1} 小節早く終える`); }
    if (fb.b0 !== b0) { out.useStart = btB[fb.b0]; what.push(`次の曲を ${fb.b0 - b0} 小節先から始める`); }
    return what.length ? { ...out, what: what.join('・') } : null;
  }

  function save(fields) {
    queue.patch(ctx.i, fields);
    render();
  }

  // ---- カットイン（design_cutin.md） ----
  /** 候補（解析時の小節）を今の小節に吸着させ、使える範囲のものだけ。kind: 'outs' | 'ins'。 */
  function cutCands(G, kind) {
    const d = kind === 'outs' ? ctx.da : ctx.db, list = d.cut?.[kind] ?? [];
    const seen = new Set(), out = [];
    for (const c of list) {
      const t = snapTime(d, c.t, 'bar');
      if (seen.has(t)) continue;
      const keep = kind === 'outs' ? Math.min(CUT_MIN_USE, 0.6 * (G.ua.t1 - G.ua.t0)) : Math.min(CUT_MIN_USE, 0.6 * (G.ub.t1 - G.ub.t0));
      if (kind === 'outs' ? t - G.ua.t0 < keep : G.ub.t1 - t < keep) continue;
      seen.add(t);
      out.push({ ...c, t, bar: barIdx(d, t) });
    }
    return out;
  }
  const stars = sc => (sc >= CUT_GREAT ? '★★' : sc >= CUT_GOOD ? '★' : '');
  /** 実際に掛ける音量合わせと、その根拠（前後 4 小節の聞こえる大きさ）。 */
  function cutTrim(G) {
    const { da, db } = ctx, tCut = da.barTimes[G.aEnd] ?? G.ua.t1, tIn = db.barTimes[G.bIn] ?? G.ub.t0;
    const barO = da.bm.beatLen(tCut) * (da.a.meter || 4), barI = db.bm.beatLen(tIn) * (db.a.meter || 4), T = lufsTarget();
    const auto = autoTrimDb(da.cut?.bars, tCut, barO, da.a.lufs ?? -14, db.cut?.bars, tIn, barI, db.a.lufs ?? -14, T);
    const lo = levelDb(da.cut?.bars, tCut - LEVEL_BARS * barO, tCut), li = levelDb(db.cut?.bars, tIn, tIn + LEVEL_BARS * barI);
    return { auto, db: trimDb(G.j.params, auto), mode: G.j.params.cutTrim ?? 'auto',
             lo: lo == null ? null : lo + loudDb(da.a.lufs ?? -14, T), li: li == null ? null : li + loudDb(db.a.lufs ?? -14, T), tCut, tIn };
  }
  function renderCut(G) {
    const box = body.querySelector('[data-r=cutbox]');
    box.hidden = G.j.tpl !== 'cut';
    if (box.hidden) return;
    const tr = cutTrim(G), { da, db } = ctx;
    const bpmA = 60 / da.bm.beatLen(tr.tCut), bpmB = 60 / db.bm.beatLen(tr.tIn);
    body.querySelector('[data-r=cuttime]').innerHTML =
      `前の曲の <b>${fmt(tr.tCut)}</b>（${G.aEnd + 1} 小節目の頭）で止め、同じ瞬間に次の曲の <b>${fmt(tr.tIn)}</b>（${G.bIn + 1} 小節目の 1 拍目）を鳴らします。` +
      `両方とも拍の位置から計算し、サンプル単位で予約するので、CUE でリズムを取る必要はありません（テンポは ${bpmA.toFixed(1)} → ${bpmB.toFixed(1)} BPM、それぞれ原速のまま）。`;
    const list = (kind, cur, key) => {
      const cs = cutCands(G, kind).filter(c => c.score >= CUT_SHOW || Math.abs(c.t - cur) < 0.05);
      const near = cs.filter(c => Math.abs(c.bar - barIdx(kind === 'outs' ? da : db, cur)) <= CUT_NEAR);
      body.querySelector(`[data-r=${key}]`).innerHTML = near.map(c =>
        `<div class="cpt ${Math.abs(c.t - cur) < 0.05 ? 'on' : ''}" data-ct="${kind}" data-t="${c.t}" title="点数 ${c.score}"><b class="mono">${fmt(c.t)}</b> <span class="st">${stars(c.score)}</span> <small>${esc(c.tags.join('・') || '小節の頭')}</small></div>`).join('')
        || '<div class="lbl2">近くに候補がありません（◀ ▶ で曲の中の候補を探せます）</div>';
    };
    list('outs', tr.tCut, 'couts');
    list('ins', tr.tIn, 'cins');
    body.querySelectorAll('[data-r=trim] [data-v]').forEach(x => x.classList.toggle('on', String(tr.mode) === x.dataset.v));
    body.querySelector('[data-r=trimv]').textContent = `${tr.db > 0 ? '+' : ''}${tr.db.toFixed(1)} dB`;
    const f = v => (v == null ? '—' : `${v.toFixed(1)} dB`);
    body.querySelector('[data-r=trimhint]').textContent =
      `前の曲の最後の 4 小節 ${f(tr.lo)} ／ 次の曲の最初の 4 小節 ${f(tr.li)}（曲ごとの音量の揃えを含む）。自動なら ${tr.auto > 0 ? '+' : ''}${tr.auto.toFixed(1)} dB` +
      (tr.auto === 0 && tr.lo != null && tr.li != null ? '（差が 1dB 未満なのでそのまま）' : '');
  }
  function setCutPoint(kind, t) {
    if (kind === 'outs') queue.patch(ctx.i, { use_end: t });
    else queue.patch(ctx.i + 1, { use_start: t });
    render();
  }

  // ---- 描画 ----
  function render() {
    if (!ctx) return;
    const G = ctx.lastG = geom();
    renderHead(G);
    drawOv('A', G); drawOv('B', G);
    drawEditor(G);
    drawAuto(G);
    renderCut(G);
    present();
  }

  function renderHead(G) {
    const { A, B, da, db } = ctx;
    const bpmA = da.a.bpm_display ?? da.a.bpm, bpmB = db.a.bpm_display ?? db.a.bpm;
    const t = tempoRel(bpmA, bpmB), kA = da.a.key?.camelot, kB0 = db.a.key?.camelot, shift = G.j.params.shift || 0, kB = shiftCamelot(kB0, shift);
    const tc = Math.abs(t.pct) <= 3 ? 'okc' : Math.abs(t.pct) <= 8 ? 'midc' : 'ngc';
    const k = kA && kB ? keyRel(kA, kB) : null;
    const sec = G.beats * da.bm.beatLen(da.barTimes[G.aEnd] ?? 0);
    root.querySelector('[data-r=info]').innerHTML =
      `<span>テンポ <b>${bpmA.toFixed(1)} → ${bpmB.toFixed(1)}</b> BPM ・ <span class="${tc}">次の曲を ${t.pct >= 0 ? '+' : ''}${t.pct.toFixed(1)}%${t.k !== 1 ? `（×${t.k}）` : ''} で合わせる</span></span>` +
      `<span>キー <b>${kA ?? '—'} → ${kB ?? '—'}</b>${shift ? `（元は ${kB0}、${shift > 0 ? '+' : ''}${shift} 半音）` : ''}${k ? ` ・ <span class="${k[1]}c">${k[0]} ${k[2]}${k[1] === 'ng' ? '（重なりは短めがおすすめ）' : ''}</span>` : ''}</span>` +
      `<span>重なり 約 <b>${Math.round(sec)}</b> 秒</span>`;
    const w = root.querySelector('[data-r=warn]');
    w.hidden = !G.jd.reason || G.j.tpl === 'cut';
    if (G.jd.reason) w.textContent = G.j.tpl === 'echo'
      ? `⚠ ${G.jd.reason}。拍を合わせずにエコーアウトで繋ぎます`
      : `⚠ ${G.jd.reason}。拍を合わせられないので、自動で「エコーアウト 2 小節」に切り替えて繋ぎます（下の線は切り替え後の動き）`;
    const tpl = body.querySelector('[data-r=tpl]');
    if (tpl && document.activeElement !== tpl) tpl.value = G.j.tpl;
    body.querySelector('[data-r=nb]').textContent = G.j.tpl === 'cut' ? '重ねない' : `${G.j.bars} 小節`;
    body.querySelector('[data-r=bars]').classList.toggle('off', G.j.tpl === 'cut');
    const ed = EDITABLE[G.eff.tpl], p = G.eff.params, meter = ctx.da.a.meter || 4;
    // 重なりの中の位置を「N 小節目（の M 拍目）」で表す
    const at = u => {
      if (u >= 1) return '重なりの終わり';
      const beat = Math.round(u * G.beats), bar = Math.floor(beat / meter), b = beat - bar * meter;
      return `${bar + 1}小節目${b ? `の${b + 1}拍目` : ''}`;
    };
    const parts = [];
    if (ed.includes('swap')) parts.push(`低音の入れ替え: ${at(p.swapAt)}`);
    if (ed.includes('xf')) parts.push(`クロスフェード: ${at(p.xfStart)}〜${at(p.xfEnd)}`);
    body.querySelector('[data-r=pos]').textContent = parts.join(' ・ ');
    // テンポの合わせ方・ループ・キー合わせ（Phase 4）
    const late = !!G.jd.reason || entersAtEnd(G.eff.tpl);
    // パートごとに鳴らす（Phase 5 B2）: 拍を合わせて重ねる繋ぎ（ロング・ショート・低音だけ入れ替え）だけ
    const partsOk = !late && ['long', 'short', 'bass'].includes(G.eff.tpl);
    body.querySelector('[data-r=parts]').classList.toggle('off', !partsOk);
    body.querySelectorAll('[data-r=parts] [data-v]').forEach(x => x.classList.toggle('on', (G.j.params.parts || 'none') === x.dataset.v));
    partsStatus(G, partsOk);
    for (const [k2, v] of [['tempo', G.j.params.tempo || 'sync'], ['loop', G.j.params.loop || 0], ['shift', shift]])
      body.querySelectorAll(`[data-r=${k2}] [data-v]`).forEach(x => x.classList.toggle('on', String(v) === x.dataset.v));
    body.querySelector('[data-r=tempo]').classList.toggle('off', late);
    body.querySelector('[data-r=loop]').classList.toggle('off', late);
    const good = kA && kB0 ? [-2, -1, 1, 2].find(sft => ['ok'].includes(keyRel(kA, shiftCamelot(kB0, sft))[1])) : null;
    const hints = [];
    if (G.rampTo) hints.push(`テンポランプ: 重なりの間に前の曲を ${((G.rampTo - 1) * 100).toFixed(1)}% 変え、次の曲は重なりの終わりで原速`);
    if (G.loop) hints.push(`ループ: 前の曲の最後の ${G.loop} 小節を 1 回繰り返す（重なり ${G.n} 小節）`);
    if (kA && kB0 && keyRel(kA, kB0)[1] !== 'ok' && good != null && shift !== good) hints.push(`キー: ${good > 0 ? '+' : ''}${good} 半音で ${shiftCamelot(kB0, good)}（${keyRel(kA, shiftCamelot(kB0, good))[2]}）になります`);
    if (late) hints.push('エコーアウト・カットインではテンポランプとループは使いません');
    // ボーカルの重なり（Phase 4 B1）
    const vf = vocalFit(G);
    const vbtn = body.querySelector('[data-r=vfix]');
    if (!ctx.sa || !ctx.sb) { hints.push('パート（ボーカルなど）を分析しています…'); vbtn.classList.add('off'); }
    else {
      hints.push(vf.clash0 ? `ボーカルが ${vf.clash0} 小節で重なっています（赤）` : 'ボーカルは重なっていません');
      vbtn.classList.toggle('off', !vf.clash0 || vf.clash >= vf.clash0 || !!G.loop || late);
    }
    // 拍の取れない所で繋ごうとしている → 拍の取れる所へずらせるか（Phase 5 F）
    const bf = beatFix(G);
    body.querySelector('[data-r=bfix]').classList.toggle('off', !bf);
    if (bf) hints.push(`拍の取れる所で繋げます（${bf.what}）`);
    body.querySelector('[data-r=morehint]').textContent = hints.join(' ／ ');
    body.querySelector('[data-r=autohint]').textContent = ed.length
      ? `ミキサーの動き — ${ed.includes('swap') ? '縦線（低音の入れ替え）' : ''}${ed.length > 1 ? 'と' : ''}${ed.includes('xf') ? '◆（クロスフェーダーの始まり・終わり）' : ''}をドラッグして直せます（拍に吸着）`
      : 'ミキサーの動き（この繋ぎ方では自動で決まります）';
  }

  function drawOv(id, G) {
    const d = id === 'A' ? ctx.da : ctx.db, u = id === 'A' ? G.ua : G.ub, tmp = drag?.ov === id ? drag.u : u;
    const [g, W, H, dp] = fitCanvas(body.querySelector(`[data-ov=${id}]`));
    g.fillStyle = '#050607'; g.fillRect(0, 0, W, H);
    drawWave(g, d, 0, d.a.duration, 0, W, 0, H, colors, 0.25);
    const x0 = tmp.t0 / d.a.duration * W, x1 = tmp.t1 / d.a.duration * W;
    g.save(); g.beginPath(); g.rect(x0, 0, x1 - x0, H); g.clip(); drawWave(g, d, 0, d.a.duration, 0, W, 0, H, colors, 1); g.restore();
    g.strokeStyle = '#fff'; g.lineWidth = 2 * dp; g.strokeRect(x0, dp, x1 - x0, H - 2 * dp);
    if (G.j.tpl === 'cut') {          // カットインの切り替えポイントの候補（★ 以上）
      g.fillStyle = '#facc15';
      for (const c of cutCands(G, id === 'A' ? 'outs' : 'ins')) {
        if (c.score < CUT_GOOD) continue;
        const x = c.t / d.a.duration * W;
        g.beginPath(); g.moveTo(x - 4 * dp, 0); g.lineTo(x + 4 * dp, 0); g.lineTo(x, 7 * dp); g.closePath(); g.fill();
      }
    }
    body.querySelector(`[data-rng=${id}]`).textContent = `${fmt(tmp.t0)} – ${fmt(tmp.t1)}`;
  }

  // 重ねた波形と自動操作の線は、下描き用のキャンバスに描いておき、再生位置の線だけを毎フレーム重ねる
  const buf = { ed: document.createElement('canvas'), auto: document.createElement('canvas') };
  function bufCtx(key) {
    const [, W, H, dp] = fitCanvas(body.querySelector(`[data-r=${key}]`));
    const b = buf[key];
    if (b.width !== W || b.height !== H) { b.width = W; b.height = H; }
    return [b.getContext('2d'), W, H, dp];
  }
  const X = (G, W) => rel => (rel - G.left) * W / view;

  function drawEditor(G) {
    const { da, db } = ctx, [g, W, H, dp] = bufCtx('ed'), xb = X(G, W);
    g.fillStyle = '#050607'; g.fillRect(0, 0, W, H);
    const ruler = 22 * dp, gap = 8 * dp, lh = (H - ruler - gap) / 2, yA = ruler, yB = ruler + lh + gap;
    g.fillStyle = 'rgba(34,197,94,.13)'; g.fillRect(xb(0), ruler, G.n * W / view, H - ruler);
    // 小節ごとに、それぞれの曲の小節の長さで伸縮して描く（テンポが違っても小節線がそろう）
    for (let rel = G.left; rel < G.left + view; rel++) {
      const x0 = xb(rel), x1 = xb(rel + 1);
      const rep = G.loop && rel >= G.n - G.loop && rel < G.n;           // ループで繰り返す小節
      const ia = rep ? G.loopStartBar + (rel - (G.n - G.loop)) : G.aStart + rel;
      if (rel < G.n && ia >= 0 && ia + 1 < da.barTimes.length) {
        drawWave(g, da, da.barTimes[ia], da.barTimes[ia + 1], x0, x1, yA, lh, colors, 0.95);
        if (rep) {
          g.save(); g.strokeStyle = 'rgba(250,204,21,.4)'; g.lineWidth = dp;
          for (let x = x0; x < x1; x += 6 * dp) { g.beginPath(); g.moveTo(x, yA); g.lineTo(x + 6 * dp, yA + lh); g.stroke(); }
          g.restore();
        }
      }
      const ib = G.bIn + rel - G.bOff;
      if (rel >= G.bOff && ib + 1 < db.barTimes.length) drawWave(g, db, db.barTimes[ib], db.barTimes[ib + 1], x0, x1, yB, lh, colors, 0.95);
    }
    // ボーカルの帯（前の曲は波形の下、次の曲は波形の上）と、両方の曲にボーカルがある小節（赤。Phase 4 B1）
    const outIdx = rel => (G.loop && rel >= G.n - G.loop && rel < G.n ? G.loopStartBar + (rel - (G.n - G.loop)) : G.aStart + rel);
    const vA = rel => (ctx.sa && rel < G.n ? ctx.sa.bars[outIdx(rel)]?.[0] : undefined);
    const vB = rel => (ctx.sb && rel >= G.bOff ? ctx.sb.bars[G.bIn + rel - G.bOff]?.[0] : undefined);
    for (let rel = G.left; rel < G.left + view; rel++) {
      const x0 = xb(rel), x1 = xb(rel + 1), a = vA(rel), b = vB(rel);
      if (a !== undefined) { g.globalAlpha = Math.min(1, a * 1.4); g.fillStyle = STEM_COLORS.vocals; g.fillRect(x0, yA + lh - 5 * dp, x1 - x0, 5 * dp); }
      if (b !== undefined) { g.globalAlpha = Math.min(1, b * 1.4); g.fillStyle = STEM_COLORS.vocals; g.fillRect(x0, yB, x1 - x0, 5 * dp); }
      g.globalAlpha = 1;
      if (rel >= 0 && rel < G.n && a > VOCAL_ON && b > VOCAL_ON) { g.fillStyle = 'rgba(239,68,68,.25)'; g.fillRect(x0, ruler, x1 - x0, H - ruler); }
    }
    g.font = `${10 * dp}px Consolas`;
    for (let rel = G.left; rel <= G.left + view; rel++) {
      const x = Math.round(xb(rel));
      g.fillStyle = rel % 4 === 0 ? 'rgba(255,255,255,.35)' : 'rgba(255,255,255,.1)'; g.fillRect(x, ruler, dp, H - ruler);
      if (rel % 4 === 0) { g.fillStyle = colors['--dim']; g.fillText(`${rel > 0 ? '+' : ''}${rel}`, x + 3 * dp, 14 * dp); }
    }
    const mark = (x, y, col, label) => {
      g.fillStyle = col; g.fillRect(Math.round(x) - dp, y, 2 * dp, lh);
      g.beginPath(); g.moveTo(x, y); g.lineTo(x + 9 * dp, y); g.lineTo(x, y + 11 * dp); g.closePath(); g.fill();
      g.fillStyle = '#e8e8e8'; g.font = `${11 * dp}px "Hiragino Sans", "Yu Gothic UI", sans-serif`; g.fillText(label, x + 6 * dp, y + lh - 6 * dp);
    };
    mark(xb(G.n), yA, colors['--deck-a'], `前の曲の終わり ${fmt(da.barTimes[G.aEnd] ?? G.ua.t1)}（${G.aEnd + 1}小節目）`);
    mark(xb(G.bOff), yB, colors['--deck-b'], `次の曲の始まり ${fmt(db.barTimes[G.bIn] ?? G.ub.t0)}（${G.bIn + 1}小節目）`);
    if (G.loop) { g.fillStyle = '#facc15'; g.font = `${11 * dp}px "Hiragino Sans", "Yu Gothic UI", sans-serif`; g.fillText(`↻ 最後の ${G.loop} 小節を繰り返す`, xb(G.n - G.loop) + 4 * dp, yA + 14 * dp); }
    g.fillStyle = '#e8e8e8'; g.font = `${12 * dp}px "Hiragino Sans", "Yu Gothic UI", sans-serif`;
    // 重なりが短いときは、前の曲の終わりの文字と重ならないよう重なりの左側に書く
    const narrow = G.n * W / view < 140 * dp;
    g.textAlign = narrow ? 'right' : 'left';
    g.fillText(G.n ? `重なり ${G.n} 小節` : '重ねずに切り替え', xb(0) + (narrow ? -6 : 6) * dp, ruler + lh + 6 * dp);
    g.textAlign = 'left';
    // 低音を入れ替える位置は、波形の上にも点線で出す
    if (EDITABLE[G.eff.tpl].includes('swap')) {
      const x = Math.round(xb(G.eff.params.swapAt * G.n));
      g.save(); g.setLineDash([4 * dp, 4 * dp]); g.strokeStyle = '#facc15'; g.lineWidth = 1.5 * dp;
      g.beginPath(); g.moveTo(x, ruler); g.lineTo(x, H); g.stroke(); g.restore();
    }
  }

  function rowsOf(G) {
    const rows = [['クロスフェーダー（上が次の曲）', 'xf', '#e8e8e8']];
    if (usesParts(G.eff)) {            // パートごとの音量（Phase 5 B2）。EQ の LOW は動かさない
      const m = G.eff.params.parts, A = colors['--deck-a'], B = colors['--deck-b'];
      if (m === 'vocal' || m === 'swap') rows.push(['前の曲 ボーカル', 'p:out:0', A]);
      if (m === 'swap') rows.push(['前の曲 ベース', 'p:out:2', A]);
      if (m === 'drums' || m === 'swap') rows.push(['次の曲 その他', 'p:in:3', B], ['次の曲 ベース', 'p:in:2', B], ['次の曲 ボーカル', 'p:in:0', B]);
    } else rows.push(['前の曲の LOW', 'lowOut', colors['--deck-a']], ['次の曲の LOW', 'lowIn', colors['--deck-b']]);
    if (FX_ROW[G.eff.tpl]) rows.push([...FX_ROW[G.eff.tpl], '#c084fc']);
    if (G.rampTo) rows.push(['前の曲の速度（テンポランプ）', 'rate', '#fb923c']);
    return rows;
  }

  function drawAuto(G) {
    const cvA = body.querySelector('[data-r=auto]'), want = `${Math.max(200, rowsOf(G).length * 46)}px`;
    if (cvA && cvA.style.height !== want) cvA.style.height = want;     // パートの行が増えたら高く
    const [g, W, H, dp] = bufCtx('auto'), xb = X(G, W);
    g.fillStyle = '#0a0b0d'; g.fillRect(0, 0, W, H);
    const x0 = xb(0), x1 = xb(G.n), rows = rowsOf(G), rh = H / rows.length;
    g.fillStyle = 'rgba(34,197,94,.10)'; g.fillRect(x0, 0, Math.max(dp, x1 - x0), H);
    for (let rel = G.left; rel <= G.left + view; rel += 4) { g.fillStyle = 'rgba(255,255,255,.05)'; g.fillRect(Math.round(xb(rel)), 0, dp, H); }
    const uAt = x => (G.n ? (x - x0) / (x1 - x0) : (x < x0 ? 0 : 1));
    rows.forEach(([name, key, col], r) => {
      const y0 = r * rh + 8 * dp, h = rh - 14 * dp;
      g.fillStyle = 'rgba(255,255,255,.07)'; g.fillRect(0, (r + 1) * rh - dp, W, dp);
      g.strokeStyle = col; g.lineWidth = 2 * dp; g.beginPath();
      for (let x = 0; x <= W; x += 2) {
        const u = clamp(uAt(x), 0, 1);
        // 速度の行: 1.0 と ランプの終わりの速度 の間を 0..1 に（上が速い）
        const v = key === 'rate' ? (() => { const lo = Math.min(1, G.rampTo) - 0.01, hi = Math.max(1, G.rampTo) + 0.01, r = 1 + (G.rampTo - 1) * u; return (r - lo) / (hi - lo); })()
          : key.startsWith('p:') ? partGains(G.eff, u, G.beats)[key.split(':')[1]][+key.split(':')[2]]
          : levels(G.eff, u, G.beats)[key];
        const y = y0 + (1 - v) * h;
        x === 0 ? g.moveTo(x, y) : g.lineTo(x, y);
      }
      g.stroke();
      g.fillStyle = colors['--dim']; g.font = `${11 * dp}px "Hiragino Sans", "Yu Gothic UI", sans-serif`;
      g.fillText(key === 'rate' ? `${name}  ${(100).toFixed(0)}% → ${(G.rampTo * 100).toFixed(1)}%` : name, 8 * dp, r * rh + 18 * dp);
    });
    const ed = EDITABLE[G.eff.tpl], p = G.eff.params;
    if (ed.includes('swap')) {
      const x = Math.round(xb(p.swapAt * G.n));
      g.fillStyle = '#facc15'; g.fillRect(x - dp, 0, 2 * dp, H);
      g.font = `${11 * dp}px "Hiragino Sans", "Yu Gothic UI", sans-serif`; g.fillText('低音の入れ替え', x + 5 * dp, H - 6 * dp);
    }
    if (ed.includes('xf')) {
      const h = rh - 14 * dp, y0 = 8 * dp;
      for (const [u, y] of [[p.xfStart, y0 + h], [p.xfEnd, y0]]) {
        const x = xb(u * G.n), s = 7 * dp;
        g.fillStyle = '#fff'; g.beginPath(); g.moveTo(x, y - s); g.lineTo(x + s, y); g.lineTo(x, y + s); g.lineTo(x - s, y); g.closePath(); g.fill();
      }
    }
  }

  /** 下描きを画面へ写し、試聴・ミックス中なら再生位置の線を重ねる。 */
  function present() {
    if (!ctx) return;
    const ph = playhead?.();
    for (const key of ['ed', 'auto']) {
      const cv = body.querySelector(`[data-r=${key}]`);
      if (!cv) return;
      const [g, W, H, dp] = fitCanvas(cv);
      g.drawImage(buf[key], 0, 0);
      if (ph && ph.pair === ctx.i && Number.isFinite(ph.bar)) {
        const G = ctx.lastG ?? geom(), x = Math.round(X(G, W)(ph.bar));
        g.fillStyle = '#ff3b3b'; g.fillRect(x - dp, 0, 2 * dp, H);
      }
    }
  }

  function runnerChanged() {
    const btn = body.querySelector('[data-r=play]');
    if (!btn || !ctx) return;
    const busy = isBusy?.();
    btn.textContent = busy ? '■ 止める' : '▶ つなぎ目だけ試聴';
    cancelAnimationFrame(raf);
    if (busy) {
      const loop = () => { if (!root.offsetParent || !ctx) return; present(); raf = requestAnimationFrame(loop); };
      raf = requestAnimationFrame(loop);
    } else present();
  }

  // ---- 操作 ----
  function bind() {
    // 小さな波形: 使う範囲
    for (const id of ['A', 'B']) {
      const cv = body.querySelector(`[data-ov=${id}]`);
      const d = () => (id === 'A' ? ctx.da : ctx.db);
      const tm = ev => { const r = cv.getBoundingClientRect(); return clamp((ev.clientX - r.left) / r.width, 0, 1) * d().a.duration; };
      cv.addEventListener('pointerdown', ev => {
        cv.setPointerCapture(ev.pointerId);
        const s = snapTime(d(), tm(ev), 'bar');
        drag = { ov: id, anchor: s, u: { t0: s, t1: s } };
      });
      cv.addEventListener('pointermove', ev => {
        if (drag?.ov !== id) return;
        const s = snapTime(d(), tm(ev), 'bar');
        drag.u = { t0: Math.min(drag.anchor, s), t1: Math.max(drag.anchor, s) };
        drawOv(id, geom());
      });
      cv.addEventListener('pointerup', () => {
        if (drag?.ov !== id) return;
        let { t0, t1 } = drag.u;
        drag = null;
        const dur = d().a.duration;
        if (t1 - t0 < 2) { t0 = 0; t1 = dur; }
        const full = t0 <= 0.05 && t1 >= dur - 0.05;
        queue.patch(ctx.i + (id === 'A' ? 0 : 1), { use_start: full ? null : t0, use_end: full ? null : t1 });
        render();
      });
    }

    // 重ねた波形: 下の曲を左右にドラッグして重なりの長さ
    const ed = body.querySelector('[data-r=ed]');
    ed.addEventListener('pointerdown', ev => {
      if (J().tpl === 'cut') return;
      ed.setPointerCapture(ev.pointerId);
      drag = { bars: true, x: ev.clientX, n: J().bars };
      ed.style.cursor = 'grabbing';
    });
    ed.addEventListener('pointermove', ev => {
      if (!drag?.bars) return;
      const r = ed.getBoundingClientRect(), dBars = Math.round((drag.x - ev.clientX) / (r.width / view));
      const n = clamp(drag.n + dBars, 1, Math.min(64, view - 8));
      if (n !== J().bars) { ctx.A.join_bars = n; render(); }
    });
    ed.addEventListener('pointerup', () => {
      if (!drag?.bars) return;
      drag = null; ed.style.cursor = 'grab';
      view = Math.max(48, J().bars + 16);
      save({ join_bars: J().bars });
    });

    // ミキサーの動き: 低音の入れ替え（縦線）、クロスフェードの始まり・終わり（◆）
    const au = body.querySelector('[data-r=auto]');
    const hitAt = ev => {
      const G = geom(), r = au.getBoundingClientRect(), ed2 = EDITABLE[G.eff.tpl], p = G.eff.params;
      const px = u => r.left + (u * G.n - G.left) * r.width / view;
      const rh = r.height / rowsOf(G).length;
      const cand = [];
      if (ed2.includes('xf') && ev.clientY - r.top < rh) cand.push(['xfStart', Math.abs(ev.clientX - px(p.xfStart))], ['xfEnd', Math.abs(ev.clientX - px(p.xfEnd))]);
      if (ed2.includes('swap')) cand.push(['swapAt', Math.abs(ev.clientX - px(p.swapAt))]);
      const best = cand.filter(c => c[1] < HIT).sort((a, b) => a[1] - b[1])[0];
      return best?.[0] ?? null;
    };
    au.addEventListener('pointerdown', ev => {
      const h = hitAt(ev);
      if (!h) return;
      au.setPointerCapture(ev.pointerId);
      drag = { param: h };
    });
    au.addEventListener('pointermove', ev => {
      if (!drag?.param) { au.style.cursor = hitAt(ev) ? 'ew-resize' : 'default'; return; }
      const G = geom(), r = au.getBoundingClientRect();
      const oneBeat = 1 / G.beats;
      let u = ((ev.clientX - r.left) * view / r.width + G.left) / G.n;
      u = Math.round(clamp(u, 0, 1) * G.beats) / G.beats;         // 拍に吸着
      const p = { ...G.j.params };
      if (drag.param === 'xfStart') p.xfStart = clamp(u, 0, p.xfEnd - oneBeat);
      else if (drag.param === 'xfEnd') p.xfEnd = clamp(u, p.xfStart + oneBeat, 1);
      else p.swapAt = u;
      ctx.A.join_params = p;
      render();
    });
    au.addEventListener('pointerup', () => {
      if (!drag?.param) return;
      drag = null;
      save({ join_params: ctx.A.join_params });
    });

    body.querySelector('[data-r=tpl]').addEventListener('change', e => {
      const tpl = e.target.value;
      view = Math.max(48, TEMPLATES[tpl].bars + 16);
      save({ join_tpl: tpl, join_bars: TEMPLATES[tpl].bars || J().bars });
    });
    body.querySelector('[data-r=bars]').addEventListener('click', e => {
      const b = e.target.closest('[data-d]');
      if (!b || J().tpl === 'cut') return;
      const n = clamp(J().bars + +b.dataset.d, 1, 64);
      view = Math.max(48, n + 16);
      save({ join_bars: n });
    });
    body.querySelector('[data-r=reset]').addEventListener('click', () => save({ join_params: { ...DEFAULT_PARAMS } }));
    body.querySelector('[data-r=vfix]').addEventListener('click', () => {
      const G = ctx.lastG, f = vocalFit(G);
      if (!f.clash0 || f.clash >= f.clash0) return;
      const fields = { join_bars: f.bars };
      if (f.trim) fields.use_end = ctx.da.barTimes[G.aEnd - f.trim];
      save(fields);
      flash?.(`ボーカルの重なりを ${f.clash0} → ${f.clash} 小節に（重なり ${f.bars} 小節${f.trim ? `・前の曲を ${f.trim} 小節早く終える` : ''}）`);
    });
    body.querySelector('[data-r=bfix]').addEventListener('click', () => {
      const f = beatFix(ctx.lastG);
      if (!f) return;
      if (f.useEnd != null) queue.patch(ctx.i, { use_end: f.useEnd });
      if (f.useStart != null) queue.patch(ctx.i + 1, { use_start: f.useStart });
      render();
      flash?.(`拍の取れる所で繋ぐように、${f.what}`);
    });
    for (const k of ['tempo', 'loop', 'shift', 'parts']) {
      body.querySelector(`[data-r=${k}]`).addEventListener('click', e => {
        const x = e.target.closest('[data-v]'); if (!x) return;
        const v = k === 'tempo' || k === 'parts' ? x.dataset.v : +x.dataset.v;
        save({ join_params: { ...J().params, [k]: v } });
      });
    }
    body.querySelector('[data-r=play]').addEventListener('click', () => (isBusy?.() ? onStop() : onPreview('join', ctx.i)));
    // カットイン: 候補を選ぶ・前後の候補へ・音量合わせ
    const cb = body.querySelector('[data-r=cutbox]');
    cb.addEventListener('click', e => {
      const c = e.target.closest('[data-ct]');
      if (c) { setCutPoint(c.dataset.ct, +c.dataset.t); return; }
      const n = e.target.closest('[data-cn]');
      if (n) {
        const [w, dir] = n.dataset.cn.split(':'), kind = w === 'out' ? 'outs' : 'ins', G = ctx.lastG, tr = cutTrim(G);
        const cur = kind === 'outs' ? tr.tCut : tr.tIn;
        const cs = cutCands(G, kind).filter(x => x.score >= CUT_GOOD).sort((a, b) => a.t - b.t);
        const nx = +dir > 0 ? cs.find(x => x.t > cur + 0.05) : cs.reverse().find(x => x.t < cur - 0.05);
        if (nx) setCutPoint(kind, nx.t); else flash?.('その向きには★の候補がありません');
        return;
      }
      const v = e.target.closest('[data-r=trim] [data-v]');
      if (v) { save({ join_params: { ...J().params, cutTrim: v.dataset.v } }); return; }
      const st = e.target.closest('[data-tr]');
      if (st) {
        const now = cutTrim(ctx.lastG).db;
        const x = Math.max(-TRIM_MAX_DB, Math.min(TRIM_MAX_DB, Math.round((now + +st.dataset.tr) * 2) / 2));
        save({ join_params: { ...J().params, cutTrim: x } });
      }
    });
  }

  addEventListener('resize', () => { if (ctx && root.offsetParent) render(); });
  return { show, runnerChanged, get current() { return cur; } };
}
