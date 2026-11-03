// 設定画面（design_phase1.md §8 P1-10）。出力デバイス・音量補正の目標・テンポの幅・音楽フォルダ・MIDI 割り当て・キーボード。
import { DEFAULT_KEYS } from '../input/keyboard.js';
import { modeLabel } from '../input/midi.js';

const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const KEY_NAMES = { Comma: ',', Period: '.' };
const keyLabel = code => KEY_NAMES[code] ?? code.replace(/^Key|^Digit/, '');

export async function putSettings(patch) {
  const r = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(patch) });
  if (!r.ok) throw new Error((await r.json()).detail ?? `設定の保存に失敗しました (${r.status})`);
  return r.json();
}

const itError = e => (e === 'not_found' ? 'iTunes のライブラリ XML が見つかりません' : e);
const ago = t => { const m = Math.round((Date.now() / 1000 - t) / 60); return m < 1 ? 'たった今' : m < 60 ? `${m} 分前` : m < 1440 ? `${Math.round(m / 60)} 時間前` : new Date(t * 1000).toLocaleDateString(); };

/** iTunes との連携（Phase 5 E。design_phase5.md §E-4）。 */
function itunesHTML(it) {
  if (!it) return '';
  const L = it.last ?? {};
  const ok = it.enabled && it.exists && !L.error && L.matched != null;
  const state = !it.enabled ? '<span class="st">連携していません</span>'
    : ok ? `<span class="ok">読み込み済み（${ago(L.at)}。iTunes で変わると自動で読み直します）</span>`
    : L.error && L.error !== 'not_found' ? `<span class="warn">${esc(L.error)}</span>`
    : `<span class="warn">XML が見つかりません${it.xml ? `（${esc(it.xml)}）` : `（${esc(it.default_dir)} に無い）`}</span>`;
  const counts = ok ? `<div class="kv"><span>曲</span><span>${L.tracks.toLocaleString()} 曲中 ${L.matched.toLocaleString()} 曲がライブラリの曲と対応</span>
      <span>★</span><span>${L.rated.toLocaleString()} 曲</span><span>再生回数</span><span>${L.played.toLocaleString()} 曲</span>
      <span>プレイリスト</span><span>${L.playlists} 本</span></div>` : '';
  const howto = it.enabled && !it.exists ? `<p class="note" style="line-height:1.9">ミュージック.app（iTunes）が他のアプリ向けの XML を書き出していません。次の設定を入れると、自動で見つけます。<br>
      <b>ミュージック → 設定… → 詳細 →「ライブラリ XML をほかのアプリケーションと共有」にチェック</b><br>
      入れた後、ミュージックで何か 1 曲再生するか ★ を付けると XML（~/Music/Music/Library.xml）が書き出されます。</p>` : '';
  return `<section><h3>iTunes との連携</h3>
      <p class="note">iTunes の ★・再生回数・最後に再生した日・プレイリストを読むだけで取り込みます（iTunes 側も、音楽ファイルのタグも変えません）。</p>
      <div class="srow"><label>状態</label><div>${state}</div></div>${counts}${howto}
      <div class="srow"><label>ライブラリ XML</label><div class="rootadd" style="flex:1"><input data-itxml value="${esc(it.xml ?? '')}" placeholder="空なら ${esc(it.default_dir)} を探します">
        <div class="btn" data-itsave>この XML を使う</div></div></div>
      <div class="srow" style="align-items:flex-start"><label>★ の扱い</label><div class="itradio">
        <label><input type="radio" name="itrate" data-itrate value="1" ${it.use_rating ? 'checked' : ''}> アプリで付けた ★ を優先し、付けていない曲は iTunes の ★ を使う（ピンクで表示）</label>
        <label><input type="radio" name="itrate" data-itrate value="0" ${it.use_rating ? '' : 'checked'}> iTunes の ★ は使わない（再生回数とプレイリストだけ）</label></div></div>
      <div class="srow"><label></label><div class="btn" data-itimport>今すぐ読み込む</div>${it.enabled && L.matched != null ? '<div class="btn" data-itunlink>連携をやめる</div>' : ''}</div></section>`;
}

/** 音楽フォルダの一覧（使っているもの＋外したが曲が残っているもの）。 */
function rootsHTML(roots) {
  return roots.map(r => r.active
    ? `<div class="rt"><code title="${esc(r.path)}">${esc(r.path)}</code>` +
      `<span class="st ${r.exists ? '' : 'bad'}">${r.exists ? `${r.tracks.toLocaleString()} 曲` : `見つかりません（飛ばしています・${r.tracks.toLocaleString()} 曲は残しています）`}</span>` +
      `<span class="load" data-rootdel="${esc(r.path)}">外す</span></div>`
    : `<div class="rt off"><code title="${esc(r.path)}">${esc(r.path)}</code><span class="st">外しています（${r.tracks.toLocaleString()} 曲を隠しています）</span>` +
      `<span class="load" data-rootback="${esc(r.path)}">戻す</span></div>`).join('') || '<p class="note">音楽フォルダがありません。</p>';
}

export async function outputDevices() {
  try {
    const all = await navigator.mediaDevices.enumerateDevices();
    return all.filter(d => d.kind === 'audiooutput' && d.deviceId !== 'default' && d.deviceId !== 'communications');
  } catch { return []; }
}

/** ctx: {settings, apply(patch), midi, flash} */
export async function renderSettings(root, ctx) {
  const s = ctx.settings;
  const [devices, stats, ports, itn] = await Promise.all([
    outputDevices(),
    fetch('/api/stats').then(r => r.json()).catch(() => null),
    fetch('/api/midi/ports').then(r => r.json()).catch(() => null),
    fetch('/api/itunes').then(r => r.json()).catch(() => null),
  ]);
  const sel = (name, opts, cur) => `<select data-set="${name}">${opts.map(([v, l]) =>
    `<option value="${esc(v)}" ${String(v) === String(cur) ? 'selected' : ''}>${esc(l)}</option>`).join('')}</select>`;
  const maps = ctx.midi?.entries ?? [];   // 起動直後（MIDI の準備前）に描いても落ちないように
  const src = m => `${esc(m.port)} ch${m.ch + 1} ${m.type === 'note' ? `ノート ${m.num}` : m.type === 'pb' ? 'ピッチベンド' : `CC ${m.num}${m.lsb != null ? `+${m.lsb}` : ''}`}`;
  const modeSel = (m, i) => m.type === 'cc' && m.mode !== 'button'
    ? `<select data-mapmode="${i}">${['abs', 'rel-twos', 'rel-offset', 'rel-sign'].map(x => `<option value="${x}" ${x === m.mode ? 'selected' : ''}>${modeLabel(x)}</option>`).join('')}</select>`
    : modeLabel(m.mode);
  root.innerHTML = `
  <div class="settings">
    <section><h3>音声出力</h3>
      <div class="srow"><label>出力デバイス</label>${'setSinkId' in AudioContext.prototype
        ? sel('output_device', [['', '既定のデバイス'], ...devices.map((d, i) => [d.deviceId, d.label || `出力デバイス ${i + 1}`])], s.output_device)
        : '<span class="note">Mac の「システム設定」→「サウンド」→「出力」で選んだ機器から鳴ります</span>'}</div>
      <p class="note">ヘッドホンでの事前試聴（キュー）は使わない設計です（出力は 1 系統）。</p></section>
    <section><h3>音量・テンポ</h3>
      <div class="srow"><label>音量補正の目標</label>${sel('lufs_target', [-18, -16, -14, -12, -10].map(v => [v, `${v} LUFS${v === -14 ? '（標準）' : ''}`]), s.lufs_target)}</div>
      <div class="srow"><label>テンポフェーダーの幅</label>${sel('tempo_range', [[0.08, '±8%'], [0.16, '±16%'], [0.5, '±50%']], s.tempo_range)}</div>
      <div class="srow"><label>クロスフェーダーのカーブ</label>${sel('xfade_curve', [['smooth', 'なめらか'], ['linear', 'リニア'], ['cut', 'カット']], s.xfade_curve)}</div>
      <div class="srow"><label>繋ぎの後の速度</label>${sel('tempo_return', [['true', '8 小節かけて原速へ戻す'], ['false', '合わせた速度のまま']], String(s.tempo_return !== false))}</div></section>
    <section><h3>ライブラリ</h3>
      <div class="srow" style="align-items:flex-start"><label>音楽フォルダ</label><div style="flex:1">
        <div class="rootlist">${rootsHTML(stats?.roots ?? [])}</div>
        <div class="rootadd"><input data-newroot placeholder="追加するフォルダのパス（例: /Volumes/外付け/Music）" style="flex:1">
          <div class="btn" data-addroot>追加</div><div class="btn" data-pickroot>フォルダを選ぶ…</div></div></div></div>
      <p class="note">外したフォルダの曲は、ライブラリ・おまかせ・まとめて解析に出なくなります（消しません。★・履歴・解析結果は残り、「戻す」で元どおりに見えます）。
        外付けドライブが外れているときは、そのフォルダを飛ばします（曲は消しません）。</p>
      <p class="note">${stats ? `${stats.tracks.toLocaleString()} 曲 ／ 解析済み ${stats.analyzed.toLocaleString()} 曲${stats.failed ? ` ／ 解析失敗 ${stats.failed}` : ''}` : ''}</p></section>
    ${itunesHTML(itn)}
    <section><h3>MIDI コントローラー</h3>
      <p class="note">接続中: ${ports?.available === false ? 'MIDI を使えません' : ports?.inputs?.length ? esc(ports.inputs.join('、')) : 'なし'}。
        操作子を右クリック →「MIDI 割り当て」で、コントローラーを動かして割り当てます。</p>
      ${maps.length ? `<table class="stbl"><tr><th>操作</th><th>コントローラー</th><th>種類</th><th></th></tr>${maps.map((m, i) =>
        `<tr><td>${esc(m.action)}</td><td>${src(m)}</td><td>${modeSel(m, i)}</td><td><span class="load" data-mapdel="${i}">削除</span></td></tr>`).join('')}</table>`
        : '<p class="note">割り当てはまだありません。</p>'}</section>
    <section><h3>キーボード</h3>
      <table class="stbl"><tr><th>キー</th><th>操作</th></tr>${Object.entries(DEFAULT_KEYS).map(([k, a]) =>
        `<tr><td class="n">${keyLabel(k)}</td><td>${esc(a)}</td></tr>`).join('')}
        <tr><td class="n">← / →</td><td>クロスフェーダー（Shift で大きく）</td></tr></table></section>
  </div>`;

  root.querySelectorAll('select[data-set]').forEach(el => el.addEventListener('change', () => {
    const k = el.dataset.set;
    const v = ['lufs_target', 'tempo_range'].includes(k) ? +el.value : k === 'tempo_return' ? el.value === 'true' : el.value;
    ctx.apply({ [k]: v });
  }));
  // iTunes との連携（Phase 5 E）
  const itPost = async (url, msg) => {
    const r = await fetch(url, { method: 'POST' });
    const j = await r.json().catch(() => ({}));
    ctx.flash(r.ok ? (j.last?.error ? itError(j.last.error) : msg(j)) : (j.detail ?? '失敗しました'));
    ctx.onItunes?.();
    renderSettings(root, ctx);
  };
  root.querySelector('[data-itimport]')?.addEventListener('click', () => itPost('/api/itunes/import', j => `iTunes から読み込みました（${(j.last.matched ?? 0).toLocaleString()} 曲・プレイリスト ${j.last.playlists ?? 0} 本）`));
  root.querySelector('[data-itunlink]')?.addEventListener('click', () => itPost('/api/itunes/unlink', () => 'iTunes との連携をやめました（アプリの ★・履歴はそのままです）'));
  root.querySelector('[data-itsave]')?.addEventListener('click', async () => {
    try { await ctx.apply({ itunes_xml: root.querySelector('[data-itxml]').value.trim() }); } catch (e) { ctx.flash(e.message); return; }
    itPost('/api/itunes/import', j => `iTunes から読み込みました（${(j.last.matched ?? 0).toLocaleString()} 曲）`);
  });
  root.querySelectorAll('[data-itrate]').forEach(el => el.addEventListener('change', async () => {
    await ctx.apply({ itunes_use_rating: el.value === '1' });
    ctx.flash(el.value === '1' ? 'アプリで ★ を付けていない曲は iTunes の ★ を使います' : 'iTunes の ★ は使いません');
    ctx.onItunes?.();
  }));
  // 音楽フォルダ（I-3）。変えると再走査が始まり、設定画面を描き直す
  const current = (stats?.roots ?? []).filter(r => r.active).map(r => r.path);
  const setRoots = async (list, msg) => {
    try {
      await ctx.apply({ music_roots: list });
      ctx.flash(msg);
      renderSettings(root, ctx);
      // 走査が終わったら曲数を描き直す
      for (let i = 0; i < 600; i++) {
        await new Promise(r => setTimeout(r, 1000));
        const sc = await fetch('/api/library/scan').then(r => r.json()).catch(() => null);
        if (!sc?.running) break;
      }
      if (root.querySelector('.rootlist')) renderSettings(root, ctx);
    } catch (e) { ctx.flash(e.message); }
  };
  const addRoot = p => {
    p = (p ?? '').trim();
    if (!p) return;
    if (current.some(r => r.toLowerCase() === p.toLowerCase())) { ctx.flash('そのフォルダはもう入っています'); return; }
    setRoots([...current, p], `「${p}」を追加し、走査を始めました`);
  };
  root.querySelector('[data-addroot]').addEventListener('click', () => addRoot(root.querySelector('[data-newroot]').value));
  root.querySelector('[data-newroot]').addEventListener('keydown', e => { if (e.key === 'Enter') addRoot(e.target.value); });
  root.querySelector('[data-pickroot]').addEventListener('click', async () => {
    const r = await fetch('/api/dialog/folder', { method: 'POST' });
    const j = await r.json().catch(() => ({}));
    if (!r.ok) { ctx.flash(j.detail ?? 'フォルダ選択を開けませんでした。パスを入力してください'); root.querySelector('[data-newroot]').focus(); return; }
    if (j.path) addRoot(j.path);
  });
  root.querySelectorAll('[data-rootdel]').forEach(el => el.addEventListener('click', () => {
    const p = el.dataset.rootdel;
    if (current.length <= 1) { ctx.flash('音楽フォルダは 1 つ以上必要です（先に別のフォルダを追加してください）'); return; }
    setRoots(current.filter(r => r !== p), `「${p}」を外しました（曲は消さずに隠しています）`);
  }));
  root.querySelectorAll('[data-rootback]').forEach(el => el.addEventListener('click', () => {
    const p = el.dataset.rootback;
    setRoots([...current, p], `「${p}」を戻しました`);
  }));
  root.querySelectorAll('[data-mapmode]').forEach(el => el.addEventListener('change', () => {
    ctx.midi.entries[+el.dataset.mapmode].mode = el.value;
    ctx.midi.save();
  }));
  root.querySelectorAll('[data-mapdel]').forEach(el => el.addEventListener('click', () => {
    ctx.midi.entries.splice(+el.dataset.mapdel, 1);
    ctx.midi.save();
    renderSettings(root, ctx);
  }));
}
