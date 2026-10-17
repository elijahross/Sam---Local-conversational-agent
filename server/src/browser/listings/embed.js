// Embedded data: parse JSON in <script> tags (JSON blocks, JSON-LD, `x = {...}` state
// assignments) — never executes page code — and find the array of objects that holds the
// listings (the one whose elements mention the most distinct item ids seen in the page's links).
// (ids, null) → {path, count, matched, samples} | null
// (null, {path, map, url_prefix}) → items mapped by the cached field paths.
((ids, apply) => {
  const roots = {}; let ld = 0, sc = 0;
  for (const s of document.scripts) {
    const t = s.textContent.trim();
    if (t.length < 200) continue;
    if (/json/i.test(s.type)) {
      let v = null; try { v = JSON.parse(t); } catch (_) {}
      if (v) roots[s.id ? '#' + s.id : /ld\+json/.test(s.type) ? 'ld+json[' + (ld++) + ']' : 'script[' + (sc++) + ']'] = v;
      continue;
    }
    // App state: one script may assign several (`window.A = {…}\nwindow.B = {…}`); each part on its own.
    for (const part of t.split(/\n(?=(?:window\.|var |let |const )?[\w.$]+\s*=)/)) {
      const m = part.match(/^\s*(?:window\.|var |let |const )?([\w.$]+)\s*=\s*([\[{][\s\S]*[\]}])\s*;?\s*$/);
      if (!m) continue;
      let v = null; try { v = JSON.parse(m[2].replace(/\bundefined\b/g, 'null')); } catch (_) {}
      if (v) roots[s.id ? '#' + s.id + ':' + m[1] : m[1]] = v;
    }
  }
  // Paths: dot keys, numeric indexes, and [k=v] to pick from lists of {name, value} pairs.
  const get = (o, path) => {
    for (const part of (path || '').match(/[^.\[\]]+|\[[^\]]*\]/g) || []) {
      if (o == null) return null;
      if (part.startsWith('[')) { const [k, v] = part.slice(1, -1).split('='); o = Array.isArray(o) ? o.find(x => x && String(x[k]) === v) : null; }
      else o = o[part];
    }
    return o;
  };
  const rootOf = (path) => { const k = Object.keys(roots).find(k => path === k || path.startsWith(k + '.')); return k ? get(roots[k], path.slice(k.length + 1)) : null; };
  if (apply) {
    const arr = rootOf(apply.path);
    if (!Array.isArray(arr)) return [];
    const val = (el, p) => { if (!p) return ''; const v = get(el, p); return v == null ? '' : Array.isArray(v) ? String(v[0]) : typeof v === 'object' ? JSON.stringify(v).slice(0, 200) : String(v); };
    return arr.map(el => { const o = {}; for (const [f, p] of Object.entries(apply.map)) o[f] = val(el, p);
      if (o.url && apply.url_prefix && !/^https?:/.test(o.url)) o.url = apply.url_prefix.replace(/\/$/, '') + '/' + o.url.replace(/^\//, '');
      return o; });
  }
  const idset = new Set(ids);
  let best = null;
  const walk = (o, path, d) => {
    if (d > 12 || !o || typeof o !== 'object') return;
    if (Array.isArray(o)) {
      if (o.length >= 3 && o.slice(0, 3).every(x => x && typeof x === 'object')) {
        // Distinct ids: a list of links all about ONE listing must not win.
        const hit = new Set();
        for (const el of o) { const s = JSON.stringify(el); for (const id of idset) if (s.includes(id)) { hit.add(id); break; } }
        const matched = hit.size, rich = JSON.stringify(o[0]).length;
        // Ties (JSON-LD summary vs the app's own data): more elements, then richer ones win.
        if (matched >= 3 && (!best || matched > best.matched || (matched === best.matched && (o.length > best.count || (o.length === best.count && rich > best.rich))))) best = {path, count: o.length, matched, rich, arr: o};
      }
      o.slice(0, 2).forEach((x, i) => walk(x, path + '.' + i, d + 1));
      return;
    }
    for (const k of Object.keys(o)) walk(o[k], path + '.' + k, d + 1);
  };
  for (const [k, v] of Object.entries(roots)) walk(v, k, 0);
  if (!best) return null;
  const prune = (o, d) => {
    if (typeof o === 'string') return o.length > 90 ? o.slice(0, 90) + '…' : o;
    if (!o || typeof o !== 'object') return o;
    if (d > 4) return Array.isArray(o) ? '[…]' : '{…}';
    if (Array.isArray(o)) return o.slice(0, 40).map(x => prune(x, d + 1));
    const r = {}; for (const [k, v] of Object.entries(o)) { if (/image|picture|thumbnail|logo|srcset|tracking|dmp/i.test(k)) continue; r[k] = prune(v, d + 1); } return r;
  };
  return {path: best.path, count: best.count, matched: best.matched, origin: location.origin, samples: [prune(best.arr[0], 0), prune(best.arr[Math.floor(best.arr.length / 2)], 0)]};
})
