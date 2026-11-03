// エントリポイント。画面・再生エンジン・ライブラリ・ミックスキュー・アクションをつなぐ。
import { initScale, uiScale } from './ui/scale.js';
import { initTabs, initQueueToggle, toggleQueue } from './ui/layout.js';
import { renderDeck } from './ui/deck-view.js';
import { renderMixer } from './ui/mixer-view.js';
import { renderFlags, setTrackInfo, updateDeckNumbers, drawZoom, drawOverviews, localBpm } from './ui/strip.js';
import { initLibraryView } from './ui/library-view.js';
import * as queue from './queue.js';
import * as analysis from './analysis.js';
import * as actions from './input/actions.js';
import { initKeyboard } from './input/keyboard.js';
import { Midi, modeLabel } from './input/midi.js';
import { api } from './api.js';
import { AudioEngine, peakOf } from './engine/audio-engine.js';
import { SyncController } from './engine/sync.js';
import { Runner } from './engine/runner.js';
import { runDiagnostics } from './diag.js';
import { renderSettings, putSettings, outputDevices } from './ui/settings-view.js';
import { initJoinsView, forgetTrack, trackData, useOf } from './ui/joins-view.js';
import { initJoinEditor } from './ui/editor-view.js';
import { initBulkBar, reportPlaying } from './ui/bulk-bar.js';
import { initAutoView } from './ui/auto-view.js';
import { initMixesView, askMixName } from './ui/mixes-view.js';
import { openCandidates } from './ui/candidates.js';
import { openExport, reveal as revealPath } from './ui/export-dialog.js';
import { MixRecorder } from './engine/recorder.js';
import { openModal, closeModal } from './ui/modal.js';
import { checkFirstRun } from './ui/first-run.js';

const css = n => getComputedStyle(document.documentElement).getPropertyValue(n).trim();
const COLORS = Object.fromEntries(['--deck-a', '--deck-b', '--wave-low', '--wave-mid', '--wave-high', '--line', '--dim',
  '--faint', '--active', '--warn', '--ok'].map(n => [n, css(n)]));

export const ui = {};
export let engine = null;
export let sync = null;
export let midi = null;
export let runner = null;

// 画面側のエラーをサーバーのログ（data/djmix.log）に送る
function reportError(message, detail) {
  fetch('/api/log', { method: 'POST', headers: { 'Content-Type': 'application/json' },
                      body: JSON.stringify({ level: 'error', message: String(message), detail: detail ? String(detail) : null }) })
    .catch(() => {});
}
addEventListener('error', e => reportError(e.message, `${e.filename}:${e.lineno}:${e.colno} ${e.error?.stack ?? ''}`));
addEventListener('unhandledrejection', e => reportError(e.reason?.message ?? e.reason, e.reason?.stack));

let settings = {};

async function init() {
  settings = await fetch('/api/settings').then(r => r.json()).catch(() => ({}));
  initScale();
  initTabs();
  initQueueToggle(settings.queue_visible, v => applySettings({ queue_visible: v }));
  ui.deckView = { A: renderDeck(document.getElementById('deckA'), 'A'), B: renderDeck(document.getElementById('deckB'), 'B') };
  ui.mixer = renderMixer(document.getElementById('mixer'), {
    colors: { A: COLORS['--deck-a'], B: COLORS['--deck-b'] },
    onChange: (act, v) => actions.set(act, v),
  });
  for (const d of ['A', 'B']) { renderFlags(d); setTrackInfo(d, null); }

  // ライブラリとミックスキュー（P1-2）
  queue.initQueueInteractions({
    onOpenItem: i => { joinMode('range'); openTab('joinview'); ui.joins.show(i); },
    onOpenJoin: i => { joinMode('edit'); openTab('joinview'); openJoinEditor(i); },
    // キューの曲ごとの「合う曲」: その曲の次に合う曲（後ろに曲があれば前後の両方に合う曲）を選び、すぐ後ろに入れる
    onMatch: i => {
      const items = queue.getItems(), it = items[i], nx = items[i + 1];
      if (!it) return;
      openCandidates({
        title: nx ? `「${it.track.title}」と「${nx.track.title}」の間に入れる曲` : `「${it.track.title}」の次に合う曲`,
        prev: it.track_id, next: nx?.track.analysis_status === 'done' ? nx.track_id : null, groups: [], colors: COLORS, previewTrack,
        exclude: items.map(x => x.track_id), pickLabel: nx ? 'ここに入れる' : '＋キュー',
        fullTrack: async id => (await api.tracks([id])).items[0],
        onPick: async c => {
          const t = (await api.tracks([c.id])).items[0];
          // 選んだ時点のキューで、元の曲のすぐ後ろへ（候補を見ている間にキューが変わっても）
          const at = queue.getItems().findIndex(x => x === it);
          queue.insert(at >= 0 ? at + 1 : queue.getItems().length, t);
          flash(`「${c.title}」を「${it.track.title}」の後ろに入れました`);
        },
      });
    },
  });
  queue.loadQueue().then(() => {
    // キューに入っている未解析の曲は解析に回す（ユーザー回答「使う曲だけ解析」）
    for (const it of queue.getItems()) if (it.track.analysis_status !== 'done') analysis.request(it.track_id, analysis.PRIO.QUEUE);
  });
  analysis.onTrackUpdated(t => {
    queue.updateTrack(t);
    ui.library?.updateRow(t);
    for (const id of ['A', 'B']) {
      const dk = engine?.decks[id];
      if (dk?.track?.id === t.id && t.analysis_status === 'done' && !dk.analysis) attachAnalysis(dk);
    }
  });
  ui.library = initLibraryView({
    onAddToQueue: t => { queue.add(t); if (t.analysis_status !== 'done') analysis.request(t.id, analysis.PRIO.QUEUE); },
    isQueued: id => queue.getItems().some(it => it.track_id === id),
    onLoadDeck: (deck, t) => loadDeck(deck, t),
    // この曲に合う曲（Phase 3 P3-6。次の曲の候補リスト＝案Y）。何曲でもキューに足せる
    onMatch: t => openCandidates({
      title: `「${t.title}」の次に合う曲`, prev: t.id, next: null, groups: [], colors: COLORS, previewTrack,
      exclude: queue.getItems().map(it => it.track_id), pickLabel: '＋キュー',
      fullTrack: async id => (await api.tracks([id])).items[0],
      onPick: async c => { queue.add((await api.tracks([c.id])).items[0]); flash(`「${c.title}」をキューに足しました`); return false; },
    }),
  });
  document.getElementById('qadd').addEventListener('click', () => document.querySelector('.tab[data-v=libview]').click());
  document.getElementById('qomakase').addEventListener('click', () => openTab('autoview'));
  // おまかせタブ（Phase 3）: 上部にライブラリのまとめて解析の帯
  let groupsTimer = null;
  ui.bulk = initBulkBar(document.getElementById('bulkbar'), { flash, onDone: () => {
    clearTimeout(groupsTimer); groupsTimer = setTimeout(() => ui.auto?.refreshGroups(), 3000);      // 解析済みの数を更新
  } });
  document.getElementById('qsave').addEventListener('click', () => {
    if (!queue.getItems().length) { flash('キューが空です'); return; }
    askMixName({ name: stamp('ミックス'), onOk: async name => {
      const m = await saveMix(name, 'hand');
      flash(`「${m.name}」としてミックスのタブに残しました`);
    } });
  });
  // キューのクリア（I-2）。確かめてから空にする（通し再生・試聴中なら止める）
  const clearQueue = async () => {
    if (runner?.busy) runner.stop(runner.mode === 'mix' ? 'ミックスを止めました' : '試聴を止めました');
    queue.setAll([], 'replace');
    queue.playState.played.clear();
    queue.refresh();
    settings.current_mix = null;
    putSettings({ current_mix: null }).catch(() => {});
    ui.joins?.runnerChanged?.();
    flash('キューを空にしました');
  };
  document.getElementById('qclear').addEventListener('click', () => {
    const n = queue.getItems().length;
    if (!n) { flash('キューはもう空です'); return; }
    const playing = runner?.busy ? '<p class="lbl2" style="color:var(--warn)">再生中のミックス（試聴）は止まります。</p>' : '';
    openModal(`<h3>ミックスキューをクリア</h3><p>キューの ${n} 曲と、曲ごとの使う範囲・つなぎの設定を消します。デッキに読み込んだ曲はそのままです。</p>${playing}
      <div class="mfoot"><div class="btn" data-close>やめる</div><span class="sp"></span>
        <div class="btn" data-save>ミックスとして保存してからクリア</div><div class="btn primary" data-clear>クリアする</div></div>`, m => {
      m.querySelector('[data-clear]').addEventListener('click', () => { closeModal(); clearQueue(); });
      m.querySelector('[data-save]').addEventListener('click', () => {
        closeModal();
        askMixName({ name: stamp('ミックス'), onOk: async name => {
          const mx = await saveMix(name, 'hand');
          await clearQueue();
          flash(`「${mx.name}」として残してから、キューを空にしました`);
        } });
      });
    });
  });
  // 「空いたデッキに次の曲を自動で読み込む」（曲が最後まで終わったデッキに、キューの次の曲を読み込む）
  const qauto = document.getElementById('qauto');
  const showAuto = () => { qauto.querySelector('i').textContent = settings.queue_autoload !== false ? '✓' : ''; };
  showAuto();
  qauto.addEventListener('click', async () => { await applySettings({ queue_autoload: settings.queue_autoload === false }); showAuto(); });
  document.getElementById('qloadnext').addEventListener('click', () => {
    const i = queue.nextIndex();
    if (i >= 0) loadDeck(null, queue.getItems()[i].track);
    else flash('キューに次の曲がありません');
  });

  // 再生エンジン（P1-4）
  engine = await new AudioEngine().init();
  sync = await new SyncController(engine).init();
  sync.on(() => { for (const id of ['A', 'B']) refreshFlags(id); actions.notify('sync'); });
  addEventListener('pointerdown', () => engine.resume(), { capture: true });
  for (const id of ['A', 'B']) { setupDeck(id); setupChannel(id); }
  bindAbsolute('mixer.crossfader', ui.mixer.faders['mixer.crossfader'], v => engine.mixer.setCrossfader(v));
  actions.register('mixer.crossfader.curve', { type: 'absolute', set: c => applySettings({ xfade_curve: c }) });
  actions.register('ui.queue.toggle', { type: 'toggle', toggle: toggleQueue });
  actions.bindButtons(document.getElementById('deckview'));
  // 右クリック: ホットキューのパッドなら削除（P1-9 で MIDI 割り当てのメニューに拡張する）
  actions.setContextHandler((name, el, ev) => openContextMenu(name, el, ev));
  initKeyboard();
  await applySettings(settings, false);
  document.querySelector('.tab[data-v=setview]').addEventListener('click', () => showSettings());
  // 繋ぎの実行（Phase 2）
  runner = new Runner({ engine, sync, trackData, useOf, loadDeck, getItems: queue.getItems, settings: () => settings, flash,
                        markPlayed: id => { queue.playState.played.add(id); queue.refresh(); recordPlays([id], 'mix'); },
                        onChange: () => { ui.joins?.runnerChanged?.(); ui.editor?.runnerChanged(); mixButtons(); } });
  // ミックスを通して再生（P2-7）
  const qmix = document.getElementById('qmix'), qstop = document.getElementById('qstop');
  function mixButtons() {
    if (recorder?.active && !runner.busy) finishRecording();     // ミックスが終わった・止めた → 録音もそこまで
    qmix.classList.toggle('on', runner.mode === 'mix');
    qmix.textContent = runner.mode === 'mix' ? `▶ ミックスを再生中（${(runner.mixIndex ?? 0) + 1}曲目）` : '▶ ミックスを通して再生';
  }
  qmix.addEventListener('click', () => {
    if (runner.mode === 'mix') return;
    queue.playState.played.clear();
    runner.playMix(0);
    queue.flush().then(() => fetch('/api/mixes/played', { method: 'POST' })).catch(() => {});   // ミックスのままなら再生回数を数える
  });
  // 書き出し（Phase 3 P3-7・P3-8）
  recorder = new MixRecorder(engine);
  document.getElementById('qexport').addEventListener('click', () => openExport({
    queueItems: queue.getItems, mixMinutes, onRecord: startRecording, onFast: fastExport, onCancelFast: () => fetch('/api/export/cancel', { method: 'POST' }),
  }));
  qstop.addEventListener('click', () => { if (runner.busy) runner.stop(runner.mode === 'mix' ? 'ミックスを止めました' : '試聴を止めました'); });
  addEventListener('keydown', e => { if (e.key === 'Escape' && runner.busy) runner.stop(runner.mode === 'mix' ? 'ミックスを止めました' : '試聴を止めました'); });
  // つなぎ目タブ（Phase 2）
  // つなぎ目タブは「範囲選択（案A）」と「つなぎ目エディタ（案B）」を切り替えて使う
  const jv = document.getElementById('joinview');
  jv.innerHTML = '<div class="jsub" data-sub="range"></div><div class="jsub" data-sub="edit" hidden></div>';
  ui.jsub = { range: jv.querySelector('[data-sub=range]'), edit: jv.querySelector('[data-sub=edit]') };
  ui.joins = initJoinsView({ root: ui.jsub.range, colors: COLORS, onPreview: previewRange, onEditJoin: openJoinEditor });
  ui.editor = initJoinEditor({ root: ui.jsub.edit, colors: COLORS, onPreview: previewRange, flash, onStop: () => runner.stop('試聴を止めました'),
                               onBack: i => { joinMode('range'); ui.joins.show(i); }, playhead: () => runner.playhead(), isBusy: () => runner.busy,
                               lufsTarget: () => engine.mixer.targetLufs });
  // おまかせタブ（Phase 3 P3-5）
  ui.auto = initAutoView({ root: document.getElementById('autobody'), colors: COLORS, queue, flash, previewTrack,
                           previewJoin: (i, items) => runner.previewJoin(i, items),
                           openJoins: () => { joinMode('range'); openTab('joinview'); },
                           onToQueue: async ({ flow }) => { await saveMix(stamp(`おまかせ（${flow}）`), 'auto'); } });
  // ミックスのタブ（Phase 4 P4-2）
  ui.mixes = initMixesView({ root: document.getElementById('mixview'), queue, flash, settings: () => settings,
                             applySettings: p => applySettings(p), playMix: () => document.getElementById('qmix').click(),
                             exportMix: () => document.getElementById('qexport').click() });
  document.querySelector('.tab[data-v=mixview]').addEventListener('click', () => ui.mixes.show());
  document.querySelector('.tab[data-v=autoview]').addEventListener('click', () => ui.auto.show());
  document.querySelector('.tab[data-v=joinview]').addEventListener('click', () => {
    if (ui.jsub.edit.hidden) ui.joins.show(Math.max(0, ui.joins.current)); else ui.editor.show(Math.max(0, ui.editor.current));
  });
  // MIDI コントローラー（P1-9）
  midi = await new Midi({ flash, onPorts: p => {
    const el = document.getElementById('midist');
    el.textContent = !p.available ? '使用不可' : p.inputs.length ? p.inputs.join(', ') : '未接続';
  } }).init();
  for (const id of ['A', 'B']) {
    document.getElementById(`ov${id}`).addEventListener('pointerdown', e => {
      const dk = engine.decks[id], r = e.currentTarget.getBoundingClientRect();
      if (dk.track) dk.seek((e.clientX - r.left) / r.width * dk.duration);
    });
  }

  requestAnimationFrame(frame);
  runDiagnostics();
  checkFirstRun({ apply: p => applySettings(p), flash }).catch(e => console.error(e));   // 音楽フォルダがまだ無ければ選んでもらう
}

function setupDeck(id) {
  const dk = engine.decks[id];
  const view = ui.deckView[id];
  const A = n => `deck.${id}.${n}`;
  actions.register(A('play'), { type: 'toggle', toggle: () => dk.toggle(), state: () => dk.playing });
  actions.register(A('cue'), { type: 'trigger', down: () => dk.cueDown(), up: () => dk.cueUp() });
  actions.register(A('keylock'), { type: 'toggle', toggle: () => dk.setKeylock(!dk.keylock), state: () => dk.keylock });
  bindAbsolute(A('tempo'), view.tempoFader, v => {
    dk.tempoFader = (0.5 - v) * 2;               // 上が遅い（CDJ と同じ向き）
    if (sync.st[id].on) return;                  // SYNC 中のスレーブはマスターに従う
    dk.setTempoFader(dk.tempoFader);
  });
  view.tempoFader.onChange = v => actions.set(A('tempo'), v);   // 画面のフェーダーを動かしたとき
  // ナッジ: 押している間だけ ±4%（SYNC 中は位相を少しずつずらす）
  for (const [n, dir] of [['nudge.minus', -1], ['nudge.plus', 1]]) {
    actions.register(A(n), { type: 'trigger', down: () => nudge(id, dir), up: () => nudge(id, 0) });
  }
  // ジョグ（相対値）: 止まっていれば少しずつ送る、再生中は一時的に速度を変える（SYNC 中は位相をずらす）
  actions.register(A('jog'), { type: 'relative', step: d => jog(id, d) });
  actions.register(A('beatjump.back'), { type: 'trigger', down: () => sync.st[id].on ? sync.shiftBeat(id, -1) : beatJump(dk, -1) });
  actions.register(A('beatjump.fwd'), { type: 'trigger', down: () => sync.st[id].on ? sync.shiftBeat(id, 1) : beatJump(dk, 1) });
  actions.register(A('sync'), { type: 'toggle', state: () => sync.st[id].on, toggle: () => {
    if (sync.st[id].on) { sync.disable(id); return; }
    const r = sync.enable(id);
    if (!r.ok) flash(`SYNC できません: ${r.reason}`);
  } });
  actions.register(A('master'), { type: 'toggle', state: () => sync.masterId() === id && (dk.playing || !!dk.track), toggle: () => sync.setMaster(id) });
  actions.register(A('tempo.half'), { type: 'trigger', down: () => sync.st[id].on ? sync.setK(id, sync.st[id].k / 2 < 0.5 ? 0.5 : sync.st[id].k / 2) : flash('×½ は SYNC 中に使えます') });
  actions.register(A('tempo.double'), { type: 'trigger', down: () => sync.st[id].on ? sync.setK(id, sync.st[id].k * 2 > 2 ? 2 : sync.st[id].k * 2) : flash('×2 は SYNC 中に使えます') });
  actions.register(A('grid.downbeat'), { type: 'trigger', down: () => setDownbeatHere(id) });
  actions.register(A('grid.meter'), { type: 'trigger', down: () => openGridMenu(id) });
  // ループ（P1-8）
  dk.loopUnit = 'beat';
  actions.register(A('loop.unit.beat'), { type: 'toggle', toggle: () => { dk.loopUnit = 'beat'; actions.notify(A('loop.unit.bar')); }, state: () => dk.loopUnit === 'beat' });
  actions.register(A('loop.unit.bar'), { type: 'toggle', toggle: () => { dk.loopUnit = 'bar'; actions.notify(A('loop.unit.beat')); }, state: () => dk.loopUnit === 'bar' });
  for (const x of LOOP_SIZES) {
    actions.register(A(`loop.${x}`), { type: 'toggle', toggle: () => toggleLoop(id, x),
      state: () => !!dk.loop?.active && dk.loop.size === x && dk.loop.unit === dk.loopUnit });
  }
  actions.register(A('loop.exit'), { type: 'trigger', down: () => dk.exitLoop() });
  // ホットキュー（P1-8）: 空なら今の拍を登録、あれば拍の中の位置を保ったまま飛ぶ。Shift＋クリック／右クリックで削除
  for (const i of [0, 1, 2, 3]) {
    actions.register(A(`hotcue.${i + 1}`), { type: 'trigger', down: info => hotcue(id, i, info) });
  }
  dk.on(what => {
    if (what === 'state' || what === 'tempo') actions.notify(A('play'));
    if (what === 'load' || what === 'analysis') {
      // 解析が終わったら、キーは解析結果の値で表示する（読み込み時点のライブラリ情報は未解析のことがある）
      setTrackInfo(id, dk.track && dk.analysis ? { ...dk.track, camelot: dk.analysis.key?.camelot } : dk.track);
      deckStatus(id); meterLabel(id);
    }
    if (what === 'state' && dk.playing) dk.hasPlayed = true;
    if (what === 'state') reportPlaying(engine.decks.A.playing || engine.decks.B.playing);   // 再生中はまとめて解析を控えめに
    if (what === 'state' && dk.playing && recorder?.active) recorder.mark(dk.track);          // 曲目リストの開始時刻
    if (what === 'loop' || what === 'seek' || what === 'load') refreshLoopButtons(id);
    if (what === 'analysis' || what === 'load' || what === 'hotcue') refreshPads(id);
    if (what === 'cue' && dk.analysis) api.patchEdits(dk.track.id, { cue_point: dk.cue }).catch(() => {});   // キュー位置も曲ごとに保存
    if (what === 'ended') onTrackEnded(id);
    refreshFlags(id);
    if (what === 'tempo') {
      const pct = (dk.rate - 1) * 100;
      view.root.querySelector('[data-role=tempoval]').textContent = `${pct >= 0 ? '+' : ''}${pct.toFixed(1)}%`;
    }
  });
}

function refreshFlags(id) {
  const dk = engine.decks[id];
  renderFlags(id, { master: !!dk.track && sync.masterId() === id, sync: sync.st[id].on, keylock: dk.keylock });
  actions.notify(`deck.${id}.sync`);
  actions.notify(`deck.${id}.master`);
}

/** 位相メーターの表示内容（中央帯）。 */
const TPL_SHORT = { long: 'ロングブレンド', short: 'ショート', bass: '低音入れ替え', filter: 'フィルター', echo: 'エコーアウト', cut: 'カットイン' };
function phaseInfo() {
  const r = runner?.status();
  if (r?.phase) {
    const m = sync.masterId(), bm = sync.bm[m], M = engine.decks[m];
    const info = bm && M.track ? bm.beatInBar(bm.pos(M.position())) : {};
    if (r.phase === 'last') return { ...info, text: `ミックスを再生中  ${r.outTitle}（最後の曲。使う範囲の終わりで止めます）` };
    const what = r.phase === 'pre' ? '重なりの前' : r.phase === 'overlap' ? `重なりの中 ${Math.round(r.u * 100)}%` : '繋ぎ終わり（原速へ戻しています）';
    const man = r.manual.length ? `  手動: ${r.manual.length}` : '';
    const beatAxis = r.phase === 'overlap' && !!sync.bm.A && !!sync.bm.B && (sync.st.A.on || sync.st.B.on);
    return { ...info, beatAxis, bm: sync.bm, warn: !!r.reason,
             text: `${r.mode === 'preview' ? 'つなぎ目を試聴中' : 'ミックスを再生中'}  ${r.outTitle} → ${r.inTitle}  ${TPL_SHORT[r.tpl] ?? ''} ${r.bars}小節  ${what}${man}${r.reason ? '  （' + r.reason + '）' : ''}` };
  }
  const m = sync.masterId(), s = m === 'A' ? 'B' : 'A';
  const M = engine.decks[m], st = sync.st[s];
  const bm = sync.bm[m];
  if (!bm || !M.track) return null;
  const info = bm.beatInBar(bm.pos(M.position()));
  const beatAxis = st.on && !!sync.bm.A && !!sync.bm.B;
  Object.assign(info, { beatAxis, bm: sync.bm });
  if (!st.on) return { ...info, text: `マスター ${m}  時間軸表示` };
  if (st.hold) return { ...info, text: `SYNC 保留: ${st.hold}`, warn: true };
  if (engine.decks[s].loop?.active) return { ...info, text: `SYNC ${s}→${m}  拍軸表示  ${s} はループ中` };
  const e = sync.phaseErrorMs(s);
  const k = st.k !== 1 ? `  ×${st.k}` : '';
  const settling = Math.abs(st.phiTarget - st.phi) > 0.01 ? '  位相を合わせています…' : '';
  return { ...info, text: `SYNC ${s}→${m}${k}  拍軸表示  位相のずれ ${e === null ? '—' : (e >= 0 ? '+' : '') + e.toFixed(1) + ' ms'}${settling}` };
}

// ---- つなぎ目（Phase 2） ----
function openTab(v) { document.querySelector(`.tab[data-v=${v}]`).click(); }

/** 範囲の試聴: 空いているデッキに読み込み、範囲の頭（または終わりの 8 小節前）から再生する。 */
async function previewRange(kind, i) {
  if (kind === 'join') {
    const items = queue.getItems();
    flash(`つなぎ目を試聴します: ${items[i]?.track.title} → ${items[i + 1]?.track.title}（Esc で止める）`, 4000);
    runner.previewJoin(i);
    return;
  }
  const it = queue.getItems()[i];
  if (!it) return;
  await previewTrack(it.track, useOf(it, await trackData(it.track.id)), kind);
}

/** 曲の使う範囲 u を、空いているデッキで試聴する（kind: 'head' 範囲の頭から / 'tail' 終わりの 8 小節前から）。 */
async function previewTrack(track, u, kind = 'head') {
  const d = await trackData(track.id);
  const D = engine.decks;
  let id = ['A', 'B'].find(x => D[x].track?.id === track.id);
  if (!id) {
    id = !D.A.playing ? 'A' : !D.B.playing ? 'B' : null;
    if (!id) { flash('両方のデッキが再生中です。止めてから試聴してください'); return; }
    await loadDeck(id, track);
  }
  const dk = D[id];
  for (let k = 0; k < 40 && dk.loadedSec < Math.min(dk.duration, u.t1) - 0.5; k++) await new Promise(r => setTimeout(r, 100));
  const bi = d.barTimes.findIndex(t => t >= u.t1 - 0.05);
  const start = kind === 'tail' ? d.barTimes[Math.max(0, (bi < 0 ? d.barTimes.length : bi) - 8)] : u.t0;
  dk.seek(start);
  if (!dk.playing) dk.play();
  flash(`デッキ${id}: ${track.title} を ${kind === 'tail' ? '範囲の終わりの 8 小節前' : '範囲の頭'}から再生します`);
}

// ---- 書き出し（Phase 3） ----
let recorder = null, recOpt = null, recTimer = null;
const TPL_BARS_EX = { long: 16, short: 8, bass: 16, filter: 16, echo: 2, cut: 0 };
/** キューのミックスのおおよその長さ（分）。 */
function mixMinutes() {
  const its = queue.getItems();
  let sec = 0;
  its.forEach((it, i) => {
    const t0 = it.use_start ?? 0, t1 = it.use_end ?? it.track.duration ?? 0;
    sec += t1 - t0;
    if (i + 1 < its.length) sec -= (it.join_bars ?? TPL_BARS_EX[it.join_tpl ?? 'long'] ?? 16) * 4 * 60 / (it.track.bpm || 120);
  });
  return Math.max(0, sec / 60);
}
async function startRecording(opt) {
  if (runner.busy) runner.stop();
  try {
    const path = await recorder.start({ folder: opt.folder, name: opt.name, format: opt.format });
    recOpt = opt;
    const st = document.getElementById('recstat');
    st.hidden = false;
    const total = mixMinutes() * 60;
    const fmt = s => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, '0')}`;
    recTimer = setInterval(() => { st.innerHTML = `<b>● 録音中</b> ${fmt(recorder.elapsed)} / 約${fmt(total)}　${path}`; }, 500);
    queue.playState.played.clear();
    await runner.playMix(0);
    if (!runner.busy) finishRecording();
  } catch (e) { flash(String(e.message ?? e), 5000); }
}
async function finishRecording() {
  if (!recorder.active) return;
  clearInterval(recTimer);
  const st = document.getElementById('recstat');
  st.textContent = '録音を保存しています…';
  try {
    const r = await recorder.stop(recOpt?.tracklist !== false);
    const sec = Math.round(r.seconds);          // 先に丸める（119.9 秒が「1分60秒」にならないように）
    st.innerHTML = `録音を保存しました（${Math.floor(sec / 60)}分${sec % 60}秒）: ${r.path} <span class="btn" data-reveal style="height:24px;font-size:12px;padding:0 8px;min-width:0">📂 フォルダを開く</span>`;
    st.querySelector('[data-reveal]').addEventListener('click', () => revealPath(r.path).catch(e => flash(e.message)));
    afterExport({ ...r, format: recOpt?.format });
    flash('録音を保存しました', 4000);
  } catch (e) { st.textContent = `録音の保存に失敗しました: ${e.message}`; }
}
/** 高速で書き出す（サーバーで計算。P3-8）。onProgress({progress, eta_sec, message})。 */
async function fastExport(opt, onProgress) {
  const items = queue.getItems().map(({ track, ...rest }) => rest);
  // ストレッチは再生と同じエンジン（画面の WebAssembly）で音を出さずに計算し、サーバーはミキサー以降を計算する（2026-10-02）
  const r = await fetch('/api/export/render', { method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ ...opt, items, engine: 'browser', settings: { lufs_target: settings.lufs_target ?? -14, xfade_curve: settings.xfade_curve ?? 'smooth', tempo_return: settings.tempo_return !== false } }) });
  const j = await r.json();
  if (!r.ok) throw new Error(j.detail ?? `書き出しを始められませんでした（${r.status}）`);
  let rendering = null;
  for (;;) {
    await new Promise(res => setTimeout(res, 500));
    const s = await fetch('/api/export/status').then(x => x.json());
    if (s.error) throw new Error(s.error);
    if (s.stage === 'need_audio' && !rendering) {
      const { renderAll } = await import('./engine/offline-render.js');
      rendering = renderAll(null).catch(e => { console.error(e); flash(`画面での計算に失敗したので、予備の方式で書き出します: ${e.message}`, 5000); });
    }
    onProgress(s);
    if (s.done) { afterExport({ ...s.result, format: opt.format }); return s.result; }
  }
}

// ---- ミックスの保存・好み（Phase 4） ----
function stamp(label) {
  const d = new Date(), p = x => String(x).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}${p(d.getMinutes())} ${label}`;
}
/** 今のキューをミックスとして残す（キューはそのミックスにつながる）。 */
async function saveMix(name, kind) {
  await queue.flush();
  const m = await fetch('/api/mixes', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ name, kind }) }).then(r => r.json());
  settings.current_mix = m.id;
  ui.mixes?.refresh();
  return m;
}
/** 書き出しの後: ミックスに記録（キューを変えていれば新しいミックスとして残す）。 */
async function afterExport(res) {
  try {
    await queue.flush();
    const m = await fetch('/api/mixes/after-export', { method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path: res.path, format: res.format, seconds: res.seconds, tracklist: res.tracklist }) }).then(r => r.json());
    settings.current_mix = m.id;
    ui.mixes?.refresh();
  } catch (e) { console.error(e); }
}
function recordPlays(ids, source) {
  fetch('/api/plays', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ track_ids: ids, source }) }).catch(() => {});
}
// デッキで 30 秒以上鳴った曲を「かけた」と記録する（5 秒ごとに数える。ミックスの通し再生中は runner が記録する）
const deckPlay = { A: { id: null, sec: 0, done: false }, B: { id: null, sec: 0, done: false } };
setInterval(() => {
  if (!engine) return;
  for (const id of ['A', 'B']) {
    const dk = engine.decks[id], st = deckPlay[id];
    if (dk.track?.id !== st.id) Object.assign(st, { id: dk.track?.id ?? null, sec: 0, done: false });
    if (dk.playing && st.id != null && !st.done) {
      st.sec += 5;
      if (st.sec >= 30 && !runner?.busy) { st.done = true; recordPlays([st.id], 'deck'); }
    }
  }
}, 5000);

function joinMode(m) { ui.jsub.range.hidden = m !== 'range'; ui.jsub.edit.hidden = m !== 'edit'; }
function openJoinEditor(i) {
  joinMode('edit');
  ui.editor.show(i);
}

// ---- 設定（P1-10） ----
/** 設定を反映する。save=true ならサーバーにも保存する。 */
async function applySettings(patch, save = true) {
  if (save) settings = await putSettings(patch);
  else settings = { ...settings, ...patch };
  const s = settings;
  engine.mixer.targetLufs = s.lufs_target ?? -14;
  for (const id of ['A', 'B']) {
    const dk = engine.decks[id];
    if ('lufs_target' in patch && dk.analysis) deckStatus(id);                       // 補正量を計算し直す
    if ('tempo_range' in patch) {
      dk.setTempoRange(s.tempo_range);
      ui.deckView[id].root.querySelector('[data-role=range]').textContent = `±${Math.round(s.tempo_range * 100)}%`;
    }
  }
  if ('xfade_curve' in patch) {
    engine.mixer.setCurve(s.xfade_curve);
    document.querySelectorAll('[data-curve]').forEach(x => x.classList.toggle('on', x.dataset.curve === s.xfade_curve));
  }
  if ('output_device' in patch) await setOutput(s.output_device);
  if ('music_roots' in patch && save) ui.library?.rootsChanged();
}

async function setOutput(deviceId) {
  const el = document.getElementById('outdev');
  if (!('setSinkId' in AudioContext.prototype)) {        // Mac（WKWebView）: 出力は Mac の「サウンド」設定で選ぶ
    el.textContent = 'Mac の出力';
    return;
  }
  try {
    await engine.ctx.setSinkId(deviceId || '');
    const d = (await outputDevices()).find(x => x.deviceId === deviceId);
    el.textContent = deviceId ? (d?.label || '選択したデバイス') : '既定のデバイス';
  } catch (e) {
    el.textContent = '既定のデバイス';
    flash(`出力デバイスを切り替えられません: ${e.message}`);
  }
}

function showSettings() {
  renderSettings(document.getElementById('setview'), { settings, midi, flash, apply: p => applySettings(p),
    onItunes: () => { ui.library?.refreshFolders(); ui.library?.reload(); ui.auto?.refreshGroups(); } });
}

// ---- ループ・ホットキュー（P1-8） ----
const LOOP_SIZES = ['1/4', '1/2', '1', '2', '4', '8', '16'];
const sizeBeats = x => (x.includes('/') ? 1 / +x.split('/')[1] : +x);

/** ループの設定と解除。長さは「拍」か「小節」単位。いまの拍（小節単位なら小節の頭）から始める。 */
function toggleLoop(id, size) {
  const dk = engine.decks[id], bm = sync.bm[id];
  if (!bm) { flash('解析が終わってから使えます'); return; }
  if (dk.loop?.active && dk.loop.size === size && dk.loop.unit === dk.loopUnit) { dk.exitLoop(); return; }
  const p = bm.pos(dk.position(engine.now + 0.05));
  let startBeat, lenBeats;
  if (dk.loopUnit === 'bar') {
    const bb = bm.beatInBar(p);
    startBeat = Math.floor(p - bb.beat + 1e-6);
    lenBeats = sizeBeats(size) * bb.len;
  } else {
    lenBeats = sizeBeats(size);
    startBeat = lenBeats < 1 ? Math.floor(p / lenBeats) * lenBeats : Math.floor(p);   // 1 拍未満のループは拍の中の区切りから
  }
  dk.setLoop(bm.time(startBeat), bm.time(startBeat + lenBeats), lenBeats);
  Object.assign(dk.loop, { size, unit: dk.loopUnit });
  refreshLoopButtons(id);
}

function refreshLoopButtons(id) {
  for (const x of LOOP_SIZES) actions.notify(`deck.${id}.loop.${x}`);
}

async function hotcue(id, i, info) {
  const dk = engine.decks[id];
  if (!dk.track) return;
  if (info.shift) {
    if (dk.hotcues[i] == null) return;
    dk.hotcues[i] = null;
    flash(`ホットキュー ${i + 1} を削除しました`);
  } else if (dk.hotcues[i] == null) {
    dk.hotcues[i] = dk.snapToBeat(dk.position());
  } else {
    dk.jumpQuantized(dk.hotcues[i], sync.bm[id]);
    return;
  }
  dk.emit('hotcue');
  if (dk.analysis) await api.patchEdits(dk.track.id, { hotcues: dk.hotcues });
}

function refreshPads(id) {
  const dk = engine.decks[id];
  ui.deckView[id].root.querySelectorAll('[data-act*=".hotcue."]').forEach((el, i) => el.classList.toggle('set', dk.hotcues[i] != null));
}

// ---- 拍・小節の修正（P1-7） ----
/** 修正を保存し、反映後の解析結果でデッキ（と SYNC の拍位置）を更新する。 */
async function saveEdits(id, body) {
  const dk = engine.decks[id];
  if (!dk.track || !dk.analysis) { flash('解析が終わってから使えます'); return null; }
  const track = dk.track;
  const a = await api.patchEdits(track.id, body);
  forgetTrack(track.id);
  if (dk.track !== track) return null;
  dk.setAnalysis(a, dk.wave);
  const t = (await api.tracks([track.id])).items[0];
  if (t) { queue.updateTrack(t); ui.library?.updateRow(t); }
  return a;
}

/** 再生位置にいちばん近い拍を 1 拍目にする（その拍を含む区間だけ。design_phase1.md §5.1）。 */
async function setDownbeatHere(id) {
  const dk = engine.decks[id], a = dk.analysis;
  if (!a) { flash('解析が終わってから使えます'); return; }
  const p = dk.position(), b = a.beats;
  let best = 0;
  for (let i = 1; i < b.length; i++) if (Math.abs(b[i] - p) < Math.abs(b[best] - p)) best = i;
  await saveEdits(id, { add_anchor: best });
  flash(`デッキ${id}: ${fmtTime(b[best])} の拍を 1 拍目にしました`);
}

function meterLabel(id) {
  const a = engine.decks[id].analysis, btn = ui.deckView[id].root.querySelector('[data-act$="grid.meter"]');
  btn.textContent = a ? `拍子: ${a.meter}${a.meter_source === 'user' ? '' : '(自動)'}` : '拍子: 自動';
}

/** 拍子・グリッド・BPM 表示のメニュー。 */
function openGridMenu(id) {
  const dk = engine.decks[id], a = dk.analysis;
  if (!a) { flash('解析が終わってから使えます'); return; }
  document.getElementById('gridmenu')?.remove();
  const btn = ui.deckView[id].root.querySelector('[data-act$="grid.meter"]');
  const el = document.createElement('div');
  el.id = 'gridmenu';
  el.className = 'popmenu';
  const e = a.edits ?? {};
  const meters = [['自動', null], ...[3, 4, 5, 6, 7].map(m => [String(m), m])];
  el.innerHTML = `
    <div class="pm-sec">拍子</div>
    <div class="pm-row">${meters.map(([l, m]) => `<div class="btn ${(m === null ? !e.meter : e.meter === m) ? 'on' : ''}" data-meter="${m ?? ''}">${l}</div>`).join('')}</div>
    <div class="pm-sec">グリッドを前後にずらす（いま ${(e.grid_offset_ms ?? 0).toFixed(0)} ms）</div>
    <div class="pm-row"><div class="btn" data-off="-10">◀ 10ms</div><div class="btn" data-off="0">0 に戻す</div><div class="btn" data-off="10">10ms ▶</div></div>
    <div class="pm-sec">BPM の表示（いま ${a.bpm_display?.toFixed(1)}）</div>
    <div class="pm-row">${[0.5, 1, 2].map(k => `<div class="btn ${(e.bpm_scale ?? 1) === k ? 'on' : ''}" data-scale="${k}">×${k === 0.5 ? '½' : k}</div>`).join('')}</div>
    <div class="pm-sec">1 拍目の指定（${(e.downbeat_anchors ?? []).length} か所）</div>
    <div class="pm-row"><div class="btn" data-clear>すべて解除</div><div class="btn" data-close>閉じる</div></div>`;
  document.body.append(el);
  // メニューにも画面の拡大率（zoom）がかかるので、位置は拡大率で割り戻して指定する
  const r = btn.getBoundingClientRect(), u = uiScale();
  el.style.left = `${Math.min(innerWidth - el.offsetWidth * u - 8, r.left) / u}px`;
  el.style.top = `${Math.max(8, r.top - el.offsetHeight * u - 6) / u}px`;
  el.addEventListener('click', async ev => {
    const t = ev.target.closest('.btn');
    if (!t) return;
    if ('meter' in t.dataset) await saveEdits(id, { meter: t.dataset.meter ? +t.dataset.meter : null });
    else if ('off' in t.dataset) await saveEdits(id, { grid_offset_ms: t.dataset.off === '0' ? 0 : (e.grid_offset_ms ?? 0) + +t.dataset.off });
    else if ('scale' in t.dataset) await saveEdits(id, { bpm_scale: +t.dataset.scale });
    else if ('clear' in t.dataset) await saveEdits(id, { clear_anchors: true });
    el.remove();
    if (!('close' in t.dataset)) openGridMenu(id);
  });
  setTimeout(() => addEventListener('pointerdown', function close(ev) {
    if (!el.contains(ev.target)) { el.remove(); removeEventListener('pointerdown', close, true); }
  }, true), 0);
}

const fmtTime = s => `${Math.floor(s / 60)}:${(s % 60).toFixed(1).padStart(4, '0')}`;

/** ミキサーのチャンネル（P1-5）。 */
function setupChannel(id) {
  const ch = engine.mixer.ch[id];
  const A = n => `mixer.${id}.${n}`;
  const w = n => ui.mixer.knobs[A(n)] ?? ui.mixer.faders[A(n)];
  bindAbsolute(A('gain'), w('gain'), v => ch.set.gain(v));
  for (const band of ['high', 'mid', 'low']) bindAbsolute(A(`eq.${band}`), w(`eq.${band}`), v => ch.setEq(band, v));
  bindAbsolute(A('filter'), w('filter'), v => ch.setFilter(v));
  bindAbsolute(A('fader'), w('fader'), v => ch.setFader(v));
}

/** 絶対値のアクションを画面の操作子（ノブ・フェーダー）と結ぶ。キーボード・MIDI から動かしても画面の操作子が一緒に動く。 */
function bindAbsolute(name, widget, apply) {
  actions.register(name, { type: 'absolute', get: () => widget.get(), set: v => { widget.set(v, false); apply(v); } });
}

function nudge(id, dir) {
  const dk = engine.decks[id];
  if (sync.st[id].on) { sync.st[id].nudge = dir; return; }
  dk.applyRate(dk.baseRate() * (1 + dir * 0.04));
}

function jog(id, d) {
  const dk = engine.decks[id];
  if (!dk.track) return;
  if (!dk.playing) { dk.seek(dk.position() + d * 0.03); return; }
  if (sync.st[id].on) { sync.st[id].phi += d * 0.005; sync.st[id].phiTarget += d * 0.005; return; }
  dk.applyRate(dk.baseRate() * (1 + Math.max(-0.08, Math.min(0.08, d * 0.01))));
  clearTimeout(dk.jogTimer);
  dk.jogTimer = setTimeout(() => dk.applyRate(dk.baseRate()), 120);
}

/** 操作子の右クリックメニュー: MIDI 割り当て・割り当ての解除・（ホットキューなら）削除。 */
function openContextMenu(name, el, ev) {
  document.getElementById('ctxmenu')?.remove();
  const menu = document.createElement('div');
  menu.id = 'ctxmenu';
  menu.className = 'popmenu';
  const maps = midi.mapped(name);
  const items = [];
  if (/\.hotcue\.\d$/.test(name)) items.push(['hotcue-del', 'ホットキューを削除']);
  items.push(['learn', 'MIDI 割り当て（コントローラーを動かす）']);
  if (maps.length) items.push(['unmap', '割り当てを外す']);
  const kindName = m => (m.type === 'note' ? `ノート ${m.num}` : m.type === 'pb' ? 'ピッチベンド' : `CC ${m.num}`);
  const cur = maps.map(m => `${m.port} ch${m.ch + 1} ${kindName(m)}（${modeLabel(m.mode)}）`).join('<br>');
  menu.innerHTML = `<div class="pm-sec">${name}</div>${cur ? `<div class="pm-sec" style="color:var(--text)">${cur}</div>` : ''}` +
    items.map(([k, l]) => `<div class="pm-item" data-k="${k}">${l}</div>`).join('');
  document.body.append(menu);
  const u = uiScale();
  menu.style.left = `${Math.min(innerWidth - menu.offsetWidth * u - 8, ev.clientX) / u}px`;
  menu.style.top = `${Math.min(innerHeight - menu.offsetHeight * u - 8, ev.clientY) / u}px`;
  menu.addEventListener('click', e => {
    const k = e.target.closest('[data-k]')?.dataset.k;
    if (!k) return;
    menu.remove();
    if (k === 'hotcue-del') actions.trigger(name, true, { shift: true });
    if (k === 'unmap') midi.unmap(name);
    if (k === 'learn') {
      el.classList.add('learning');
      midi.learn(name, () => el.classList.remove('learning'));
    }
  });
  setTimeout(() => addEventListener('pointerdown', function close(e2) {
    if (!menu.contains(e2.target)) { menu.remove(); removeEventListener('pointerdown', close, true); }
  }, true), 0);
}

/** ±1 拍移動（解析前は ±0.5 秒）。 */
function beatJump(dk, dir) {
  if (!dk.track) return;
  const p = dk.position(), b = dk.analysis?.beats;
  if (!b?.length) { dk.seek(p + dir * 0.5); return; }
  const lb = localBpm(dk, p) || dk.analysis.bpm;
  dk.seek(p + dir * 60 / lb);
}

/** デッキ画面の状態表示（テンポ揺れ・拍なし・ラウドネス）。 */
function deckStatus(id) {
  const dk = engine.decks[id], el = ui.deckView[id].root.querySelector('[data-role=status]');
  const a = dk.analysis;
  if (!dk.track) { el.innerHTML = '<span class="empty">曲が読み込まれていません</span>'; return; }
  if (!a) { el.innerHTML = '<span class="empty">解析中…</span>'; return; }
  const conf = a.bars.length ? a.bars.reduce((s, b) => s + b.conf, 0) / a.bars.length : 0;
  const db = engine.mixer.ch[id].setLoudness(a.lufs, engine.mixer.targetLufs);
  const cl = conf > 0.6 ? '高' : conf > 0.3 ? '中' : '低';
  el.innerHTML = `小節頭の信頼度 <em>${cl}</em> ／ ${a.meter}拍子 ／ 拍: ${a.beat_class}<br>` +
    `テンポ揺れ ${Math.round(a.bpm_p5)}–${Math.round(a.bpm_p95)} BPM・拍なし ${Math.round(a.no_beat_total)}秒・${a.lufs.toFixed(1)} LUFS → ${db >= 0 ? '+' : ''}${db.toFixed(1)}dB`;
}

async function attachAnalysis(dk, track = dk.track) {
  if (!track) return;
  const [a, w] = await Promise.all([api.analysis(track.id), api.wave(track.id)]);
  if (dk.track !== track) return;          // 取得中に別の曲が読み込まれた
  const n = a.wave.length;
  dk.setAnalysis(a, { low: w.subarray(0, n), mid: w.subarray(n, 2 * n), high: w.subarray(2 * n, 3 * n) });
}

/** 曲が最後まで終わったデッキ: 再生済みにし、自動読み込みが ON ならキューの次の曲を読み込む。 */
function onTrackEnded(id) {
  const dk = engine.decks[id];
  if (!dk.track) return;
  queue.playState.played.add(dk.track.id);
  queue.refresh();
  if (settings.queue_autoload === false || runner?.busy) return;     // 繋ぎの実行中は runner がデッキを管理する
  const i = queue.nextIndex();
  if (i >= 0) loadDeck(id, queue.getItems()[i].track);
}

/** デッキへの読み込み。deck=null なら空いているデッキ（両方埋まっていれば止まっている側）。再生中のデッキには読み込まない。 */
async function loadDeck(deck, track) {
  const D = engine.decks;
  if (!deck) {
    deck = !D.A.track ? 'A' : !D.B.track ? 'B' : !D.A.playing ? 'A' : !D.B.playing ? 'B' : null;
    if (!deck) { flash('両方のデッキが再生中です。止めてから読み込んでください'); return; }
  }
  const dk = D[deck];
  if (dk.playing) { flash(`デッキ${deck}は再生中です。止めてから読み込んでください`); return; }
  if (dk.track && dk.hasPlayed) queue.playState.played.add(dk.track.id);
  dk.hasPlayed = false;
  queue.playState[deck] = track.id;
  queue.refresh();
  const loading = dk.load(track).catch(e => flash(String(e.message ?? e)));
  const st = await analysis.request(track.id, analysis.PRIO.DECK);
  if (st === 'done') attachAnalysis(dk, track);
  await loading;
}

const meterBuf = new Float32Array(1024);
function frame() {
  for (const id of ['A', 'B']) {
    updateDeckNumbers(id, engine.decks[id]);   // 曲の終わりの検出は sync.js の刻みで行う
  }
  drawZoom(engine.decks, COLORS, phaseInfo());
  drawOverviews(engine.decks, COLORS);
  for (const id of ['A', 'B']) {
    const pk = peakOf(engine.mixer.ch[id].meter, meterBuf);
    const db = 20 * Math.log10(Math.max(1e-6, pk));
    ui.mixer.setMeter(id, Math.max(0, Math.min(1, (db + 48) / 48)));
  }
  const red = engine.limiter.reduction;
  document.getElementById('limiter').textContent = red < -0.1 ? `${red.toFixed(1)} dB` : '0.0 dB';
  requestAnimationFrame(frame);
}

/** 画面下部に短いメッセージを出す。 */
export function flash(msg, ms = 2500) {
  let el = document.getElementById('flash');
  if (!el) {
    el = document.createElement('div');
    el.id = 'flash';
    document.body.append(el);
  }
  el.textContent = msg;
  el.classList.add('on');
  clearTimeout(flash.t);
  flash.t = setTimeout(() => el.classList.remove('on'), ms);
}

init();
