/* Unity Asset Index web UI. Plain JS, no build step; talks to /api/* on the same origin. */
(() => {
  'use strict';

  const $ = (id) => document.getElementById(id);
  const el = (tag, attrs = {}, ...children) => {
    const n = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs)) {
      if (k === 'class') n.className = v;
      else if (k === 'text') n.textContent = v;
      else if (k === 'html') n.innerHTML = v;
      else if (k.startsWith('on')) n.addEventListener(k.slice(2), v);
      else if (v !== null && v !== undefined && v !== false) n.setAttribute(k, v === true ? '' : v);
    }
    for (const c of children) if (c !== null && c !== undefined && c !== false) n.append(c.nodeType ? c : document.createTextNode(String(c)));
    return n;
  };
  const human = (n) => {
    n = Number(n) || 0;
    for (const u of ['B', 'KB', 'MB', 'GB', 'TB']) {
      if (n < 1024 || u === 'TB') return (u === 'B' ? n.toFixed(0) : n.toFixed(1)) + u;
      n /= 1024;
    }
  };
  const KIND_ICON = {
    prefab: '◈', material: '●', shader: '✦', texture: '▨', model: '⬡', audio: '♪', animation: '▶', scene: '⛰',
    script: '{ }', asset: '▣', font: 'A', ui: '▭', video: '▶', doc: '¶', data: '{}', vfx: '✺', folder: '▰', other: '•',
  };
  const KINDS = ['prefab', 'material', 'shader', 'texture', 'model', 'audio', 'animation', 'scene', 'script', 'asset', 'font', 'ui', 'video', 'doc', 'data', 'vfx'];
  const IMG_EXT = new Set(['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'svg']);
  const AUDIO_EXT = new Set(['wav', 'mp3', 'ogg', 'flac', 'aif', 'aiff']);
  const VIDEO_EXT = new Set(['mp4', 'webm', 'mov']);
  const MODEL_EXT = new Set(['fbx', 'obj', 'glb', 'gltf']);
  let activeViewer = null;
  function dropViewer() { if (activeViewer) { try { activeViewer.dispose(); } catch (_) {} activeViewer = null; } }

  /* Mount the three.js viewer into the preview box and load a model; returns the viewer + a toolbar. */
  async function show3D(pv, a, modelAsset, after) {
    if (!window.UaiViewer) { toast('3D viewer is not loaded', true); return null; }
    dropViewer();
    pv.innerHTML = '';
    pv.classList.add('viewer');
    const bar = el('div', { class: 'viewer-bar' });
    pv.after(bar);
    bar.append(el('span', { class: 'muted' }, el('span', { class: 'spinner' }), ` loading ${modelAsset.name} (${human(modelAsset.size)})…`));
    const viewer = window.UaiViewer.createViewer(pv, { textureUrl: (name) => `/api/raw?ident=${encodeURIComponent(name)}&package=${modelAsset.package_id}&near=${modelAsset.id}` });
    activeViewer = viewer;
    window.__uaiViewer = viewer;
    try {
      const info = await viewer.load(`/api/assets/${modelAsset.id}/raw`, (modelAsset.ext || '').toLowerCase());
      if (activeViewer !== viewer) return null;
      bar.innerHTML = '';
      if (info.clips.length) {
        const sel = el('select', { 'data-clip': '1', onchange: () => viewer.play(+sel.value) });
        info.clips.forEach((c, i) => sel.append(el('option', { value: String(i), text: `${c.name || 'clip ' + (i + 1)} (${c.duration.toFixed(2)}s)` })));
        bar.append(el('span', { class: 'muted', text: 'clip' }), sel);
      }
      let paused = false;
      const pause = el('button', { class: 'ghost', type: 'button', text: '❚❚', title: 'Pause / resume', onclick: () => { paused = !paused; viewer.setPaused(paused); pause.textContent = paused ? '▶' : '❚❚'; } });
      const speed = el('select', { onchange: () => viewer.setSpeed(+speed.value) }, ...['0.25', '0.5', '1', '2'].map((v) => el('option', { value: v, text: v + '×', selected: v === '1' })));
      bar.append(pause, speed, el('button', { class: 'ghost', type: 'button', text: 'Fit', onclick: () => viewer.frame() }));
      bar.append(el('span', { class: 'muted', text: `${info.bones} bones · ${info.clips.length} clip(s) · drag to orbit, wheel to zoom` }));
      if (after) await after(viewer, bar, info);
      return viewer;
    } catch (e) {
      bar.innerHTML = '';
      bar.append(el('span', { class: 'muted', text: 'Could not load: ' + (e && e.message ? e.message : e) }));
      return null;
    }
  }
  const PAGE = 60;

  const state = {
    q: '', kinds: new Set(), package: '', publisher: '', ext: '', offset: 0, results: [], view: 'grid',
    basket: new Map(), detailId: null, packages: [], kindCounts: {}, browsing: null,
  };
  try { state.view = localStorage.getItem('uai.view') || 'grid'; } catch (_) {}
  try { const b = JSON.parse(localStorage.getItem('uai.basket') || '[]'); for (const a of b) state.basket.set(a.guid, a); } catch (_) {}

  let toastTimer = null;
  function toast(msg, err = false) {
    const t = $('toast');
    t.textContent = msg;
    t.className = 'toast' + (err ? ' err' : '');
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => t.classList.add('hidden'), err ? 6000 : 2800);
  }

  async function api(path, opts) {
    const r = await fetch(path, opts);
    if (!r.ok) {
      let msg = `HTTP ${r.status}`;
      try { const j = await r.json(); msg = j.error || msg; } catch (_) {}
      throw new Error(msg);
    }
    return r.json();
  }
  const postJson = (path, body) => api(path, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });

  // ----- URL state --------------------------------------------------------------------------------
  function writeHash() {
    const p = new URLSearchParams();
    if (state.q) p.set('q', state.q);
    if (state.kinds.size) p.set('kind', [...state.kinds].join(','));
    if (state.package) p.set('package', state.package);
    if (state.publisher) p.set('publisher', state.publisher);
    if (state.ext) p.set('ext', state.ext);
    if (state.detailId) p.set('asset', state.detailId);
    const h = p.toString();
    if (('#' + h) !== location.hash) history.replaceState(null, '', h ? '#' + h : location.pathname);
  }
  function readHash() {
    const p = new URLSearchParams(location.hash.replace(/^#/, ''));
    state.q = p.get('q') || '';
    state.kinds = new Set((p.get('kind') || '').split(',').filter(Boolean));
    state.package = p.get('package') || '';
    state.publisher = p.get('publisher') || '';
    state.ext = p.get('ext') || '';
    state.detailId = p.get('asset') ? Number(p.get('asset')) : null;
  }

  // ----- header / filters ---------------------------------------------------------------------------
  function renderFilters() {
    $('q').value = state.q;
    $('ext').value = state.ext;
    const chips = $('kindChips');
    chips.innerHTML = '';
    for (const k of KINDS) {
      const n = state.kindCounts[k];
      const c = el('button', { type: 'button', class: 'chip' + (state.kinds.has(k) ? ' active' : ''), onclick: () => { state.kinds.has(k) ? state.kinds.delete(k) : state.kinds.add(k); search(); } }, k, n ? el('span', { class: 'n', text: n.toLocaleString() }) : null);
      chips.append(c);
    }
    $('packageSel').value = state.package;
    $('publisherSel').value = state.publisher;
  }

  async function loadMeta() {
    const [stats, packages, publishers, kinds] = await Promise.all([api('/api/stats'), api('/api/packages'), api('/api/publishers'), api('/api/kinds')]);
    state.packages = packages;
    for (const k of kinds) state.kindCounts[k.kind] = k.n;
    $('stats').textContent = `${stats.packages} packages · ${stats.assets.toLocaleString()} assets · ${human(stats.bytes)}` + (stats.library_mounted ? ((stats.libraries || []).some((l) => !l.mounted) ? ' · some roots offline' : '') : ' · library offline');
    const ps = $('packageSel');
    ps.innerHTML = '<option value="">All packages</option>';
    for (const p of packages) ps.append(el('option', { value: String(p.id), text: `${p.name} (${p.entry_count.toLocaleString()})` }));
    const pub = $('publisherSel');
    pub.innerHTML = '<option value="">All publishers</option>';
    for (const p of publishers) pub.append(el('option', { value: p, text: p }));
    renderPackageList();
  }

  function renderPackageList() {
    const f = ($('pkgFilter').value || '').toLowerCase();
    const list = $('pkgList');
    list.innerHTML = '';
    let lastPub = null;
    for (const p of state.packages) {
      if (f && !(`${p.name} ${p.publisher} ${p.title || ''}`.toLowerCase().includes(f))) continue;
      if (p.publisher !== lastPub) { list.append(el('li', { class: 'pub', text: p.publisher || '—' })); lastPub = p.publisher; }
      const li = el('li', { class: String(p.id) === state.package ? 'active' : '', title: (p.root ? p.root + '/' : '') + p.rel_path, onclick: () => selectPackage(p) },
        el('span', { class: 'name', text: p.name }),
        el('span', { class: 'sub', text: `${p.entry_count.toLocaleString()} assets · ${human(p.total_bytes)}${p.version ? ' · v' + p.version : ''}${p.status !== 'ok' ? ' · ' + p.status : ''}` }));
      list.append(li);
    }
  }

  function selectPackage(p) {
    state.package = String(p.id);
    state.browsing = { pkg: p, prefix: 'Assets' };
    state.q = '';
    search();
  }

  // ----- search / results ----------------------------------------------------------------------------
  let searchSeq = 0;
  async function search(append = false) {
    if (!append) { state.offset = 0; state.results = []; }
    const seq = ++searchSeq;
    renderFilters();
    writeHash();
    $('pkgList').querySelectorAll('li').forEach((li) => li.classList.remove('active'));
    renderPackageList();
    const p = new URLSearchParams({ q: state.q, limit: String(PAGE), offset: String(state.offset) });
    if (state.kinds.size) p.set('kind', [...state.kinds].join(','));
    if (state.package) p.set('package', state.package);
    if (state.publisher) p.set('publisher', state.publisher);
    if (state.ext) p.set('ext', state.ext);
    $('more').classList.add('hidden');
    $('empty').classList.add('hidden');
    if (!append) $('grid').innerHTML = '<div class="muted"><span class="spinner"></span> searching…</div>';
    let rows;
    try { rows = await api('/api/search?' + p); } catch (e) { toast(e.message, true); rows = []; }
    if (seq !== searchSeq) return;
    state.results = append ? state.results.concat(rows) : rows;
    state.offset += rows.length;
    renderResults();
    $('more').classList.toggle('hidden', rows.length < PAGE);
    $('empty').classList.toggle('hidden', state.results.length > 0);
    renderBrowser();
  }

  function titleFor() {
    const parts = [];
    if (state.q) parts.push(`“${state.q}”`);
    if (state.kinds.size) parts.push([...state.kinds].join(', '));
    if (state.package) { const p = state.packages.find((x) => String(x.id) === state.package); if (p) parts.push('in ' + p.name); }
    if (state.publisher) parts.push('by ' + state.publisher);
    if (state.ext) parts.push('.' + state.ext);
    return parts.length ? parts.join(' · ') : 'Browse';
  }

  function thumb(a, big = false) {
    const box = el('div', { class: big ? 'preview-box' : 'thumb' });
    if (a.has_preview) {
      const img = el('img', { loading: 'lazy', alt: '', src: `/api/assets/${a.id}/preview.png` });
      img.addEventListener('error', () => { img.replaceWith(el('span', { class: 'ph', text: KIND_ICON[a.kind] || '•' })); });
      box.append(img);
    } else {
      box.append(el('span', { class: 'ph', text: KIND_ICON[a.kind] || '•' }));
    }
    if (!big) box.append(el('span', { class: 'kind', text: a.kind }));
    return box;
  }

  function renderResults() {
    const g = $('grid');
    g.className = 'grid' + (state.view === 'list' ? ' list' : '');
    g.innerHTML = '';
    $('resultsTitle').textContent = titleFor() + (state.results.length ? ` — ${state.results.length}${state.results.length % PAGE === 0 ? '+' : ''}` : '');
    const frag = document.createDocumentFragment();
    for (const a of state.results) frag.append(card(a));
    g.append(frag);
  }

  function card(a) {
    const inBasket = state.basket.has(a.guid);
    const c = el('div', { class: 'card' + (inBasket ? ' in-basket' : '') + (a.id === state.detailId ? ' selected' : ''), 'data-id': a.id, onclick: () => openDetail(a.id) },
      thumb(a),
      el('div', { class: 'body' },
        el('div', { class: 'name', text: a.name, title: a.path }),
        el('div', { class: 'sub', text: `${a.package} · ${human(a.size)}` })),
      el('button', { class: 'add', type: 'button', title: inBasket ? 'Remove from basket' : 'Add to basket', text: inBasket ? '✓' : '+', onclick: (e) => { e.stopPropagation(); toggleBasket(a); } }));
    return c;
  }

  // ----- package browser ---------------------------------------------------------------------------
  async function renderBrowser() {
    const box = $('pkgBrowser');
    const b = state.browsing;
    if (!b || String(b.pkg.id) !== state.package) { box.classList.add('hidden'); box.innerHTML = ''; state.browsing = null; return; }
    box.classList.remove('hidden');
    const p = b.pkg;
    box.innerHTML = '';
    box.append(el('h2', { text: `${p.publisher} / ${p.name}` }));
    const meta = el('div', { class: 'meta' });
    if (p.title && p.title !== p.name) meta.append(el('span', { text: p.title }));
    if (p.version) meta.append(el('span', { text: 'v' + p.version }));
    if (p.unity_version) meta.append(el('span', { text: 'Unity ' + p.unity_version }));
    if (p.category_label || p.category) meta.append(el('span', { text: p.category_label || p.category }));
    if (p.pubdate) meta.append(el('span', { text: p.pubdate }));
    meta.append(el('span', { text: `${p.entry_count.toLocaleString()} assets · ${human(p.total_bytes)} unpacked · ${human(p.size)} package` }));
    if (p.cached) meta.append(el('span', { text: 'cached locally' }));
    if (p.root) meta.append(el('span', { text: p.root + '/' + p.rel_path, title: 'package file' }));
    box.append(meta);
    if (p.description) {
      const d = el('div', { class: 'desc', text: p.description.replace(/<[^>]+>/g, '') });
      d.addEventListener('click', () => d.classList.toggle('open'));
      box.append(d);
    }
    const crumbs = el('div', { class: 'crumbs' });
    const parts = b.prefix.split('/').filter(Boolean);
    parts.forEach((seg, i) => {
      if (i) crumbs.append('/');
      crumbs.append(el('button', { type: 'button', text: seg, onclick: () => { b.prefix = parts.slice(0, i + 1).join('/'); browseFolder(); } }));
    });
    box.append(crumbs);
    const dirs = el('div', { class: 'dirs', html: '<span class="muted"><span class="spinner"></span></span>' });
    box.append(dirs);
    try {
      const l = await api('/api/ls?' + new URLSearchParams({ package: String(p.id), prefix: b.prefix + '/', limit: '20000' }));
      const subdirs = new Map();
      let files = 0;
      const base = b.prefix + '/';
      for (const a of l.assets) {
        if (!a.path.startsWith(base)) continue;
        const rest = a.path.slice(base.length);
        const i = rest.indexOf('/');
        if (i >= 0) subdirs.set(rest.slice(0, i), (subdirs.get(rest.slice(0, i)) || 0) + 1);
        else if (!a.is_folder) files++;
      }
      dirs.innerHTML = '';
      for (const [d, n] of [...subdirs].sort((x, y) => x[0].localeCompare(y[0]))) {
        dirs.append(el('button', { type: 'button', class: 'dir', onclick: () => { b.prefix = base + d; browseFolder(); } }, '▰ ' + d, el('span', { class: 'muted', text: ` ${n}` })));
      }
      if (!subdirs.size && !files) dirs.append(el('span', { class: 'muted', text: 'empty folder' }));
      // Show this folder's direct files as the result grid when not searching.
      if (!state.q && state.browsing === b) {
        const direct = l.assets.filter((a) => !a.is_folder && a.path.startsWith(base) && !a.path.slice(base.length).includes('/'));
        const all = l.assets.filter((a) => !a.is_folder);
        const rows = direct.length ? direct : all;
        if (!direct.length && all.length) dirs.append(el('span', { class: 'muted', text: ` · showing all ${all.length.toLocaleString()} files below` }));
        state.results = rows.slice(0, 600);
        $('more').classList.add('hidden');
        $('empty').classList.toggle('hidden', rows.length > 0);
        renderResults();
        $('resultsTitle').textContent = `${p.name} / ${b.prefix}${rows.length > 600 ? ` — first 600 of ${rows.length}` : ` — ${rows.length}`}`;
      }
    } catch (e) { dirs.textContent = e.message; }
  }
  function browseFolder() { state.q = ''; renderFilters(); writeHash(); renderBrowser(); }

  // ----- detail -------------------------------------------------------------------------------------
  async function openDetail(id) {
    state.detailId = id;
    writeHash();
    document.querySelectorAll('.card.selected').forEach((c) => c.classList.remove('selected'));
    const c = document.querySelector(`.card[data-id="${id}"]`);
    if (c) c.classList.add('selected');
    const panel = $('detail');
    panel.classList.remove('hidden');
    document.querySelector('.layout').classList.add('has-detail');
    panel.innerHTML = '<div class="detail-inner"><span class="spinner"></span></div>';
    let info;
    try { info = await api(`/api/assets/${id}`); } catch (e) { panel.innerHTML = `<div class="detail-inner"><p class="muted">${e.message}</p></div>`; return; }
    if (state.detailId !== id) return;
    renderDetail(info);
  }
  function closeDetail() {
    dropViewer();
    state.detailId = null;
    writeHash();
    $('detail').classList.add('hidden');
    document.querySelector('.layout').classList.remove('has-detail');
    document.querySelectorAll('.card.selected').forEach((c) => c.classList.remove('selected'));
  }

  function renderDetail(info) {
    const a = info;
    const panel = $('detail');
    dropViewer();
    panel.innerHTML = '';
    const inner = el('div', { class: 'detail-inner' });
    const inBasket = state.basket.has(a.guid);
    inner.append(el('div', { class: 'detail-head' },
      el('h2', { text: a.name }),
      el('button', { class: 'ghost', type: 'button', text: inBasket ? '✓ In basket' : '+ Basket', onclick: (e) => { toggleBasket(a); e.target.textContent = state.basket.has(a.guid) ? '✓ In basket' : '+ Basket'; } }),
      el('button', { class: 'ghost', type: 'button', text: '✕', 'aria-label': 'Close', onclick: closeDetail })));
    const pv = thumb(a, true);
    inner.append(pv);
    const tools = el('div', { class: 'preview-tools' });
    const ext = (a.ext || '').toLowerCase();
    const raw = `/api/assets/${a.id}/raw`;
    if (IMG_EXT.has(ext)) tools.append(el('button', { class: 'ghost', type: 'button', text: `View original (${human(a.size)})`, onclick: (e) => { e.target.disabled = true; pv.innerHTML = ''; const img = el('img', { src: raw, alt: a.name }); img.addEventListener('error', () => toast('Could not load the original image', true)); pv.append(img); } }));
    if (AUDIO_EXT.has(ext)) tools.append(el('button', { class: 'ghost', type: 'button', text: `▶ Play (${human(a.size)})`, onclick: (e) => { e.target.disabled = true; pv.innerHTML = ''; pv.append(el('audio', { controls: true, autoplay: true, src: raw })); } }));
    if (VIDEO_EXT.has(ext)) tools.append(el('button', { class: 'ghost', type: 'button', text: `▶ Play (${human(a.size)})`, onclick: (e) => { e.target.disabled = true; pv.innerHTML = ''; pv.append(el('video', { controls: true, autoplay: true, src: raw })); } }));
    if (MODEL_EXT.has(ext)) tools.append(el('button', { class: 'primary', type: 'button', text: `View in 3D (${human(a.size)})`, onclick: (e) => { e.target.disabled = true; show3D(pv, a, a); } }));
    if (a.has_preview) tools.append(el('button', { class: 'ghost', type: 'button', text: 'Load preview from package', title: 'The thumbnail is read from the package file and cached for next time', onclick: (e) => { e.target.disabled = true; pv.innerHTML = '<span class="spinner"></span>'; const img = el('img', { src: `/api/assets/${a.id}/preview.png?extract=1&t=${Date.now()}`, alt: a.name }); img.addEventListener('load', () => { pv.innerHTML = ''; pv.append(img); }); img.addEventListener('error', () => { pv.innerHTML = ''; pv.append(el('span', { class: 'ph', text: KIND_ICON[a.kind] || '•' })); toast('Could not load the preview', true); }); } }));
    if (!a.is_folder) tools.append(el('a', { href: raw + '?download=1', download: a.name, text: 'download file' }));
    if (tools.children.length) inner.append(tools);
    if (!a.package_cached && (AUDIO_EXT.has(ext) || IMG_EXT.has(ext) || VIDEO_EXT.has(ext))) tools.append(el('span', { class: 'muted', text: 'streams from the package; large packages take a while' }));

    const kv = el('dl', { class: 'kv' });
    const row = (k, v) => { if (v === null || v === undefined || v === '') return; kv.append(el('dt', { text: k }), el('dd', {}, v)); };
    row('path', el('span', { class: 'mono', text: a.path }));
    row('guid', el('span', { class: 'mono', text: a.guid }));
    row('kind', `${a.kind}${a.main_class ? ' · ' + a.main_class : ''}${a.importer ? ' · ' + a.importer : ''}`);
    row('size', human(a.size));
    row('package', el('a', { href: '#', text: `${a.publisher} / ${a.package}`, onclick: (e) => { e.preventDefault(); const p = state.packages.find((x) => x.id === a.package_id); if (p) selectPackage(p); } }));
    if (a.labels && a.labels.length) row('labels', a.labels.join(', '));
    if (a.same_guid_in && a.same_guid_in.length) row('also in', a.same_guid_in.map((o) => o.package).join(', '));
    row('cli', el('code', { class: 'mono', text: `uai export ${a.guid} --project <UnityProject>` }));
    inner.append(kv);

    const tabs = el('div', { class: 'tabs' });
    const body = el('div', { class: 'tab-body' });
    const tabDefs = [
      ['Dependencies', () => renderDeps(a, body)],
      [`Used by (${a.referrers.length})`, () => renderReferrers(a, body)],
    ];
    if (ext === 'anim') tabDefs.unshift(['Animation', () => renderAnim(a, body, pv)]);
    if (a.is_text) tabDefs.push(['Content', () => renderText(a, body)]);
    tabDefs.forEach(([label, fn], i) => {
      const b = el('button', { type: 'button', text: label, class: i === 0 ? 'active' : '', onclick: () => { tabs.querySelectorAll('button').forEach((x) => x.classList.remove('active')); b.classList.add('active'); fn(); } });
      tabs.append(b);
    });
    inner.append(tabs, body);
    panel.append(inner);
    tabDefs[0][1]();
  }

  async function renderDeps(a, body) {
    body.innerHTML = '<span class="spinner"></span>';
    let cl;
    try { cl = await postJson('/api/deps', { identifiers: ['#' + a.id], include_scripts: true }); } catch (e) { body.textContent = e.message; return; }
    const byGuid = new Map(cl.assets.map((x) => [x.guid, x]));
    const unres = new Map(cl.unresolved.map((u) => [u.guid, u]));
    body.innerHTML = '';
    body.append(el('div', { class: 'summary', text: `${cl.assets.length} asset(s) · ${human(cl.total_bytes)} · ${cl.packages.length} package(s) · ${cl.unresolved.length} unresolved` }));
    const printed = new Set();
    const walk = (guid, depth) => {
      const li = el('li');
      const node = byGuid.get(guid);
      if (!node) {
        const u = unres.get(guid);
        li.append(el('span', { class: 'node unres', title: guid }, el('span', { class: 'k', text: '??' }), u && u.label ? u.label : `unresolved ${guid.slice(0, 8)}…`));
        return li;
      }
      const dup = printed.has(guid);
      li.append(el('span', { class: 'node', title: node.path, onclick: () => openDetail(node.id) },
        el('span', { class: 'k', text: node.kind }), node.name,
        node.package_id !== a.package_id ? el('span', { class: 'pkgtag', text: ` [${node.package}]` }) : null,
        dup ? el('span', { class: 'muted', text: ' (see above)' }) : null));
      if (dup || depth >= 8) return li;
      printed.add(guid);
      const kids = cl.edges[guid] || [];
      if (kids.length) {
        const ul = el('ul');
        for (const k of kids) ul.append(walk(k, depth + 1));
        li.append(ul);
      }
      return li;
    };
    const tree = el('ul', { class: 'tree' });
    for (const r of cl.roots) tree.append(walk(r, 0));
    body.append(tree);
    if (cl.skipped_scripts.length) body.append(el('div', { class: 'muted', text: `${cl.skipped_scripts.length} scripts skipped` }));
  }

  function renderReferrers(a, body) {
    body.innerHTML = '';
    if (!a.referrers.length) { body.append(el('p', { class: 'muted', text: 'Nothing in the library references this asset.' })); return; }
    const ul = el('ul', { class: 'list-plain' });
    for (const r of a.referrers) ul.append(el('li', { onclick: () => openDetail(r.id), title: r.path }, el('span', { class: 'badge', text: r.kind }), ' ', r.name, el('span', { class: 'muted', text: `  [${r.package}]` })));
    body.append(ul);
  }

  /* .anim clips: stats + curves, and playback on an FBX from the same package (generic rigs only). */
  async function renderAnim(a, body, pv) {
    body.innerHTML = '<span class="spinner"></span>';
    let parsed;
    try {
      const t = await api(`/api/text?ident=${encodeURIComponent('#' + a.id)}&max_bytes=${64 << 20}`);
      parsed = window.UaiViewer ? window.UaiViewer.parseUnityAnim(t.text) : null;
    } catch (e) { body.textContent = e.message; return; }
    body.innerHTML = '';
    if (!parsed) { body.append(el('p', { class: 'muted', text: '3D viewer is not loaded.' })); return; }
    const wrap = { 0: 'default', 1: 'once', 2: 'loop', 4: 'ping-pong', 8: 'clamp forever' }[parsed.wrapMode] || String(parsed.wrapMode);
    const dl = el('dl', { class: 'anim-summary' });
    const row = (k, v) => dl.append(el('dt', { text: k }), el('dd', { text: v }));
    row('length', `${parsed.length.toFixed(2)} s · ${parsed.sampleRate} fps · ${Math.round(parsed.length * parsed.sampleRate)} frames`);
    row('wrap', wrap + (parsed.legacy ? ' · legacy' : ''));
    row('type', parsed.humanoid ? `humanoid (${parsed.muscleCurves} muscle curves)` : 'generic (transform curves)');
    const sprites = parsed.pptr.filter((c) => /m_Sprite/.test(c.attribute) || !c.attribute);
    const spriteFrames = sprites.reduce((n, c) => n + c.keys.length, 0);
    row('curves', `${parsed.euler.length} euler · ${parsed.rotation.length} rotation · ${parsed.position.length} position · ${parsed.scale.length} scale · ${parsed.floats.length} float` + (spriteFrames ? ` · ${spriteFrames} sprite frames` : ''));
    body.append(dl);
    if (spriteFrames) { await renderFlipbook(a, body, pv, parsed, sprites[0]); return; }
    const paths = [...new Set([...parsed.euler, ...parsed.rotation, ...parsed.position, ...parsed.scale].map((c) => c.path || '(root)'))];
    if (paths.length) body.append(el('div', { class: 'anim-paths', text: paths.join('\n') }));
    else if (parsed.floats.length) body.append(el('div', { class: 'anim-paths', text: parsed.floats.slice(0, 60).map((f) => (f.path ? f.path + ' : ' : '') + f.attribute).join('\n') + (parsed.floats.length > 60 ? `\n… ${parsed.floats.length - 60} more` : '') }));
    const tokens = (s) => new Set(s.toLowerCase().split(/[^a-z0-9]+/).filter((t) => t.length > 2));
    const animTokens = tokens(a.path);
    const overlap = (name) => [...tokens(name)].filter((t) => animTokens.has(t)).length;
    if (parsed.humanoid || !paths.length) {
      body.append(el('p', { class: 'muted', text: parsed.humanoid
        ? 'Humanoid clips are stored as muscle values and need Unity\'s retargeter to play; the curve list above is the best preview outside Unity.'
        : 'This clip animates no transforms, so there is nothing to play on a model.' }));
      // Publishers often ship the source FBX with the clips embedded; those play in the 3D viewer.
      try {
        const l = await api('/api/ls?' + new URLSearchParams({ package: String(a.package_id), kind: 'model', limit: '2000' }));
        const cands = l.assets.filter((m) => /^fbx$/i.test(m.ext) && /(anim|rig|source|motion|mocap)/i.test(m.path))
          .map((m) => ({ m, s: overlap(m.path) })).sort((x, y) => y.s - x.s || y.m.size - x.m.size).slice(0, 6);
        if (cands.length) {
          const ul = el('ul', { class: 'list-plain' });
          for (const { m } of cands) ul.append(el('li', { onclick: () => openDetail(m.id), title: m.path }, el('span', { class: 'badge', text: 'fbx' }), ' ', m.name, el('span', { class: 'muted', text: '  ' + human(m.size) })));
          body.append(el('div', { class: 'muted', text: 'FBX files in this package that may contain this animation (open and use View in 3D):' }), ul);
        }
      } catch (_) {}
      return;
    }
    // Candidate rigs: FBX files in the same package, nearest folder first.
    const ctl = el('div', { class: 'viewer-bar' }, el('span', { class: 'muted' }, el('span', { class: 'spinner' }), ' finding models…'));
    body.append(ctl);
    let models = [];
    try {
      const l = await api('/api/ls?' + new URLSearchParams({ package: String(a.package_id), kind: 'model', limit: '2000' }));
      const dirOf = (p) => p.split('/').slice(0, -1);
      const mine = dirOf(a.path);
      const common = (p) => { const d = dirOf(p); let i = 0; while (i < d.length && i < mine.length && d[i] === mine[i]) i++; return i; };
      // Nearest folder first, then shared name tokens (e.g. "Rig_Medium"), then the bigger file (characters beat props).
      models = l.assets.filter((m) => /^(fbx|obj|glb|gltf)$/i.test(m.ext))
        .sort((x, y) => common(y.path) - common(x.path) || overlap(y.name) - overlap(x.name) || y.size - x.size || x.path.localeCompare(y.path));
    } catch (_) {}
    ctl.innerHTML = '';
    if (!models.length) { ctl.append(el('span', { class: 'muted', text: 'No model files in this package to play the clip on.' })); return; }
    const sel = el('select', { title: 'Model to play the clip on' });
    models.forEach((m) => sel.append(el('option', { value: String(m.id), text: m.name })));
    const axis = el('select', { title: 'Axis convention fix' }, el('option', { value: 'x', text: 'mirror X (Unity default)' }), el('option', { value: 'z', text: 'mirror Z' }), el('option', { value: 'none', text: 'no mirroring' }));
    const status = el('span', { class: 'muted' });
    const run = async () => {
      const m = models.find((x) => String(x.id) === sel.value);
      status.textContent = '';
      await show3D(pv, a, m, async (viewer, bar) => {
        const { clip, matched, missing } = window.UaiViewer.clipFromUnityAnim(parsed, viewer.model, { axis: axis.value, name: a.name });
        viewer.playClip(clip);
        status.textContent = `${matched.length} of ${matched.length + missing.length} animated nodes found in ${m.name}` + (missing.length ? ` (missing: ${missing.slice(0, 5).join(', ')}${missing.length > 5 ? '…' : ''})` : '');
        const r = bar.querySelector('select[data-clip]'); if (r) { r.previousSibling && r.previousSibling.remove(); r.remove(); } // the model's own clips are irrelevant here
      });
    };
    ctl.append(el('span', { class: 'muted', text: 'play on' }), sel, axis, el('button', { class: 'primary', type: 'button', text: '▶ Play on model', onclick: run }), status);
    axis.addEventListener('change', () => { if (activeViewer) run(); });
  }

  /* 2D sprite animation: resolve each frame's sprite guid to its texture and cycle through them. */
  async function renderFlipbook(a, body, pv, parsed, curve) {
    dropViewer();
    const keys = curve.keys.slice().sort((x, y) => x.t - y.t);
    const guids = [...new Set(keys.map((k) => k.v.guid))];
    const status = el('div', { class: 'muted' }, el('span', { class: 'spinner' }), ` resolving ${guids.length} sprite(s)…`);
    body.append(status);
    const byGuid = new Map();
    await Promise.all(guids.map(async (g) => {
      try { byGuid.set(g, await api(`/api/resolve?ident=${g}&package=${a.package_id}`)); }
      catch (_) { try { byGuid.set(g, await api(`/api/resolve?ident=${g}`)); } catch (__) {} }
    }));
    const frames = keys.map((k) => ({ t: k.t, asset: byGuid.get(k.v.guid) || null, fileID: k.v.fileID, sub: k.v.fileID !== 21300000, rect: null }));
    const found = frames.filter((f) => f.asset).length;
    status.innerHTML = '';
    if (!found) { status.textContent = 'The sprites referenced by this clip are not in the library.'; return; }
    // Sub-sprites of a sheet: crop with the rects from the texture's .meta.
    const sheets = new Map();
    await Promise.all([...new Set(frames.filter((f) => f.sub && f.asset).map((f) => f.asset.id))].map(async (id) => {
      try { const r = await fetch(`/api/assets/${id}/meta`); if (r.ok) sheets.set(id, window.UaiViewer.parseSpriteSheet(await r.text())); } catch (_) {}
    }));
    let uncropped = 0;
    for (const f of frames) {
      if (!f.sub || !f.asset) continue;
      const sheet = sheets.get(f.asset.id);
      f.rect = (sheet && sheet.rects.get(f.fileID)) || null;
      if (!f.rect) uncropped++;
    }
    pv.innerHTML = '';
    pv.classList.remove('viewer');
    const canvas = el('canvas', { style: 'image-rendering: pixelated; max-width: 100%; max-height: 320px' });
    pv.append(canvas);
    const ctx = canvas.getContext('2d');
    const length = Math.max(parsed.length, keys[keys.length - 1].t + 1 / (parsed.sampleRate || 60));
    const images = new Map();
    const imageFor = (f) => new Promise((res) => {
      if (!f.asset) return res(null);
      if (images.has(f.asset.id)) return res(images.get(f.asset.id));
      const im = new Image();
      im.onload = () => { images.set(f.asset.id, im); res(im); };
      im.onerror = () => { images.set(f.asset.id, null); res(null); };
      im.src = `/api/assets/${f.asset.id}/raw`;
    });
    await Promise.all(frames.map(imageFor));
    // One canvas size for the whole clip so it does not jump between frames.
    let W = 1, H = 1;
    for (const f of frames) { const im = images.get(f.asset && f.asset.id); if (!im) continue; const r = f.rect || { width: im.width, height: im.height }; W = Math.max(W, r.width); H = Math.max(H, r.height); }
    canvas.width = W; canvas.height = H;
    const scale = Math.min(4, Math.floor(320 / H) || 1);
    canvas.style.width = W * scale + 'px'; canvas.style.height = H * scale + 'px';
    let i = -1, timer = null, playing = true;
    const show = (n) => {
      i = n;
      const f = frames[n];
      const im = images.get(f.asset && f.asset.id);
      ctx.clearRect(0, 0, W, H);
      if (im) {
        if (f.rect) ctx.drawImage(im, f.rect.x, im.height - f.rect.y - f.rect.height, f.rect.width, f.rect.height, Math.floor((W - f.rect.width) / 2), H - f.rect.height, f.rect.width, f.rect.height);
        else ctx.drawImage(im, Math.floor((W - im.width) / 2), H - im.height);
      }
      label.textContent = `frame ${n + 1}/${frames.length} · ${f.t.toFixed(2)}s`;
    };
    const step = () => {
      if (!playing) return;
      const n = (i + 1) % frames.length;
      show(n);
      const next = n + 1 < frames.length ? frames[n + 1].t : length;
      timer = setTimeout(step, Math.max(16, (next - frames[n].t) * 1000 / speed));
    };
    let speed = 1;
    const label = el('span', { class: 'muted' });
    const bar = el('div', { class: 'viewer-bar' },
      el('button', { class: 'ghost', type: 'button', text: '❚❚', onclick: (e) => { playing = !playing; e.target.textContent = playing ? '❚❚' : '▶'; if (playing) step(); else clearTimeout(timer); } }),
      el('button', { class: 'ghost', type: 'button', text: '⟨', title: 'previous frame', onclick: () => { playing = false; clearTimeout(timer); show((i - 1 + frames.length) % frames.length); } }),
      el('button', { class: 'ghost', type: 'button', text: '⟩', title: 'next frame', onclick: () => { playing = false; clearTimeout(timer); show((i + 1) % frames.length); } }),
      el('select', { onchange: (e) => { speed = +e.target.value; } }, ...['0.25', '0.5', '1', '2'].map((v) => el('option', { value: v, text: v + '×', selected: v === '1' }))),
      label);
    pv.after(bar);
    body.append(el('div', { class: 'muted', text: `${found} of ${frames.length} frames resolved · ${(frames.length / length).toFixed(1)} fps effective · ${W}×${H}px` + (uncropped ? ` · ${uncropped} sheet frame(s) without a rect are shown whole` : '') }));
    const ul = el('ul', { class: 'list-plain' });
    for (const g of guids) { const s = byGuid.get(g); if (s) ul.append(el('li', { onclick: () => openDetail(s.id), title: s.path }, el('span', { class: 'badge', text: s.kind }), ' ', s.name)); }
    body.append(ul);
    step();
  }

  async function renderText(a, body) {
    body.innerHTML = '<span class="spinner"></span>';
    try {
      const t = await api(`/api/text?ident=${encodeURIComponent('#' + a.id)}&max_bytes=400000`);
      body.innerHTML = '';
      if (t.truncated) body.append(el('div', { class: 'muted', text: 'showing the first 400 KB' }));
      body.append(el('pre', { class: 'code', text: t.text }));
    } catch (e) { body.textContent = e.message; }
  }

  // ----- basket -------------------------------------------------------------------------------------
  function saveBasket() { try { localStorage.setItem('uai.basket', JSON.stringify([...state.basket.values()].map(({ id, guid, path, name, kind, size, package: pkg, package_id }) => ({ id, guid, path, name, kind, size, package: pkg, package_id })))); } catch (_) {} }
  function toggleBasket(a) {
    if (state.basket.has(a.guid)) state.basket.delete(a.guid); else state.basket.set(a.guid, a);
    saveBasket();
    renderBasket();
    const c = document.querySelector(`.card[data-id="${a.id}"]`);
    if (c) c.replaceWith(card(a));
  }
  function renderBasket() {
    $('basketCount').textContent = state.basket.size;
    const list = $('basketList');
    list.innerHTML = '';
    for (const a of state.basket.values()) {
      list.append(el('li', {}, el('span', { class: 'badge', text: a.kind }), el('span', { class: 'p', text: a.path, title: a.path }), el('span', { class: 'muted', text: human(a.size) }),
        el('button', { type: 'button', text: '✕', 'aria-label': 'Remove', onclick: () => toggleBasket(a) })));
    }
    if (!state.basket.size) list.append(el('li', { class: 'muted', text: 'Empty. Use “+” on a card or “+ Basket” in the detail panel.' }));
    $('downloadBtn').disabled = $('planBtn').disabled = !state.basket.size;
  }
  function exportRequest() {
    return {
      identifiers: [...state.basket.values()].map((a) => '#' + a.id),
      include_deps: $('optDeps').checked, include_scripts: $('optScripts').checked, include_folders: $('optFolders').checked,
    };
  }
  async function showPlan() {
    const out = $('planOut');
    out.innerHTML = '<span class="spinner"></span>';
    try {
      const p = await postJson('/api/export/plan', exportRequest());
      out.innerHTML = '';
      out.append(el('div', {}, el('b', { text: `${p.planned} file(s), ${human(p.planned_bytes)}` }), ` from ${p.packages.join(', ')}`));
      if (p.warnings.length) { const ul = el('ul', { class: 'warn' }); for (const w of p.warnings) ul.append(el('li', { text: w })); out.append(ul); }
      if (p.unresolved.length) {
        const ul = el('ul');
        for (const u of p.unresolved.slice(0, 12)) ul.append(el('li', {}, el('span', { class: 'mono', text: u.guid.slice(0, 10) + '… ' }), u.label || 'not in library'));
        if (p.unresolved.length > 12) ul.append(el('li', { class: 'muted', text: `… ${p.unresolved.length - 12} more` }));
        out.append(el('div', { class: 'muted', text: 'Unresolved references (Unity built-ins / UPM packages / packs you do not own):' }), ul);
      }
      const cli = `uai export ${[...state.basket.values()].map((a) => a.guid).join(' ')}${$('optDeps').checked ? '' : ' --no-deps'}${$('optScripts').checked ? '' : ' --no-scripts'} --project <UnityProject>`;
      out.append(el('div', { class: 'muted', text: 'Same thing from the terminal:' }), el('pre', { class: 'code', text: cli }));
    } catch (e) { out.textContent = e.message; }
  }
  async function download() {
    const btn = $('downloadBtn');
    btn.disabled = true;
    const old = btn.textContent;
    btn.innerHTML = '<span class="spinner"></span> building…';
    try {
      const r = await fetch('/api/export/unitypackage', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(exportRequest()) });
      if (!r.ok) { let m = `HTTP ${r.status}`; try { m = (await r.json()).error || m; } catch (_) {} throw new Error(m); }
      const cd = r.headers.get('content-disposition') || '';
      const m = /filename="([^"]+)"/.exec(cd);
      const name = m ? m[1] : 'export.unitypackage';
      const blob = await r.blob();
      const url = URL.createObjectURL(blob);
      const a = el('a', { href: url, download: name });
      document.body.append(a); a.click(); a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 10000);
      const missing = Number(r.headers.get('x-uai-missing') || 0);
      toast(`${name} (${human(blob.size)}) — ${r.headers.get('x-uai-files') || '?'} files${missing ? `, ${missing} missing (index stale?)` : ''}`);
    } catch (e) { toast('Export failed: ' + e.message, true); }
    btn.disabled = false; btn.textContent = old;
  }

  // ----- wiring -------------------------------------------------------------------------------------
  $('searchForm').addEventListener('submit', (e) => { e.preventDefault(); state.q = $('q').value.trim(); state.browsing = null; search(); });
  $('ext').addEventListener('change', () => { state.ext = $('ext').value.trim().replace(/^\./, ''); search(); });
  $('packageSel').addEventListener('change', () => { state.package = $('packageSel').value; const p = state.packages.find((x) => String(x.id) === state.package); state.browsing = p && !state.q ? { pkg: p, prefix: 'Assets' } : null; search(); });
  $('publisherSel').addEventListener('change', () => { state.publisher = $('publisherSel').value; search(); });
  $('clearFilters').addEventListener('click', () => { state.kinds.clear(); state.package = ''; state.publisher = ''; state.ext = ''; state.q = ''; state.browsing = null; search(); });
  $('pkgFilter').addEventListener('input', renderPackageList);
  $('more').addEventListener('click', () => search(true));
  $('viewGrid').addEventListener('click', () => setView('grid'));
  $('viewList').addEventListener('click', () => setView('list'));
  $('basketBtn').addEventListener('click', () => { $('basket').classList.toggle('hidden'); renderBasket(); });
  $('basketClose').addEventListener('click', () => $('basket').classList.add('hidden'));
  $('basketClear').addEventListener('click', () => { state.basket.clear(); saveBasket(); renderBasket(); renderResults(); $('planOut').innerHTML = ''; });
  $('planBtn').addEventListener('click', showPlan);
  $('downloadBtn').addEventListener('click', download);
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') { if (!$('basket').classList.contains('hidden')) $('basket').classList.add('hidden'); else closeDetail(); }
    if (e.key === '/' && document.activeElement !== $('q') && !/INPUT|TEXTAREA|SELECT/.test(document.activeElement.tagName)) { e.preventDefault(); $('q').focus(); }
  });
  window.addEventListener('hashchange', () => { readHash(); search(); if (state.detailId) openDetail(state.detailId); });
  function setView(v) { state.view = v; try { localStorage.setItem('uai.view', v); } catch (_) {} $('viewGrid').classList.toggle('active', v === 'grid'); $('viewList').classList.toggle('active', v === 'list'); renderResults(); }

  (async () => {
    readHash();
    setView(state.view);
    renderBasket();
    try { await loadMeta(); } catch (e) { toast('Cannot reach the server: ' + e.message, true); }
    if (state.package && !state.q) { const p = state.packages.find((x) => String(x.id) === state.package); if (p) state.browsing = { pkg: p, prefix: 'Assets' }; }
    await search();
    if (state.detailId) openDetail(state.detailId);
  })();
})();
