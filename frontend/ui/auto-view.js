// おまかせタブ（案X。design_phase3.md §5）。モックアップ mockup/phase3/ui_p3_X.html を本番用にしたもの。
// 条件（出発点・起点の曲・ジャンルのまとまり・長さ・流れ）→ 3 案 → 比べてキューに入れる。
// 曲の差し替えには次の曲の候補リスト（案Y。ui/candidates.js）を使う。
import { putSettings } from './settings-view.js';
import { fitCanvas } from './waveform.js';
import { openModal, closeModal } from './modal.js';
import { openCandidates } from './candidates.js';
import { starsHTML } from './stars.js';
import { trackData, drawWave } from './joins-view.js';

const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const fmt = s => { s = Math.max(0, s); return `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, '0')}`; };
const TPL = { long: 'ロングブレンド', short: 'ショート', echo: 'エコーアウト', bass: '低音入れ替え', filter: 'フィルター', cut: 'カットイン' };
// つなぎ方（design_cutin.md）
const JOIN_MODES = [['auto', 'おまかせ（重ねられない所はカットイン）'], ['mix', 'すべてミックス（重ねて繋ぐ）'], ['cut', 'すべてカットイン']];
const FLOWS = {
  up: { name: '上げていく', f: (x, e0) => e0 + (Math.min(1, e0 + 0.45) - e0) * x },
  keep: { name: 'キープ', f: (x, e0) => e0 },
  peak: { name: '山を作る', f: (x, e0) => e0 + 0.4 * Math.sin(Math.PI * x) },
  down: { name: '落ち着かせる', f: (x, e0) => e0 - (e0 - Math.max(0, e0 - 0.45)) * x },
  custom: { name: '自分で描く…', f: null },         // 案Z（P3-9）。線は cond.curve（0..1 を時間に等間隔）
};
const CURVE_N = 41;
const interp = (c, x) => { const f = Math.max(0, Math.min(1, x)) * (c.length - 1), i = Math.min(c.length - 2, Math.floor(f)); return c[i] + (c[i + 1] - c[i]) * (f - i); };
const DEFAULT_CURVE = Array.from({ length: CURVE_N }, (_, k) => 0.4 + 0.35 * Math.sin(Math.PI * k / (CURVE_N - 1)));
const MIN_MINUTES = 5, MAX_MINUTES = 180;                  // 全体の長さは 5 分単位（I-7）
const TRACK_LENS = [[0, 'おまかせ'], [30, '約30秒'], [45, '約45秒'], [60, '約1分'], [90, '約1分半'], [120, '約2分'], [180, '約3分'], [240, '約4分'], [300, '約5分']];
const SHORT_SEC = 90;     // これ以下は「短くたくさん繋ぐ」（segments.py の SHORT_SEC と同じ）
const POSITIONS = [['auto', 'おまかせ'], ['intro', '冒頭（0:00）から'], ['head', '前奏を飛ばして頭から'], ['peak', '盛り上がり中心'], ['tail', '終わりまで'], ['full', '曲全体']];
const STORE = 'djmix.auto.conditions';
const post = (url, body) => fetch(url, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) })
  .then(async r => { const j = await r.json(); if (!r.ok) throw new Error(j.detail ?? `${r.status}`); return j; });

export function initAutoView({ root, colors, queue, flash, previewTrack, previewJoin, openJoins, onToQueue }) {
  let cond = { mode: 'lib', seed: null, groups: ['ジャズ・フュージョン'], minutes: 40, flow: 'up', curve: DEFAULT_CURVE, range: 'groups', playlists: [],
               trackLen: 0, trackPos: 'auto', joinMode: 'auto',
               albums: [], albumSeed: null, useAll: false };      // アルバムで選ぶ（2026-10-25）: アルバム名・起点の曲（null = おまかせ）・全曲を使う
  // 条件はアプリの設定 auto_cond に残す（専用ウィンドウは localStorage が再起動で消えるため。初めて開いたときに読む）
  let condLoaded = false;
  async function loadCond() {
    try {
      const s = await fetch('/api/settings').then(r => r.json());
      let saved = s.auto_cond;
      if (!saved || !Object.keys(saved).length) {           // 前の版で localStorage に残した条件を引き継ぐ
        try { saved = JSON.parse(localStorage.getItem(STORE) || '{}'); } catch { saved = {}; }
        if (Object.keys(saved).length) putSettings({ auto_cond: saved }).catch(() => {});
      }
      Object.assign(cond, saved ?? {});
    } catch { /* 既定のまま */ }
    condLoaded = true;
  }
  let playlists = [];                    // iTunes のプレイリスト（Phase 5 E）
  let seedTrack = null, groups = [], result = null, plan = 0, sel = -1, hover = -1, boxes = [], busy = false;
  let saveTimer;
  const save = () => { clearTimeout(saveTimer); saveTimer = setTimeout(() => putSettings({ auto_cond: cond }).catch(() => {}), 400); };
  const flowFn = k => (k === 'custom' ? (x => interp(cond.curve ?? DEFAULT_CURVE, x)) : FLOWS[k]?.f ?? FLOWS.up.f);
  const scoreColor = s => s > 0.8 ? colors['--ok'] : s > 0.65 ? colors['--warn'] : colors['--active'];

  root.innerHTML = `<div class="xgrid">
    <div class="p2card cond">
      <div><h4>出発点</h4><div class="seg2 big" data-r="mode"><div data-m="lib">ライブラリから選んで作る</div><div data-m="queue">キューの曲を並べ替える</div></div></div>
      <div data-r="libconds">
        <div style="margin-bottom:14px"><h4>起点の曲（最初にかける曲）</h4><div class="seedbox" data-r="seed"></div></div>
        <div style="margin-bottom:14px"><h4>選ぶ範囲（数字は 解析済み / 全体）</h4>
          <div class="seg2" data-r="range" style="margin-bottom:8px"><div data-rg="groups">ジャンルのまとまり</div><div data-rg="playlists">iTunes のプレイリスト</div><div data-rg="albums">アルバム</div></div>
          <div class="gchips" data-r="groups"></div><div class="plpick" data-r="pls"></div><div class="albsel" data-r="albs"></div></div>
        <label class="opt" data-r="useallbox" style="margin:-6px 0 12px"><input type="checkbox" data-r="useall"> 選んだアルバムの曲を全部使う（全体の長さは使いません）</label>
        <div style="margin-bottom:14px" data-r="lenbox"><h4>全体の長さ（5 分単位）</h4><div class="lenpick" data-r="len">
          <div class="btn" data-mstep="-5">−5分</div><select data-r="minsel">${Array.from({ length: (MAX_MINUTES - MIN_MINUTES) / 5 + 1 }, (_, k) => MIN_MINUTES + 5 * k)
            .map(m => `<option value="${m}">${m >= 60 ? `${Math.floor(m / 60)}時間${m % 60 ? `${m % 60}分` : ''}` : `${m}分`}</option>`).join('')}</select><div class="btn" data-mstep="5">＋5分</div></div></div>
        <div style="margin-bottom:14px"><h4>1 曲あたりの長さ</h4><div class="seg2" data-r="tlen">${TRACK_LENS.map(([v, l]) => `<div data-tl="${v}">${l}</div>`).join('')}</div></div>
        <div><h4>使う区間（結果の曲ごとにも変えられます）</h4><div class="seg2" data-r="tpos">${POSITIONS.map(([v, l]) => `<div data-tp="${v}">${l}</div>`).join('')}</div>
          <div class="lbl2" data-r="poshint" style="margin-top:4px;line-height:1.6"></div></div>
      </div>
      <div data-r="qconds" class="lbl2" style="line-height:1.8"></div>
      <div><h4>つなぎ方（結果のつなぎ目ごとにも変えられます）</h4><div class="seg2" data-r="jmode">${JOIN_MODES.map(([v, l]) => `<div data-jmv="${v}">${l}</div>`).join('')}</div>
        <div class="lbl2" style="margin-top:4px;line-height:1.6">ミックス = 拍を合わせて重ねる（合わせられない所はエコーアウト）。カットイン = 重ねずに、前の曲の小節の頭でスパッと次の曲の 1 拍目へ切り替える（テンポの違う曲どうしも繋げる。切れ目の良い小節を探し、音量も自動で合わせる）。「すべてカットイン」はテンポの離れた曲（±50%）も候補にします。</div></div>
      <div data-r="flowbox"><h4>流れ</h4><div class="flows" data-r="flows">${Object.entries(FLOWS).map(([k, f]) => `<div class="flow" data-f="${k}"><canvas></canvas>${f.name}</div>`).join('')}</div></div>
      <div class="btn primary" data-r="go" style="height:44px">おまかせで作る</div>
      <div class="lbl2" style="line-height:1.7">テンポ（倍/半も考慮）・キー・繋ぐ部分の響き・音の雰囲気の近さで並びを探し、曲ごとの使う範囲と繋ぎ方も決めます。候補は解析済みの曲だけです。結果はキューに入れてから、つなぎ目タブで直せます。</div>
    </div>
    <div class="xres" data-r="res"><div class="p2card jempty">条件を選んで「おまかせで作る」を押してください。</div></div>
  </div>`;
  const $ = k => root.querySelector(`[data-r=${k}]`);
  root.querySelectorAll('[data-rg]').forEach(x => x.addEventListener('click', () => { cond.range = x.dataset.rg; save(); renderConds(); }));

  // ---- 条件 ----
  function renderConds() {
    root.querySelectorAll('[data-m]').forEach(x => x.classList.toggle('on', x.dataset.m === cond.mode));
    $('libconds').style.display = cond.mode === 'lib' ? '' : 'none';
    $('flowbox').style.display = cond.mode === 'lib' ? '' : 'none';
    $('qconds').style.display = cond.mode === 'queue' ? '' : 'none';
    const n = queue.getItems().length;
    $('qconds').innerHTML = `キューにある ${n} 曲の並び・使う範囲・繋ぎ方を決め直します（曲は入れ替えません）。最初の曲も含めて探します。<br>解析されていない曲は並べ替えに使えません。`;
    cond.minutes = Math.max(MIN_MINUTES, Math.min(MAX_MINUTES, Math.round((cond.minutes || 40) / 5) * 5));
    $('minsel').value = String(cond.minutes);
    root.querySelectorAll('[data-tl]').forEach(x => x.classList.toggle('on', +x.dataset.tl === (cond.trackLen || 0)));
    root.querySelectorAll('[data-tp]').forEach(x => x.classList.toggle('on', x.dataset.tp === (cond.trackPos || 'auto')));
    $('tlen').classList.toggle('off', cond.trackPos === 'full');
    $('poshint').textContent = {
      auto: '区間の区切りで決めます（長い曲は途中の区切りで終える）',
      intro: '曲の冒頭（0:00）から', head: '拍の取れない前奏を飛ばして、曲の頭から', peak: '一番音の大きい・密度の高い所を中心に',
      tail: '曲の終わり（拍の取れない終わりは除く）まで', full: '曲全体（0:00〜最後）。1 曲あたりの長さは使いません',
    }[cond.trackPos || 'auto'] + '。範囲の端は区間の区切りにそろえ、繋ぐ所は拍の取れる所にします。ミックスの最初の曲は、短い前奏も飛ばさず冒頭から鳴らします' +
      // 短くたくさん繋ぐ（2026-10-25）: おおよその曲数と、短いときの決め方
      (cond.trackLen && cond.trackLen <= SHORT_SEC && cond.trackPos !== 'full'
        ? `。1 曲 ${cond.trackLen} 秒ほどで、${cond.minutes} 分なら約 ${Math.round(cond.minutes * 60 / (cond.trackLen * 0.85))} 曲を繋ぎます（重なりは曲の長さに合わせて 4〜8 小節。${(cond.trackPos || 'auto') === 'auto' ? '「おまかせ」は盛り上がりの所を使います' : ''}）`
        : '');
    root.querySelectorAll('.flow').forEach(x => x.classList.toggle('on', x.dataset.f === cond.flow));
    root.querySelectorAll('[data-jmv]').forEach(x => x.classList.toggle('on', x.dataset.jmv === (cond.joinMode || 'auto')));
    const useAlb = cond.range === 'albums';
    if (useAlb) {
      // アルバムで選ぶ: 起点の曲は おまかせ（流れの始まりに合う曲を自動で）か、選んだアルバムの曲から指定
      const st = cond.albumSeed;
      $('seed').innerHTML = st
        ? `<div><b>${esc(st.title)}</b> <span>${esc(st.artist ?? '')} ・ 『${esc(st.album ?? '')}』</span></div><div class="sp" style="flex:1"></div>
           <div class="btn" data-aseed style="height:30px;min-width:0;font-size:12px">変更…</div><div class="btn" data-aseedclr style="height:30px;min-width:0;font-size:12px">おまかせに戻す</div>`
        : `<span class="lbl2">おまかせ（選んだアルバムから、流れの始まりに合う曲を自動で。案ごとに違う曲）</span><div class="sp" style="flex:1"></div><div class="btn" data-aseed style="height:30px;min-width:0;font-size:12px">指定する…</div>`;
      $('seed').querySelector('[data-aseed]').addEventListener('click', pickAlbumSeed);
      $('seed').querySelector('[data-aseedclr]')?.addEventListener('click', () => { cond.albumSeed = null; save(); renderConds(); });
    } else $('seed').innerHTML = seedTrack
      ? `<div><b>${esc(seedTrack.title)}</b> <span>${esc(seedTrack.artist ?? '')}${seedTrack.bpm ? ` ・ ${seedTrack.bpm.toFixed(1)} BPM` : ''}${seedTrack.camelot ? ` ・ ${esc(seedTrack.camelot)}` : ''}</span></div><div class="sp" style="flex:1"></div><div class="btn" data-seed style="height:30px;min-width:0;font-size:12px">変更…</div>`
      : `<span class="lbl2">起点の曲を選んでください</span><div class="sp" style="flex:1"></div><div class="btn" data-seed style="height:30px;min-width:0;font-size:12px">選ぶ…</div>`;
    $('seed').querySelector('[data-seed]')?.addEventListener('click', pickSeed);
    $('groups').innerHTML = groups.map(g => `<label class="gchip ${cond.groups.includes(g.name) ? 'on' : ''}"><input type="checkbox" value="${esc(g.name)}" ${cond.groups.includes(g.name) ? 'checked' : ''}>${esc(g.name)}<small>${g.analyzed.toLocaleString()} / ${g.total.toLocaleString()}</small></label>`).join('');
    $('groups').querySelectorAll('input').forEach(x => x.addEventListener('change', () => {
      cond.groups = [...$('groups').querySelectorAll('input')].filter(i => i.checked).map(i => i.value);
      x.parentElement.classList.toggle('on', x.checked); save();
    }));
    // 選ぶ範囲: ジャンルのまとまり／iTunes のプレイリスト（複数を合わせて使える）
    const usePl = cond.range === 'playlists';
    root.querySelectorAll('[data-rg]').forEach(x => x.classList.toggle('on', x.dataset.rg === cond.range));
    $('groups').style.display = usePl || useAlb ? 'none' : '';
    $('pls').style.display = usePl ? '' : 'none';
    $('albs').style.display = useAlb ? '' : 'none';
    $('useallbox').style.display = useAlb ? '' : 'none';
    $('useall').checked = !!cond.useAll;
    $('lenbox').classList.toggle('off', useAlb && !!cond.useAll);
    renderAlbums();
    const pls = playlists.filter(p => !p.folder);
    cond.playlists = (cond.playlists ?? []).filter(id => pls.some(p => p.id === id));
    $('pls').innerHTML = pls.length ? pls.map(p => `<label class="${cond.playlists.includes(p.id) ? 'on' : ''}"><input type="checkbox" value="${p.id}" ${cond.playlists.includes(p.id) ? 'checked' : ''}>` +
        `${p.smart ? '⚙ ' : ''}${esc(p.name)}<small>${p.analyzed.toLocaleString()} / ${p.count.toLocaleString()}</small></label>`).join('')
      : '<div class="lbl2" style="line-height:1.8">iTunes のプレイリストがありません（設定タブの「iTunes との連携」）。</div>';
    $('pls').querySelectorAll('input').forEach(x => x.addEventListener('change', () => {
      cond.playlists = [...$('pls').querySelectorAll('input')].filter(i => i.checked).map(i => +i.value);
      x.parentElement.classList.toggle('on', x.checked); save();
    }));
    drawFlows();
  }
  // ---- アルバムで選ぶ（2026-10-25）: 3 枚以上のアルバムの曲だけで作る ----
  function renderAlbums() {
    const n = (cond.albums ?? []).length;
    $('albs').innerHTML = `<div class="albchips">${(cond.albums ?? []).map((a, i) => `<span class="albchip" title="${esc(a)}">${esc(a)}<b data-arm="${i}" title="外す">×</b></span>`).join('')
      || '<span class="lbl2">アルバムを 3 枚以上選んでください</span>'}</div>
      <div style="display:flex;gap:8px;align-items:center;margin-top:6px"><div class="btn" data-albpick style="height:30px;min-width:0;font-size:12px">アルバムを選ぶ…</div>
      <span class="lbl2">${n} 枚選択${n && n < 3 ? '（あと ' + (3 - n) + ' 枚）' : ''}</span></div>`;
    $('albs').querySelector('[data-albpick]').addEventListener('click', pickAlbums);
    $('albs').querySelectorAll('[data-arm]').forEach(b => b.addEventListener('click', () => {
      cond.albums.splice(+b.dataset.arm, 1);
      if (cond.albumSeed && !cond.albums.includes(cond.albumSeed.album)) cond.albumSeed = null;
      save(); renderConds();
    }));
  }

  /** アルバムを選ぶ小窓: 検索（アルバム名・アーティスト）、解析済みの曲数つき。 */
  async function pickAlbums() {
    let all = [], sel = new Set(cond.albums ?? []), q = '', only = false;
    openModal(`<h3>アルバムを選ぶ <span class="lbl2">（3 枚以上。選んだアルバムの収録曲だけでミックスを作ります）</span></h3>
      <div class="exrow"><input data-aq placeholder="アルバム名・アーティストで検索" style="flex:1">
        <label class="lbl2"><input type="checkbox" data-aonly> 選んだものだけ表示</label></div>
      <div class="lbl2" data-acount></div>
      <div class="alblist" data-alist><div class="lbl2" style="padding:12px">読み込んでいます…</div></div>
      <div class="mfoot"><span class="lbl2" data-asel></span><div class="sp"></div><div class="btn" data-aclr>選択をすべて外す</div><div class="btn" data-close>やめる</div><div class="btn primary" data-aok>この ${sel.size} 枚にする</div></div>`, m => {
      const list = m.querySelector('[data-alist]');
      const paint = () => {
        const ql = q.toLowerCase();
        const rows = all.filter(a => (!only || sel.has(a.name)) && (!ql || a.name.toLowerCase().includes(ql) || (a.artist ?? '').toLowerCase().includes(ql)));
        m.querySelector('[data-acount]').textContent = `${rows.length.toLocaleString()} 枚（全 ${all.length.toLocaleString()} 枚）。数字は 解析済み / 収録曲`;
        list.innerHTML = rows.slice(0, 400).map(a => `<label class="${sel.has(a.name) ? 'on' : ''}"><input type="checkbox" value="${esc(a.name)}" ${sel.has(a.name) ? 'checked' : ''}>
          <span class="an">${esc(a.name)}</span><span class="aa">${esc(a.artists > 1 ? `${a.artist} ほか ${a.artists - 1} 人` : a.artist)}${a.year ? ` ・ ${esc(a.year)}` : ''}</span>
          <small class="${a.analyzed ? '' : 'ng'}">${a.analyzed} / ${a.count}</small></label>`).join('')
          + (rows.length > 400 ? `<div class="lbl2" style="padding:8px">ほか ${rows.length - 400} 枚（検索で絞り込んでください）</div>` : '')
          || '<div class="lbl2" style="padding:12px">見つかりません</div>';
        list.querySelectorAll('input').forEach(x => x.addEventListener('change', () => {
          x.checked ? sel.add(x.value) : sel.delete(x.value);
          x.parentElement.classList.toggle('on', x.checked);
          foot();
        }));
        foot();
      };
      const foot = () => {
        const n = sel.size, an = all.filter(a => sel.has(a.name)).reduce((s, a) => s + a.analyzed, 0);
        m.querySelector('[data-asel]').textContent = n ? `${n} 枚 ・ 解析済みの曲 ${an} 曲${n < 3 ? `（あと ${3 - n} 枚）` : ''}` : '';
        const ok = m.querySelector('[data-aok]');
        ok.textContent = `この ${n} 枚にする`;
        ok.classList.toggle('off', n < 3);
      };
      let t = 0;
      m.querySelector('[data-aq]').addEventListener('input', e => { clearTimeout(t); t = setTimeout(() => { q = e.target.value.trim(); paint(); }, 150); });
      m.querySelector('[data-aonly]').addEventListener('change', e => { only = e.target.checked; paint(); });
      m.querySelector('[data-aclr]').addEventListener('click', () => { sel.clear(); paint(); });
      m.querySelector('[data-aok]').addEventListener('click', () => {
        if (sel.size < 3) { flash('アルバムを 3 枚以上選んでください'); return; }
        // 選んだ順を保ちつつ、新しく足したものは後ろに
        cond.albums = [...(cond.albums ?? []).filter(a => sel.has(a)), ...[...sel].filter(a => !(cond.albums ?? []).includes(a))];
        if (cond.albumSeed && !sel.has(cond.albumSeed.album)) cond.albumSeed = null;
        save(); closeModal(); renderConds();
      });
      fetch('/api/auto/albums').then(r => r.json()).then(r => { all = r.items; paint(); m.querySelector('[data-aq]').focus(); })
        .catch(() => { list.innerHTML = '<div class="lbl2" style="padding:12px">読み込めませんでした</div>'; });
    });
    document.querySelector('.mbox')?.classList.add('wide');
  }

  /** 起点の曲を、選んだアルバムの曲から指定する。 */
  async function pickAlbumSeed() {
    if ((cond.albums ?? []).length < 1) { flash('先にアルバムを選んでください'); return; }
    const qs = cond.albums.map(a => `album=${encodeURIComponent(a)}`).join('&');
    const { items } = await fetch(`/api/auto/album-tracks?${qs}`).then(r => r.json()).catch(() => ({ items: [] }));
    openModal(`<h3>起点の曲を選ぶ <span class="lbl2">（選んだアルバムの解析済みの曲）</span></h3>
      <div class="alblist seedl">${items.map((t, i) => `<div class="pm-item" data-k="${i}"><b>${esc(t.title)}</b> <span class="dim">${esc(t.artist ?? '')} ・ 『${esc(t.album)}』${t.bpm ? ` ・ ${t.bpm.toFixed(1)} BPM` : ''}${t.camelot ? ` ・ ${esc(t.camelot)}` : ''}</span>
        <span class="btn" data-pv="${i}" style="height:24px;min-width:0;font-size:11px;padding:0 8px;margin-left:auto">▶ 試聴</span></div>`).join('')
        || '<div class="lbl2" style="padding:12px">解析済みの曲がありません</div>'}</div>
      <div class="mfoot"><div class="btn" data-close>やめる</div></div>`, m => {
      m.querySelectorAll('[data-pv]').forEach(b => b.addEventListener('click', async ev => {
        ev.stopPropagation();
        const t = items[+b.dataset.pv];
        previewTrack(await fullTrack(t.id), { t0: 0, t1: t.duration }, 'head');
      }));
      m.querySelectorAll('[data-k]').forEach(el => el.addEventListener('click', () => {
        const t = items[+el.dataset.k];
        cond.albumSeed = { id: t.id, title: t.title, artist: t.artist, album: t.album };
        save(); closeModal(); renderConds();
      }));
    });
    document.querySelector('.mbox')?.classList.add('wide');
  }

  function drawFlows() {
    root.querySelectorAll('.flow').forEach(el => {
      const [g, W, H, d] = fitCanvas(el.querySelector('canvas'));
      g.clearRect(0, 0, W, H);
      g.strokeStyle = el.dataset.f === cond.flow ? '#fb923c' : '#8a9099'; g.lineWidth = 2 * d; g.beginPath();
      const fn = flowFn(el.dataset.f);
      for (let k = 0; k <= 30; k++) { const x = 4 * d + k / 30 * (W - 8 * d), y = H - 4 * d - fn(k / 30, 0.45) * (H - 8 * d); k ? g.lineTo(x, y) : g.moveTo(x, y); }
      g.stroke();
    });
  }
  $('mode').addEventListener('click', e => { const m = e.target.closest('[data-m]'); if (m) { cond.mode = m.dataset.m; save(); result = null; renderConds(); renderResult(); } });
  $('len').addEventListener('click', e => {
    const b = e.target.closest('[data-mstep]');
    if (b) { cond.minutes = Math.max(MIN_MINUTES, Math.min(MAX_MINUTES, cond.minutes + +b.dataset.mstep)); save(); renderConds(); }
  });
  $('minsel').addEventListener('change', e => { cond.minutes = +e.target.value; save(); renderConds(); });
  $('tlen').addEventListener('click', e => { const x = e.target.closest('[data-tl]'); if (x) { cond.trackLen = +x.dataset.tl; save(); renderConds(); } });
  $('jmode').addEventListener('click', e => { const x = e.target.closest('[data-jmv]'); if (x) { cond.joinMode = x.dataset.jmv; save(); renderConds(); } });
  $('useall').addEventListener('change', e => { cond.useAll = e.target.checked; save(); renderConds(); });
  $('tpos').addEventListener('click', e => { const x = e.target.closest('[data-tp]'); if (x) { cond.trackPos = x.dataset.tp; save(); renderConds(); } });
  $('flows').addEventListener('click', e => {
    const f = e.target.closest('[data-f]'); if (!f) return;
    if (f.dataset.f === 'custom') { drawCurve(); return; }
    cond.flow = f.dataset.f; save(); renderConds();
  });

  // ---- 流れを描く（案Z。design_phase3.md §7） ----
  function drawCurve() {
    let curve = [...(cond.curve ?? DEFAULT_CURVE)];
    const PRE = { up: k => 0.3 + 0.5 * k, peak: k => 0.35 + 0.45 * Math.sin(Math.PI * k), wave: k => 0.5 + 0.25 * Math.sin(4 * Math.PI * k - Math.PI / 2) + 0.1 * k, down: k => 0.8 - 0.5 * k };
    openModal(`<h3>流れを描く</h3>
      <p class="lbl2" style="line-height:1.7">ミックスの盛り上がりの流れを、枠の中をドラッグして描きます（左が始まり、上ほど盛り上がる）。描いた線に沿うように曲を並べます。</p>
      <div class="acts"><span class="lbl2">ひな形</span><span class="btn" data-pre="up">上げていく</span><span class="btn" data-pre="peak">山を作る</span><span class="btn" data-pre="wave">波を2回</span><span class="btn" data-pre="down">落ち着かせる</span></div>
      <canvas class="drawc" data-draw style="height:260px;cursor:crosshair"></canvas>
      <div class="mfoot"><div class="btn" data-close>キャンセル</div><div class="btn primary" data-ok>この流れにする</div></div>`, m => {
      const cv = m.querySelector('[data-draw]'), PAD = 26;
      const paint = () => {
        const [g, W, H, d] = fitCanvas(cv), pad = PAD * d;
        const X = k => pad + k / (CURVE_N - 1) * (W - 2 * pad), Y = e => H - pad - e * (H - 2 * pad);
        g.fillStyle = '#050607'; g.fillRect(0, 0, W, H);
        g.font = `${10 * d}px Consolas`;
        for (let q = 0; q <= 4; q++) { const x = Math.round(X(q * (CURVE_N - 1) / 4)); g.fillStyle = 'rgba(255,255,255,.08)'; g.fillRect(x, pad, d, H - 2 * pad); g.fillStyle = colors['--faint']; g.fillText(`${Math.round(cond.minutes * q / 4)}分`, x - 8 * d, H - 8 * d); }
        g.fillStyle = colors['--dim']; g.font = `${11 * d}px "Hiragino Sans", "Yu Gothic UI", sans-serif`; g.fillText('盛り上がる', 4 * d, pad - 8 * d); g.fillText('静か', 4 * d, H - pad + 14 * d);
        g.strokeStyle = '#fb923c'; g.lineWidth = 3 * d; g.lineJoin = 'round'; g.beginPath();
        curve.forEach((e, k) => (k ? g.lineTo(X(k), Y(e)) : g.moveTo(X(k), Y(e)))); g.stroke();
      };
      let last = null, drawing = false;
      const at = ev => { const r = cv.getBoundingClientRect(); return [Math.round(Math.max(0, Math.min(1, (ev.clientX - r.left - PAD) / (r.width - 2 * PAD))) * (CURVE_N - 1)), Math.max(0, Math.min(1, 1 - (ev.clientY - r.top - PAD) / (r.height - 2 * PAD)))]; };
      const put = ev => {
        const [k, e] = at(ev);
        if (last && last[0] !== k) { const [k0, e0] = last, [a, b] = k0 < k ? [k0, k] : [k, k0]; for (let j = a; j <= b; j++) curve[j] = e0 + (e - e0) * (j - k0) / (k - k0); }
        curve[k] = e; last = [k, e]; paint();
      };
      cv.addEventListener('pointerdown', ev => { drawing = true; last = null; cv.setPointerCapture?.(ev.pointerId); put(ev); });
      cv.addEventListener('pointermove', ev => drawing && put(ev));
      cv.addEventListener('pointerup', () => { drawing = false; last = null; });
      m.querySelectorAll('[data-pre]').forEach(b => b.addEventListener('click', () => { curve = curve.map((_, k) => PRE[b.dataset.pre](k / (CURVE_N - 1))); paint(); }));
      m.querySelector('[data-ok]').addEventListener('click', () => { cond.curve = curve.map(v => Math.round(v * 1000) / 1000); cond.flow = 'custom'; save(); closeModal(); renderConds(); });
      requestAnimationFrame(paint);
    });
  }
  $('go').addEventListener('click', () => run());

  async function loadSeed() {
    let id = cond.seed ?? queue.getItems()[0]?.track_id;
    if (!id) { seedTrack = null; return; }
    const { items } = await fetch(`/api/tracks?ids=${id}`).then(r => r.json());
    seedTrack = items[0] ?? null;
    if (seedTrack) cond.seed = seedTrack.id;
  }

  /** 起点の曲を選ぶ（使ってみての改善: 絞り込みを増やし、多く表示。200 曲ずつ、スクロールで続き）。 */
  function pickSeed() {
    const CAM = Array.from({ length: 12 }, (_, i) => [`${i + 1}A`, `${i + 1}B`]).flat();
    const SORTS = [['title', '曲名'], ['artist', 'アーティスト'], ['bpm', 'BPM'], ['camelot', 'キー'], ['rating', '★'], ['last_played', '最後にかけた日']];
    const f = { q: '', artist_q: '', album_q: '', bpm_min: '', bpm_max: '', key: '', rating_min: '', groups: [...(cond.groups ?? [])], sort: 'artist', desc: false };
    try { Object.assign(f, JSON.parse(sessionStorage.getItem('djmix.seedpick') || '{}')); } catch { /* 無くてよい */ }
    let rows = [], total = 0, loading = false, seq = 0;
    const box = openModal(`<h3>起点の曲を選ぶ</h3>
      <div class="seedf">
        <input data-f="q" placeholder="曲名・アーティスト・アルバムで検索" class="sf-q">
        <div class="sf-row"><span class="lbl2">ジャンル</span><div class="cfchips" data-g></div></div>
        <div class="sf-row">
          <label>アーティスト <input data-f="artist_q" list="seed-artists" placeholder="名前の一部"></label><datalist id="seed-artists"></datalist>
          <label>アルバム <input data-f="album_q" placeholder="名前の一部"></label>
          <label>BPM <input data-f="bpm_min" type="number" min="40" max="250" placeholder="から" class="sf-n"> – <input data-f="bpm_max" type="number" min="40" max="250" placeholder="まで" class="sf-n"></label>
          <label>キー <select data-f="key"><option value="">すべて</option>${CAM.map(k => `<option>${k}</option>`).join('')}</select></label>
          <label>★ <select data-f="rating_min"><option value="">すべて</option><option value="3">★3 以上</option><option value="4">★4 以上</option><option value="5">★5</option></select></label>
          <label>並び <select data-f="sort">${SORTS.map(([k, l]) => `<option value="${k}">${l}</option>`).join('')}</select></label>
          <label class="sf-c"><input type="checkbox" data-f="desc"> 逆順</label>
          <span class="sp" style="flex:1"></span><span class="btn sf-clear" data-clear>条件をすべて外す</span>
        </div>
      </div>
      <div class="lbl2" data-count>解析済みの曲だけが出ます</div>
      <div class="seedlist big" data-list></div>
      <div class="mfoot"><div class="btn" data-close>閉じる</div></div>`, m => {
      m.querySelector('.mbox')?.classList.add('wide');
      const list = m.querySelector('[data-list]');
      const params = () => ({ q: f.q, artist_q: f.artist_q, album_q: f.album_q, bpm_min: f.bpm_min || undefined, bpm_max: f.bpm_max || undefined,
        keys: f.key || undefined, rating_min: f.rating_min || undefined, groups: f.groups.join(',') || undefined, analyzed: true,
        sort: f.sort, desc: f.desc || undefined });
      const rowHTML = x => `<div class="seedrow" data-id="${x.id}"><b title="${esc(x.title)}">${esc(x.title)}</b><span title="${esc(x.artist ?? '')}">${esc(x.artist ?? '')}</span>` +
        `<span title="${esc(x.album ?? '')}">${esc(x.album ?? '')}</span><span class="mono">${x.bpm ? x.bpm.toFixed(1) : ''}</span><span class="mono">${esc(x.camelot ?? '')}</span>` +
        `<span class="st">${x.rating ? '★'.repeat(x.rating) : ''}</span><span class="mono">${x.duration ? fmt(x.duration) : ''}</span></div>`;
      const load = async reset => {
        if (loading && !reset) return;
        const my = ++seq; loading = true;
        try {
          if (reset) { rows = []; list.scrollTop = 0; }
          const r = await fetch(`/api/library?${new URLSearchParams(Object.entries({ ...params(), offset: rows.length, limit: 200 }).filter(([, v]) => v !== undefined && v !== ''))}`).then(x => x.json());
          if (my !== seq) return;
          total = r.total; rows = rows.concat(r.items);
          const head = '<div class="seedrow head"><b>曲名</b><span>アーティスト</span><span>アルバム</span><span>BPM</span><span>キー</span><span>★</span><span>長さ</span></div>';
          if (reset) list.innerHTML = head + (r.items.map(rowHTML).join('') || '<div class="lbl2" style="padding:12px">見つかりません（条件を外すか、解析済みの曲を増やしてください）</div>');
          else list.insertAdjacentHTML('beforeend', r.items.map(rowHTML).join(''));
          m.querySelector('[data-count]').textContent = `解析済みで条件に合う曲 ${total.toLocaleString()} 曲（${rows.length.toLocaleString()} 曲を表示。下へスクロールで続き）`;
          try { sessionStorage.setItem('djmix.seedpick', JSON.stringify(f)); } catch { /* 無くてよい */ }
        } finally { if (my === seq) loading = false; }
      };
      list.addEventListener('scroll', () => { if (rows.length < total && list.scrollTop + list.clientHeight > list.scrollHeight - 300) load(false); });
      // 入力欄と選択
      let t = null;
      const sync = () => m.querySelectorAll('[data-f]').forEach(el => { if (el.type === 'checkbox') el.checked = !!f[el.dataset.f]; else el.value = f[el.dataset.f] ?? ''; });
      sync();
      m.querySelectorAll('[data-f]').forEach(el => el.addEventListener(el.tagName === 'SELECT' || el.type === 'checkbox' ? 'change' : 'input', () => {
        f[el.dataset.f] = el.type === 'checkbox' ? el.checked : el.value.trim();
        clearTimeout(t); t = setTimeout(() => load(true), 200);
      }));
      // ジャンル（まとまり）
      const renderG = () => {
        m.querySelector('[data-g]').innerHTML = `<span class="cf ${f.groups.length ? '' : 'on'}" data-gall>すべて</span>` +
          groups.map(g => `<span class="cf ${f.groups.includes(g.name) ? 'on' : ''}" data-gv="${esc(g.name)}">${esc(g.name)}<small>${g.analyzed.toLocaleString()}</small></span>`).join('');
      };
      renderG();
      m.querySelector('[data-g]').addEventListener('click', e => {
        if (e.target.closest('[data-gall]')) f.groups = [];
        else { const v = e.target.closest('[data-gv]')?.dataset.gv; if (v == null) return; f.groups = f.groups.includes(v) ? f.groups.filter(x => x !== v) : [...f.groups, v]; }
        renderG(); load(true);
      });
      m.querySelector('[data-clear]').addEventListener('click', () => {
        Object.assign(f, { q: '', artist_q: '', album_q: '', bpm_min: '', bpm_max: '', key: '', rating_min: '', groups: [], desc: false });
        sync(); renderG(); load(true);
      });
      fetch('/api/auto/facets').then(r => r.json()).then(x => {
        m.querySelector('#seed-artists').innerHTML = x.artists.map(a => `<option value="${esc(a.name)}">${a.count} 曲</option>`).join('');
      }).catch(() => {});
      list.addEventListener('click', async e => {
        const row = e.target.closest('[data-id]'); if (!row) return;
        cond.seed = +row.dataset.id; save(); closeModal(); await loadSeed(); renderConds();
      });
      load(true); m.querySelector('[data-f=q]').focus();
    });
    return box;
  }

  // ---- 実行 ----
  async function run(avoid = []) {
    if (busy) return;
    const albMode = cond.mode === 'lib' && cond.range === 'albums';
    const body = cond.mode === 'queue'
      ? { mode: 'queue', ids: queue.getItems().map(it => it.track_id), join_mode: cond.joinMode || 'auto' }
      : albMode
      ? { mode: 'albums', albums: cond.albums, seed: cond.albumSeed?.id ?? null, use_all: !!cond.useAll, minutes: cond.minutes, flow: cond.flow, avoid,
          curve: cond.flow === 'custom' ? cond.curve : undefined,
          track_len: cond.trackPos === 'full' ? null : (cond.trackLen || null), track_pos: cond.trackPos || 'auto', join_mode: cond.joinMode || 'auto' }
      : { mode: 'library', seed: cond.seed, groups: cond.groups, minutes: cond.minutes, flow: cond.flow, avoid, curve: cond.flow === 'custom' ? cond.curve : undefined,
          playlists: cond.range === 'playlists' ? cond.playlists : undefined,
          track_len: cond.trackPos === 'full' ? null : (cond.trackLen || null), track_pos: cond.trackPos || 'auto', join_mode: cond.joinMode || 'auto' };
    if (albMode && (cond.albums ?? []).length < 3) { flash('アルバムを 3 枚以上選んでください'); return; }
    if (cond.mode === 'lib' && !albMode && !cond.seed) { flash('起点の曲を選んでください'); return; }
    if (cond.mode === 'lib' && cond.range === 'playlists' && !cond.playlists.length) { flash('プレイリストを選んでください'); return; }
    busy = true;
    $('go').textContent = '探しています…';
    $('res').innerHTML = '<div class="p2card jempty">並びを探しています…</div>';
    try {
      result = await post('/api/auto/plan', body);
      plan = 0; sel = -1;
      if (result.missing?.length) flash(`解析されていない ${result.missing.length} 曲は並べ替えに入れませんでした`, 4000);
      if (result.missing_albums?.length) flash(`解析済みの曲が無いアルバムは使えませんでした: ${result.missing_albums.join('、')}`, 5000);
      else if (albMode && !cond.useAll && result.plans[0] && result.plans[0].minutes < cond.minutes * 0.9)
        flash(`選んだアルバムの曲では約 ${Math.round(result.plans[0].minutes)} 分までです（全曲を繋いで最大 約 ${Math.round(result.max_minutes)} 分）`, 5000);
    } catch (e) {
      result = null;
      $('res').innerHTML = `<div class="p2card jempty">${esc(e.message)}</div>`;
    } finally {
      busy = false;
      $('go').textContent = 'おまかせで作る';
    }
    if (result) renderResult();
  }

  // ---- 結果 ----
  function renderResult() {
    if (!result) { $('res').innerHTML = '<div class="p2card jempty">条件を選んで「おまかせで作る」を押してください。</div>'; return; }
    const ps = result.plans;
    $('res').innerHTML = `
      <div class="ptabs">${ps.map((p, i) => {
        const arts = [...new Set(p.tracks.map(t => (t.artist ?? '').split(/[,;/]/)[0].slice(0, 18)))].slice(0, 3).join('・');
        const name = cond.mode === 'queue' ? (i === 0 ? 'おまかせの並び' : '今の並び') : `案${i + 1}`;
        return `<div class="ptab ${i === plan ? 'on' : ''}" data-p="${i}"><b>${name}</b> <span style="color:${scoreColor(p.score)};font-family:var(--mono)">相性 ${p.score.toFixed(2)}</span>
          <div>${p.tracks.length}曲 ・ 約${Math.round(p.minutes)}分 ・ ${esc(arts)} …</div>${p.albums ? `<div class="lbl2">アルバムごと ${Object.values(p.albums).join('・')} 曲</div>` : ''}</div>`;
      }).join('')}</div>
      <div class="p2card"><div class="lbl2" style="margin-bottom:6px">ミックス全体（帯をクリックすると一覧でその曲を示します。緑＝拍を合わせて重ねる、黄＝エコーアウト。点線は流れの狙い、橙の線は各曲のエネルギー）</div>
        <canvas class="tlc" data-r="tl"></canvas></div>
      <div class="p2card"><div class="tlist" data-r="list"></div></div>
      <div class="acts">
        <div class="btn primary" data-act="toq">この案をキューに入れる</div>
        ${cond.mode === 'lib' ? '<div class="btn" data-act="again">条件はそのままで作り直す</div>' : ''}
        <span class="lbl2">${result.albums ? `${result.albums} 枚のアルバムの ` : ''}${result.candidates ? `候補 ${result.candidates.toLocaleString()} 曲から` : ''}${result.sec != null ? ` ・ ${result.sec.toFixed(1)} 秒` : ''}${
          cond.mode === 'queue' && ps[0].tracks.map(t => t.id).join() === ps[1].tracks.map(t => t.id).join() ? ' ・ 今の並びのままが一番相性の良い並びでした' : ''}</span>
      </div>`;
    $('res').querySelector('.ptabs').addEventListener('click', e => { const p = e.target.closest('[data-p]'); if (p) { plan = +p.dataset.p; sel = -1; renderResult(); } });
    $('res').querySelector('[data-act=toq]').addEventListener('click', toQueue);
    $('res').querySelector('[data-act=again]')?.addEventListener('click', () => run(ps.flatMap(p => p.tracks.slice(1).map(t => t.id))));
    const tl = $('tl');
    const hit = ev => { const r = tl.getBoundingClientRect(), x = ev.clientX - r.left, y = ev.clientY - r.top; const b = boxes.find(b => x >= b.x0 && x <= b.x1 && y >= b.y0 && y <= b.y1); return b ? b.i : -1; };
    tl.addEventListener('mousemove', ev => { const h = hit(ev); if (h !== hover) { hover = h; drawTL(); } });
    tl.addEventListener('click', ev => { const h = hit(ev); if (h >= 0) { sel = h; drawTL(); renderList(); root.querySelector(`.trow[data-i="${h}"]`)?.scrollIntoView({ block: 'nearest', behavior: 'smooth' }); } });
    drawTL();
    renderList();
  }

  const cur = () => result.plans[plan];

  function layout(tracks) {
    const L = [];
    tracks.forEach((t, i) => {
      const len = t.use.t1 - t.use.t0;
      let start = 0;
      if (i > 0) {
        const p = tracks[i - 1], ov = Math.min((p.join?.bars ?? 16) * 4 * 60 / p.bpm, len * 0.4, L[i - 1].len * 0.4);
        start = L[i - 1].start + L[i - 1].len - ov;
        L[i - 1].ov = ov;
      }
      L.push({ t, start, len, ov: 0 });
    });
    return L;
  }

  function drawTL() {
    const cv = $('tl'); if (!cv) return;
    const [g, W, H, d] = fitCanvas(cv);
    const tracks = cur().tracks, L = layout(tracks);
    const total = Math.max(L.length ? L[L.length - 1].start + L[L.length - 1].len : 60, cond.mode === 'lib' ? cond.minutes * 60 : 0);
    const pad = 10 * d, X = s => pad + s / total * (W - 2 * pad);
    const laneH = 46 * d, top = 22 * d, eTop = top + 2 * laneH + 26 * d, eH = H - eTop - 12 * d;
    g.fillStyle = '#050607'; g.fillRect(0, 0, W, H);
    g.font = `${10 * d}px Consolas`;
    for (let m = 0; m * 60 <= total; m += 5) {
      const x = Math.round(X(m * 60));
      g.fillStyle = 'rgba(255,255,255,.08)'; g.fillRect(x, top, d, H - top - 8 * d);
      g.fillStyle = colors['--faint']; g.fillText(`${m}分`, x + 3 * d, 14 * d);
    }
    boxes = [];
    L.forEach((l, i) => {
      const lane = i % 2, y = top + lane * laneH, x0 = X(l.start), x1 = X(l.start + l.len);
      g.fillStyle = i === sel ? 'rgba(255,255,255,.14)' : i === hover ? 'rgba(255,255,255,.09)' : 'rgba(255,255,255,.05)';
      g.fillRect(x0, y + 2 * d, x1 - x0, laneH - 4 * d);
      const env = l.t.env?.length ? l.t.env : [0.5, 0.5], mid = y + laneH / 2, amp = laneH / 2 - 5 * d;
      g.beginPath(); g.moveTo(x0, mid);
      env.forEach((v, k) => g.lineTo(x0 + (x1 - x0) * k / Math.max(1, env.length - 1), mid - (v * 0.85 + 0.08) * amp));
      for (let k = env.length - 1; k >= 0; k--) g.lineTo(x0 + (x1 - x0) * k / Math.max(1, env.length - 1), mid + (env[k] * 0.85 + 0.08) * amp);
      g.closePath(); g.globalAlpha = 0.75; g.fillStyle = lane ? colors['--deck-b'] : colors['--deck-a']; g.fill(); g.globalAlpha = 1;
      if (i === sel) { g.strokeStyle = '#fff'; g.lineWidth = 2 * d; g.strokeRect(x0, y + 2 * d, x1 - x0, laneH - 4 * d); }
      g.save(); g.beginPath(); g.rect(x0, y, x1 - x0, laneH); g.clip();
      g.fillStyle = '#fff'; g.font = `600 ${11 * d}px "Hiragino Sans", "Yu Gothic UI", sans-serif`; g.shadowColor = '#000'; g.shadowBlur = 3 * d;
      g.fillText(`${i + 1}. ${l.t.title ?? ''}`, x0 + 5 * d, y + 14 * d);
      g.restore();
      boxes.push({ i, x0: x0 / d, x1: x1 / d, y0: y / d, y1: (y + laneH) / d });
    });
    L.forEach(l => {
      if (!l.ov) return;
      const x0 = X(l.start + l.len - l.ov), x1 = X(l.start + l.len), echo = l.t.join?.tpl === 'echo';
      g.fillStyle = echo ? 'rgba(234,179,8,.22)' : 'rgba(34,197,94,.22)'; g.fillRect(x0, top, x1 - x0, 2 * laneH);
      g.fillStyle = echo ? colors['--warn'] : colors['--ok']; g.font = `${10 * d}px "Hiragino Sans", "Yu Gothic UI", sans-serif`;
      g.fillText(TPL[l.t.join?.tpl] ?? '', x0, top + 2 * laneH + 13 * d);
    });
    // エネルギーの流れ
    g.fillStyle = 'rgba(255,255,255,.03)'; g.fillRect(pad, eTop, W - 2 * pad, eH);
    const Y = e => eTop + eH - Math.max(0, Math.min(1, e)) * (eH - 10 * d) - 4 * d;
    if (cond.mode === 'lib' && L.length) {
      const e0 = L[0].t.energy, f = flowFn(result.flow ?? cond.flow);
      g.strokeStyle = 'rgba(255,255,255,.55)'; g.lineWidth = 2 * d; g.setLineDash([6 * d, 5 * d]); g.beginPath();
      for (let k = 0; k <= 60; k++) { const x = X(k / 60 * total), y = Y(f(k / 60, e0)); k ? g.lineTo(x, y) : g.moveTo(x, y); }
      g.stroke(); g.setLineDash([]);
    }
    g.strokeStyle = '#fb923c'; g.lineWidth = 2.5 * d; g.beginPath();
    L.forEach((l, i) => { const x = X(l.start + l.len / 2), y = Y(l.t.energy); i ? g.lineTo(x, y) : g.moveTo(x, y); });
    g.stroke();
    L.forEach(l => { g.fillStyle = '#fb923c'; g.beginPath(); g.arc(X(l.start + l.len / 2), Y(l.t.energy), 4 * d, 0, 7); g.fill(); });
  }

  function partsHTML(j) {
    const items = [['テンポ', j.tempo], ['キー', j.key], ['響き', j.chroma], ['雰囲気', j.mood]];
    return `<span class="parts">${items.map(([n, v]) => `<span class="pt"><i>${n}</i><b style="width:${Math.round(v * 44)}px;background:${scoreColor(v)}"></b></span>`).join('')}</span>`;
  }

  function renderList() {
    const tracks = cur().tracks;
    $('list').innerHTML = tracks.map((t, i) => {
      let h = `<div class="trow ${i === sel ? 'sel' : ''}" data-i="${i}"><span class="no">${i + 1}</span>
        <div><b>${esc(t.title)}</b> ${starsHTML(t)} <small>${esc(t.artist ?? '')}${t.album ? ` ・ <span class="alb">『${esc(t.album)}』</span>` : ''} ・ 使う範囲 ${fmt(t.use.t0)}–${fmt(t.use.t1)}（${fmt(t.use.t1 - t.use.t0)}）${t.use.notes?.length ? '（' + esc(t.use.notes.join('・')) + '）' : ''}</small>
          <span class="segbtn" data-seg="${i}" title="この曲の使う区間を選び直す（繋ぎも計算し直します）">区間 ▾</span></div>
        <span class="mono dim">${t.bpm.toFixed(1)} BPM</span><span class="mono dim">${t.key}</span>
        <span class="rowbtns"><span class="btn" data-pv="${i}">▶ 試聴</span>${i > 0 || cond.mode === 'lib' ? `<span class="btn" data-rep="${i}">別の曲に替える</span>` : ''}</span></div>`;
      // つなぎ目を試聴は、どの曲の間でも同じ位置（左端）に（使ってみての改善）
      // つなぎ方（ミックス／カットイン）はつなぎ目ごとに選べる（design_cutin.md）
      if (t.join) h += `<div class="jrow"><span></span><div class="jbody"><div class="why"><span class="btn" data-jpv="${i}">▶ つなぎ目を試聴</span>
        <span class="seg2 jm" data-jm="${i}" title="このつなぎ目を、重ねて繋ぐ（ミックス）か、小節の頭で切り替える（カットイン）か"><div data-v="mix" class="${t.join.tpl !== 'cut' ? 'on' : ''}">ミックス</div><div data-v="cut" class="${t.join.tpl === 'cut' ? 'on' : ''}">カットイン</div></span>
        <span style="color:${scoreColor(t.join.score)};font-family:var(--mono)">↓ ${t.join.score.toFixed(2)}</span>
        <b style="color:var(--text);font-weight:600">${TPL[t.join.tpl]}${t.join.tpl === 'cut' ? '' : ` ${t.join.bars}小節`}</b>${partsHTML(t.join)}</div>
        <div class="jwhy">${esc(t.join.why.join(' ／ '))}</div></div></div>`;
      return h;
    }).join('');
    $('list').querySelectorAll('[data-pv]').forEach(b => b.addEventListener('click', async () => {
      const t = tracks[+b.dataset.pv];
      previewTrack(await fullTrack(t.id), t.use, 'head');
    }));
    $('list').querySelectorAll('[data-jpv]').forEach(b => b.addEventListener('click', async () => {
      previewJoin(+b.dataset.jpv, await asItems(tracks));
    }));
    $('list').querySelectorAll('[data-rep]').forEach(b => b.addEventListener('click', () => replace(+b.dataset.rep)));
    $('list').querySelectorAll('[data-jm] [data-v]').forEach(b => b.addEventListener('click', ev => {
      ev.stopPropagation();
      const i = +b.parentElement.dataset.jm, v = b.dataset.v;
      if ((tracks[i].join?.tpl === 'cut') === (v === 'cut')) return;
      applySegment(-1, null, null, { [i]: v });
    }));
    $('list').querySelectorAll('[data-seg]').forEach(b => b.addEventListener('click', ev => { ev.stopPropagation(); pickSegment(+b.dataset.seg, b); }));
  }

  // ---- 曲ごとの区間（I-7）: 候補（おまかせ・頭から・盛り上がり中心・終わりまで・曲全体）から選び、同じ並びで繋ぎを計算し直す ----
  async function pickSegment(i, anchor) {
    const tracks = cur().tracks, t = tracks[i];
    const len = t.seg?.len ?? (cond.trackLen || null);
    document.querySelector('.segmenu')?.remove();
    const menu = document.createElement('div');
    menu.className = 'popmenu segmenu';
    menu.innerHTML = '<div class="pm-sec">区間の候補を探しています…</div>';
    document.body.appendChild(menu);
    const r = anchor.getBoundingClientRect();
    menu.style.left = `${Math.min(innerWidth - 360, r.left)}px`; menu.style.top = `${r.bottom + 4}px`;
    const close = ev => { if (!menu.contains(ev.target)) { menu.remove(); removeEventListener('pointerdown', close, true); } };
    addEventListener('pointerdown', close, true);
    const { items } = await fetch(`/api/auto/segments?id=${t.id}${len ? `&len=${len}` : ''}`).then(x => x.json()).catch(() => ({ items: [] }));
    const curKey = `${t.use.bar0}-${t.use.bar1}`;
    menu.innerHTML = `<div class="pm-sec">「${esc(t.title)}」の使う区間${len ? `（1 曲あたり約${Math.round(len / 60)}分）` : ''}</div>` + (items.map((c, k) =>
      `<div class="pm-item ${`${c.use.bar0}-${c.use.bar1}` === curKey ? 'on' : ''}" data-k="${k}"><b>${esc(c.label)}</b> <span class="mono dim">${fmt(c.use.t0)}–${fmt(c.use.t1)}（${fmt(c.use.t1 - c.use.t0)}）</span></div>`).join('')
      || '<div class="pm-sec">候補がありません</div>') +
      `<div class="pm-item" data-custom><b>自分で選ぶ…</b> <span class="dim">波形の上をドラッグして区間を決める</span></div>`;
    menu.querySelectorAll('[data-k]').forEach(el => el.addEventListener('click', async () => {
      const c = items[+el.dataset.k];
      menu.remove(); removeEventListener('pointerdown', close, true);
      applySegment(i, { len: c.len, pos: c.pos }, `「${c.label}」（${fmt(c.use.t0)}–${fmt(c.use.t1)}）`);
    }));
    menu.querySelector('[data-custom]').addEventListener('click', () => {
      menu.remove(); removeEventListener('pointerdown', close, true);
      pickRange(i);
    });
  }

  /** i 曲目の区間を seg（{len, pos, t0?, t1?}）にして、同じ並びで繋ぎを計算し直す。
   *  jm: つなぎ目ごとのつなぎ方の変更 {曲の番号: 'mix' | 'cut'}（その曲から次の曲へ）。ほかのつなぎ目は今のつなぎ方のまま。 */
  async function applySegment(i, seg, what, jm = {}) {
    const tracks = cur().tracks;
    const modeOf = (x, k) => jm[k] ?? (x.join ? (x.join.tpl === 'cut' ? 'cut' : 'mix') : null);
    const spec = tracks.map((x, k) => ({ ...(k === i ? { id: x.id, ...seg }
      : { id: x.id, len: x.seg?.len ?? null, pos: x.seg?.pos ?? 'auto', t0: x.seg?.t0, t1: x.seg?.t1 }), jm: modeOf(x, k) }));
    try {
      const p = await post('/api/auto/rejoin', { tracks: spec, join_mode: cond.joinMode || 'auto' });
      result.plans[plan] = { ...cur(), ...p };
      renderResult();
      if (what) flash(`${i + 1} 曲目を${what}にして、繋ぎを計算し直しました`);
      else for (const [k, v] of Object.entries(jm)) flash(`${+k + 1} → ${+k + 2} 曲目のつなぎを${v === 'cut' ? 'カットイン' : 'ミックス'}にしました${v === 'cut' ? '（切れ目の良い小節へ範囲を寄せ、音量は自動で合わせます）' : ''}`, 3500);
    } catch (e) { flash(e.message); }
  }

  /** 自分で区間を選ぶ: 曲全体の波形の上をドラッグ（小節にそろう。冒頭・最後はそのまま）。 */
  async function pickRange(i) {
    const t = cur().tracks[i];
    const d = await trackData(t.id);
    const dur = d.a.duration, bars = d.barTimes;
    let r = { t0: t.use.t0, t1: t.use.t1 }, drag = null;
    const snap = x => {
      if (x <= (bars[0] ?? 0) + 0.5) return 0;                          // 冒頭（最初の小節頭の前も含める）
      if (x >= dur - 1) return dur;
      let k = 0; for (let j = 1; j < bars.length; j++) if (Math.abs(bars[j] - x) < Math.abs(bars[k] - x)) k = j;
      return bars[k];
    };
    openModal(`<h3>「${esc(t.title)}」の使う区間を選ぶ</h3>
      <p class="lbl2" style="line-height:1.8">波形の上をドラッグして区間を選びます（小節の頭にそろいます。左端までドラッグすると冒頭 0:00 から、右端までで最後まで）。区間の端の近くをドラッグすると、その端だけ動かせます。</p>
      <canvas class="cv" data-rcv style="height:120px;cursor:text"></canvas>
      <div class="exrow"><span class="lbl2">区間</span><b data-rtxt class="mono"></b><span class="sp" style="flex:1"></span>
        <div class="btn" data-rq="top">冒頭（0:00）から</div><div class="btn" data-rq="end">最後まで</div></div>
      <div class="exrow"><div class="btn" data-rpv="head">▶ 頭を試聴</div><div class="btn" data-rpv="tail">▶ 終わりを試聴</div></div>
      <div class="mfoot"><div class="btn" data-close>やめる</div><div class="btn primary" data-rok>この区間にする</div></div>`, m => {
      const cv = m.querySelector('[data-rcv]');
      const X = (s, W) => s / dur * W, T = (x, W) => Math.max(0, Math.min(dur, x / W * dur));
      const paint = () => {
        const [g, W, H, dp] = fitCanvas(cv);
        g.fillStyle = '#050607'; g.fillRect(0, 0, W, H);
        for (const s of d.sections ?? []) { g.fillStyle = 'rgba(255,255,255,.04)'; g.fillRect(X(s.t0, W), 0, dp, H); }
        drawWave(g, d, 0, dur, 0, W, 6 * dp, H - 12 * dp, colors, 0.45);
        g.save(); g.beginPath(); g.rect(X(r.t0, W), 0, X(r.t1, W) - X(r.t0, W), H); g.clip();
        drawWave(g, d, 0, dur, 0, W, 6 * dp, H - 12 * dp, colors, 1); g.restore();
        g.fillStyle = 'rgba(34,211,238,.10)'; g.fillRect(X(r.t0, W), 0, X(r.t1, W) - X(r.t0, W), H);
        g.fillStyle = colors['--deck-a']; g.fillRect(X(r.t0, W) - dp, 0, 2 * dp, H); g.fillRect(X(r.t1, W) - dp, 0, 2 * dp, H);
        m.querySelector('[data-rtxt]').textContent = `${fmt(r.t0)} – ${fmt(r.t1)}（${fmt(r.t1 - r.t0)}）${r.t0 === 0 ? ' ・ 冒頭から' : ''}${r.t1 >= dur ? ' ・ 最後まで' : ''}`;
      };
      cv.addEventListener('pointerdown', ev => {
        const rc = cv.getBoundingClientRect(), x = T(ev.clientX - rc.left, rc.width), near = 8 / rc.width * dur;
        drag = Math.abs(x - r.t0) < near ? { edge: 't0' } : Math.abs(x - r.t1) < near ? { edge: 't1' } : { from: x };
        cv.setPointerCapture(ev.pointerId);
      });
      cv.addEventListener('pointermove', ev => {
        if (!drag) return;
        const rc = cv.getBoundingClientRect(), x = T(ev.clientX - rc.left, rc.width);
        if (drag.edge === 't0') r.t0 = Math.min(snap(x), r.t1 - 10);
        else if (drag.edge === 't1') r.t1 = Math.max(snap(x), r.t0 + 10);
        else { const a = snap(Math.min(drag.from, x)), b = snap(Math.max(drag.from, x)); if (b - a >= 10) r = { t0: a, t1: b }; }
        paint();
      });
      cv.addEventListener('pointerup', () => { drag = null; });
      m.querySelectorAll('[data-rq]').forEach(b => b.addEventListener('click', () => { if (b.dataset.rq === 'top') r.t0 = 0; else r.t1 = dur; paint(); }));
      m.querySelectorAll('[data-rpv]').forEach(b => b.addEventListener('click', async () => previewTrack(await fullTrack(t.id), { t0: r.t0, t1: r.t1 }, b.dataset.rpv)));
      m.querySelector('[data-rok]').addEventListener('click', () => {
        if (r.t1 - r.t0 < 20) { flash('区間が短すぎます（20 秒以上）'); return; }
        closeModal();
        applySegment(i, { len: null, pos: 'custom', t0: r.t0, t1: r.t1 }, `自分で選んだ区間（${fmt(r.t0)}–${fmt(r.t1)}）`);
      });
      requestAnimationFrame(paint);
    });
    document.querySelector('.mbox')?.classList.add('wide');
  }

  // ---- 差し替え（案Y の候補リスト） ----
  function replace(i) {
    const tracks = cur().tracks;
    if (cond.mode === 'queue') { flash('キューの並べ替えでは曲を入れ替えません。キューを直してから並べ替えてください'); return; }
    const prev = tracks[i - 1]?.id ?? null, next = tracks[i + 1]?.id ?? null;
    openCandidates({
      title: `${i + 1} 曲目「${tracks[i].title}」を替える`,
      prev, next, groups: cond.groups, exclude: tracks.map(t => t.id), colors, previewTrack, fullTrack,
      onPick: c => {
        // 差し替えた曲と、その前後のつなぎを入れ替える（前後の相性は候補リストが計算済み）
        const nt = { id: c.id, title: c.title, artist: c.artist, album: c.album, genre: c.genre, bpm: c.bpm, key: c.key, energy: c.energy, use: c.use, duration: c.duration, env: c.env ?? [] };
        if (c.join_next) nt.join = c.join_next;
        tracks[i] = nt;
        if (i > 0 && c.join) tracks[i - 1].join = c.join;
        const js = tracks.filter(t => t.join).map(t => t.join.score);
        cur().score = js.reduce((a, b) => a + b, 0) / Math.max(1, js.length);
        renderResult();
        flash(`「${c.title}」に替えました`);
      },
    });
  }

  // ---- キューへ ----
  async function fullTrack(id) {
    const { items } = await fetch(`/api/tracks?ids=${id}`).then(r => r.json());
    return items[0];
  }
  async function asItems(tracks) {
    const { items } = await fetch(`/api/tracks?ids=${tracks.map(t => t.id).join(',')}`).then(r => r.json());
    const byId = Object.fromEntries(items.map(t => [t.id, t]));
    return tracks.map(t => {
      const full = t.use.t0 <= 0.05 && t.use.t1 >= (t.duration ?? byId[t.id]?.duration ?? 1e9) - 0.05;
      return { track_id: t.id, track: byId[t.id], use_start: full ? null : t.use.t0, use_end: full ? null : t.use.t1,
               join_tpl: t.join?.tpl ?? null, join_bars: t.join?.bars ?? null,
               join_params: t.join?.params && (t.join.params.tempo === 'ramp' || t.join.params.loop || t.join.params.shift
                 || (t.join.params.parts && t.join.params.parts !== 'none') || t.join.tpl === 'cut') ? { ...t.join.params } : null };
    });
  }
  async function toQueue() {
    const list = await asItems(cur().tracks);
    const apply = mode => {
      queue.setAll(list, mode); closeModal(); flash('おまかせの案をキューに入れました。つなぎ目タブで直せます', 3500); openJoins?.();
      // パートごとに鳴らす繋ぎ（Phase 5 B2）の曲は、今のうちに分離を始めておく（1 曲 20 秒ほど。再生・書き出しまでに間に合うように）
      const ids = new Set();
      list.forEach((it, i) => { if (it.join_params?.parts && it.join_params.parts !== 'none' && list[i + 1]) { ids.add(it.track_id); ids.add(list[i + 1].track_id); } });
      ids.forEach(id => fetch(`/api/tracks/${id}/stems-audio`).catch(() => {}));
      onToQueue?.({ mode: cond.mode, flow: cond.mode === 'queue' ? 'キューの並べ替え' : FLOWS[cond.flow]?.name?.replace('…', '') ?? '' });   // ミックスとして残す（Phase 4）
    };
    if (!queue.getItems().length || cond.mode === 'queue') { apply('replace'); return; }
    openModal(`<h3>この案をキューに入れる</h3><p class="lbl2" style="line-height:1.8">キューにはすでに ${queue.getItems().length} 曲あります。</p>
      <div class="mfoot"><div class="btn" data-close>やめる</div><div class="btn" data-a="append">後ろに足す</div><div class="btn primary" data-a="replace">置き換える</div></div>`,
      m => m.querySelectorAll('[data-a]').forEach(b => b.addEventListener('click', () => apply(b.dataset.a))));
  }

  async function refreshGroups() {
    try { groups = (await fetch('/api/genre-groups').then(r => r.json())).groups; } catch { groups = []; }
    try { playlists = (await fetch('/api/itunes/playlists').then(r => r.json())).items; } catch { playlists = []; }
    renderConds();
  }

  async function show() {
    if (!condLoaded) await loadCond();
    await loadSeed();
    await refreshGroups();
    if (result) { drawTL(); }
  }
  addEventListener('resize', () => { if (root.offsetParent) { drawFlows(); if (result) drawTL(); } });
  return { show, refreshGroups };
}
