/* 3D preview for the detail panel: FBX / OBJ / glTF models, animations embedded in FBX files, and
   Unity .anim clips retargeted onto a model by bone name. Loaded as an ES module (see the import map
   in index.html); app.js talks to it through window.UaiViewer. */
import * as THREE from 'three';
import { FBXLoader } from 'three/addons/loaders/FBXLoader.js';
import { OBJLoader } from 'three/addons/loaders/OBJLoader.js';
import { GLTFLoader } from 'three/addons/loaders/GLTFLoader.js';
import { OrbitControls } from 'three/addons/controls/OrbitControls.js';

function createViewer(container, { textureUrl } = {}) {
  const renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true });
  renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
  renderer.outputColorSpace = THREE.SRGBColorSpace;
  container.append(renderer.domElement);
  const scene = new THREE.Scene();
  const camera = new THREE.PerspectiveCamera(40, 1, 0.01, 10000);
  const controls = new OrbitControls(camera, renderer.domElement);
  controls.enableDamping = true;
  scene.add(new THREE.HemisphereLight(0xffffff, 0x334455, 1.6));
  const sun = new THREE.DirectionalLight(0xffffff, 2.0);
  sun.position.set(3, 6, 4);
  scene.add(sun);
  const grid = new THREE.GridHelper(10, 20, 0x8899aa, 0x445566);
  grid.material.transparent = true;
  grid.material.opacity = 0.5;
  scene.add(grid);

  let model = null, mixer = null, action = null, clips = [];
  const clock = new THREE.Clock();
  let running = true, paused = false;
  const resize = () => {
    const w = container.clientWidth || 300, h = container.clientHeight || 300;
    renderer.setSize(w, h, false);
    camera.aspect = w / h;
    camera.updateProjectionMatrix();
  };
  const ro = new ResizeObserver(resize);
  ro.observe(container);
  resize();
  (function loop() {
    if (!running) return;
    requestAnimationFrame(loop);
    const dt = clock.getDelta();
    if (mixer && !paused) mixer.update(dt);
    controls.update();
    renderer.render(scene, camera);
  })();

  function frame(obj) {
    const box = new THREE.Box3().setFromObject(obj);
    if (box.isEmpty()) return;
    const size = box.getSize(new THREE.Vector3());
    const center = box.getCenter(new THREE.Vector3());
    const maxDim = Math.max(size.x, size.y, size.z) || 1;
    grid.scale.setScalar(maxDim / 5);
    grid.position.set(center.x, box.min.y, center.z);
    camera.near = maxDim / 200;
    camera.far = maxDim * 200;
    camera.updateProjectionMatrix();
    camera.position.set(center.x + maxDim * 0.9, center.y + maxDim * 0.5, center.z + maxDim * 1.7);
    controls.target.copy(center);
    controls.update();
  }

  const manager = new THREE.LoadingManager();
  // A texture that failed to load leaves the material sampling an empty image (renders black): drop it.
  const fixMaterials = () => {
    if (!model) return;
    model.traverse((o) => {
      if (!o.isMesh) return;
      for (const m of [].concat(o.material)) {
        if (m && m.map && !(m.map.image && (m.map.image.width || m.map.image.videoWidth))) { m.map = null; m.needsUpdate = true; }
      }
    });
  };
  manager.onLoad = fixMaterials;
  manager.onError = () => setTimeout(fixMaterials, 0);
  if (textureUrl) {
    manager.setURLModifier((url) => {
      if (/^(blob:|data:)/.test(url) || /\/api\/assets\/\d+\/raw(\?|$)/.test(url)) return url; // the model itself
      const base = decodeURIComponent(url.split(/[\\/]/).pop() || '');
      return (base && textureUrl(base)) || url;
    });
  }

  function clear() {
    if (action) action.stop();
    action = null; mixer = null; clips = [];
    if (model) {
      scene.remove(model);
      model.traverse((o) => { if (o.geometry) o.geometry.dispose(); });
      model = null;
    }
  }

  async function load(url, ext) {
    clear();
    let obj, anims = [];
    ext = (ext || '').toLowerCase();
    if (ext === 'fbx') {
      obj = await new FBXLoader(manager).loadAsync(url);
      anims = obj.animations || [];
    } else if (ext === 'obj') {
      obj = await new OBJLoader(manager).loadAsync(url);
    } else {
      const g = await new GLTFLoader(manager).loadAsync(url);
      obj = g.scene;
      anims = g.animations || [];
    }
    // Materials that failed to get their textures still render; make sure nothing is invisible-black.
    obj.traverse((o) => {
      if (o.isMesh) {
        o.frustumCulled = false;
        const mats = Array.isArray(o.material) ? o.material : [o.material];
        // A black diffuse colour multiplies any texture to black; lift it (white keeps the texture as is).
        for (const m of mats) if (m && m.color && m.color.getHex() === 0) m.color.setHex(m.map ? 0xffffff : 0x9aa4b2);
      }
    });
    model = obj;
    scene.add(obj);
    frame(obj);
    setTimeout(fixMaterials, 6000);
    clips = anims.filter((c) => c.duration > 0);
    mixer = new THREE.AnimationMixer(obj);
    if (clips.length) play(0);
    let bones = 0;
    obj.traverse((o) => { if (o.isBone) bones++; });
    return { clips: clips.map((c) => ({ name: c.name, duration: c.duration })), bones };
  }

  function play(i) {
    if (!mixer || !clips[i]) return;
    if (action) action.stop();
    action = mixer.clipAction(clips[i]);
    action.reset().play();
    paused = false;
  }
  function playClip(clip) {
    if (!model) return;
    if (action) action.stop();
    mixer = new THREE.AnimationMixer(model);
    clips = [clip];
    action = mixer.clipAction(clip);
    action.reset().play();
    paused = false;
  }
  function setPaused(p) { paused = p; }
  function setSpeed(s) { if (mixer) mixer.timeScale = s; }
  function dispose() {
    running = false;
    ro.disconnect();
    clear();
    controls.dispose();
    renderer.dispose();
    renderer.domElement.remove();
  }
  return { load, play, playClip, setPaused, setSpeed, dispose, frame: () => model && frame(model), get model() { return model; }, get clips() { return clips; } };
}

/* ----- Unity .anim -> THREE.AnimationClip -------------------------------------------------------
   Unity stores generic clips as per-bone curves keyed by hierarchy path ("Armature/Hips/Spine").
   Bones are matched by the path's last segment. Unity is left-handed and its FBX importer mirrors
   the X axis, so rotations/positions are mirrored back ("axis" lets the user pick another fix if a
   file was authored differently). Keys are sampled at their times; tangents are ignored. */
const AXIS_FIX = {
  x: { q: (x, y, z, w) => [x, -y, -z, w], p: (x, y, z) => [-x, y, z] },
  z: { q: (x, y, z, w) => [-x, -y, z, w], p: (x, y, z) => [x, y, -z] },
  none: { q: (x, y, z, w) => [x, y, z, w], p: (x, y, z) => [x, y, z] },
};

function clipFromUnityAnim(parsed, root, { axis = 'x', name = 'clip' } = {}) {
  const fix = AXIS_FIX[axis] || AXIS_FIX.x;
  // FBX files often contain several copies of a skeleton (one per skinned mesh); drive every node
  // that carries the name so whichever copy the visible meshes bind to gets the motion.
  const byName = new Map();
  root.traverse((o) => { if (o.name) { if (!byName.has(o.name)) byName.set(o.name, []); byName.get(o.name).push(o); } });
  const nodesFor = (path) => {
    if (!path) return [root];
    const parts = path.split('/');
    return byName.get(parts[parts.length - 1]) || [];
  };
  const tracks = [];
  const matched = new Set(), missing = new Set();
  const e = new THREE.Euler(), q = new THREE.Quaternion();
  for (const c of parsed.euler) {
    const nodes = nodesFor(c.path);
    if (!nodes.length || !c.keys.length) { missing.add(c.path); continue; }
    matched.add(c.path);
    const times = [], values = [];
    for (const k of c.keys) {
      // Unity applies Euler angles in Z, X, Y order (q = Ry * Rx * Rz), which is three's 'YXZ'.
      e.set(THREE.MathUtils.degToRad(k.v.x), THREE.MathUtils.degToRad(k.v.y), THREE.MathUtils.degToRad(k.v.z), 'YXZ');
      q.setFromEuler(e);
      times.push(k.t);
      values.push(...fix.q(q.x, q.y, q.z, q.w));
    }
    for (const node of nodes) tracks.push(new THREE.QuaternionKeyframeTrack(`${node.uuid}.quaternion`, times, values));
  }
  for (const c of parsed.rotation) {
    const nodes = nodesFor(c.path);
    if (!nodes.length || !c.keys.length) { missing.add(c.path); continue; }
    matched.add(c.path);
    const times = [], values = [];
    for (const k of c.keys) { times.push(k.t); values.push(...fix.q(k.v.x, k.v.y, k.v.z, k.v.w)); }
    for (const node of nodes) tracks.push(new THREE.QuaternionKeyframeTrack(`${node.uuid}.quaternion`, times, values));
  }
  for (const c of parsed.position) {
    const nodes = nodesFor(c.path);
    if (!nodes.length || !c.keys.length) { missing.add(c.path); continue; }
    matched.add(c.path);
    for (const node of nodes) {
      // Unity positions are metres; the loaded model may be in centimetres. Scale by the rest pose.
      const first = c.keys[0].v;
      const restLen = node.position.length();
      const keyLen = Math.hypot(first.x, first.y, first.z);
      const s = restLen > 1e-6 && keyLen > 1e-6 ? restLen / keyLen : 1;
      const times = [], values = [];
      for (const k of c.keys) { times.push(k.t); values.push(...fix.p(k.v.x * s, k.v.y * s, k.v.z * s)); }
      tracks.push(new THREE.VectorKeyframeTrack(`${node.uuid}.position`, times, values));
    }
  }
  for (const c of parsed.scale) {
    const nodes = nodesFor(c.path);
    if (!nodes.length || !c.keys.length) { missing.add(c.path); continue; }
    matched.add(c.path);
    const times = [], values = [];
    for (const k of c.keys) { times.push(k.t); values.push(k.v.x, k.v.y, k.v.z); }
    for (const node of nodes) tracks.push(new THREE.VectorKeyframeTrack(`${node.uuid}.scale`, times, values));
  }
  const duration = parsed.stopTime || Math.max(0, ...tracks.map((t) => t.times[t.times.length - 1] || 0));
  const clip = new THREE.AnimationClip(name, duration, tracks);
  return { clip, matched: [...matched], missing: [...missing] };
}

/* Minimal line-based parser for Unity's AnimationClip YAML (enough for previews). */
function parseUnityAnim(text) {
  const out = { sampleRate: 60, stopTime: null, wrapMode: 0, legacy: false, euler: [], rotation: [], position: [], scale: [], floats: [], pptr: [] };
  const sectionOf = { m_EulerCurves: 'euler', m_RotationCurves: 'rotation', m_PositionCurves: 'position', m_ScaleCurves: 'scale', m_FloatCurves: 'floats', m_PPtrCurves: 'pptr' };
  let section = null, cur = null, inKeys = false, key = null;
  const vec = (s) => {
    const o = {};
    for (const m of s.matchAll(/([xyzw]):\s*([-\d.eE+]+)/g)) o[m[1]] = +m[2];
    return o;
  };
  const field = (k, v) => {
    if (!key) return;
    if (k === 'time') key.t = +v;
    else if (k === 'value') key.v = v.trim().startsWith('{') ? vec(v) : { x: +v };
  };
  for (const raw of text.split('\n')) {
    const line = raw.replace(/\r$/, '');
    let m;
    if ((m = /^\s+m_SampleRate:\s*([-\d.eE+]+)/.exec(line))) out.sampleRate = +m[1];
    else if ((m = /^\s+m_StopTime:\s*([-\d.eE+]+)/.exec(line))) out.stopTime = +m[1];
    else if ((m = /^  m_WrapMode:\s*(\d+)/.exec(line))) out.wrapMode = +m[1];
    else if ((m = /^  m_Legacy:\s*(\d)/.exec(line))) out.legacy = m[1] === '1';
    if ((m = /^  (m_\w+):/.exec(line))) { section = sectionOf[m[1]] || null; cur = null; inKeys = false; key = null; continue; }
    if (!section) continue;
    if (/^  - curve:/.test(line)) { cur = { path: '', attribute: '', keys: [] }; out[section].push(cur); inKeys = section === 'pptr'; key = null; continue; }
    if (!cur) continue;
    if (section === 'pptr') {
      // PPtr (sprite) curves: "    - time: 0" / "      value: {fileID: 21300000, guid: ..., type: 3}"
      if ((m = /^    - time:\s*([-\d.eE+]+)/.exec(line))) { key = { t: +m[1], v: null }; cur.keys.push(key); continue; }
      if (key && (m = /^      value:\s*\{fileID:\s*(-?\d+),\s*guid:\s*([0-9a-fA-F]{32})/.exec(line))) { key.v = { fileID: +m[1], guid: m[2].toLowerCase() }; continue; }
    }
    if ((m = /^    path:\s*(.*)$/.exec(line))) { cur.path = m[1].trim(); continue; }
    if ((m = /^    attribute:\s*(.*)$/.exec(line))) { cur.attribute = m[1].trim(); continue; }
    if (/^      m_Curve:/.test(line)) { inKeys = true; key = null; continue; }
    if (!inKeys) continue;
    if ((m = /^      - (\w+):\s*(.*)$/.exec(line))) { key = { t: 0, v: null }; cur.keys.push(key); field(m[1], m[2]); continue; }
    if ((m = /^        (\w+):\s*(.*)$/.exec(line))) { field(m[1], m[2]); continue; }
  }
  for (const k of ['euler', 'rotation', 'position', 'scale', 'floats', 'pptr']) for (const c of out[k]) c.keys = c.keys.filter((x) => x.v);
  const muscle = out.floats.filter((f) => !f.path && /^(RootT|RootQ|LeftFootT|LeftFootQ|RightFootT|RightFootQ|Spine |Chest |UpperChest |Neck |Head |Left |Right |Jaw )/.test(f.attribute)).length;
  out.humanoid = muscle >= 8;
  out.muscleCurves = muscle;
  const last = (cs) => Math.max(0, ...cs.flatMap((c) => c.keys.map((k) => k.t)));
  out.length = out.stopTime || Math.max(last(out.euler), last(out.rotation), last(out.position), last(out.scale), last(out.floats), last(out.pptr) + 1 / (out.sampleRate || 60));
  return out;
}

/* TextureImporter .meta -> { internalID -> {x, y, width, height} } for sprite sheets (rect origin is bottom-left). */
function parseSpriteSheet(metaText) {
  const rects = new Map();
  const list = [];
  let cur = null, inRect = false, inSprites = false;
  for (const raw of metaText.split('\n')) {
    const line = raw.replace(/\r$/, '');
    let m;
    if (/^\s*sprites:/.test(line)) { inSprites = true; continue; }
    if (inSprites && /^\s{4}\w+:/.test(line) && !/^\s{4}sprites:/.test(line)) { inSprites = false; }
    if (!inSprites) continue;
    if (/^\s*- /.test(line) && /^\s{4}- /.test(line)) { cur = { name: '', id: null, rect: null }; list.push(cur); inRect = false; }
    if (!cur) continue;
    if ((m = /^\s+name:\s*(.*)$/.exec(line)) && !inRect) cur.name = m[1].trim();
    else if (/^\s+rect:/.test(line)) { inRect = true; cur.rect = {}; }
    else if (inRect && (m = /^\s+(x|y|width|height):\s*([-\d.]+)/.exec(line))) { cur.rect[m[1]] = +m[2]; if (m[1] === 'height') inRect = false; }
    else if ((m = /^\s+internalID:\s*(-?\d+)/.exec(line))) cur.id = +m[1];
  }
  for (const s of list) if (s.rect && s.id !== null) rects.set(s.id, s.rect);
  return { rects, sprites: list };
}

window.UaiViewer = { createViewer, clipFromUnityAnim, parseUnityAnim, parseSpriteSheet };
