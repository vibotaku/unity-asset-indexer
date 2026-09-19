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
    $('stats').textContent = `${stats.packages} packages · ${stats.assets.toLocaleString()} assets · ${human(stats.bytes)}` + (stats.library_mounted ? '' : ' · library offline');
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
      const li = el('li', { class: String(p.id) === state.package ? 'active' : '', title: p.rel_path, onclick: () => selectPackage(p) },
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
    state.detailId = null;
    writeHash();
    $('detail').classList.add('hidden');
    document.querySelector('.layout').classList.remove('has-detail');
    document.querySelectorAll('.card.selected').forEach((c) => c.classList.remove('selected'));
  }

  function renderDetail(info) {
    const a = info;
    const panel = $('detail');
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
