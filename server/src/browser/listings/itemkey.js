// Which links are listings? Every same-site link with an id-like token (6+ digits, a 20+ hex
// id, an ASIN) votes for each plain path segment it contains ("itm", "expose", "s-anzeige",
// "d", "dp"); links without an id count against a segment. The segment linking the most
// distinct ids wins → "/<segment>/". No LLM, no per-site code.
(() => {
  const ID = /(?:^|[\/-])([A-Z0-9]{10}|\d{6,}|[0-9a-f]{20,})(?=[\/-]|$)/;
  const votes = {}, against = {};
  const host = location.host.split('.').slice(-2).join('.');
  for (const a of document.querySelectorAll('a[href]')) {
    let u; try { u = new URL(a.getAttribute('href'), location.href); } catch (_) { continue; }
    if (!u.host.endsWith(host)) continue;
    const segs = u.pathname.split('/').filter(Boolean);
    const m = u.pathname.match(ID);
    const plain = segs.filter(s => s.length <= 24 && !/\d{3,}/.test(s) && !ID.test('/' + s));
    for (const s of new Set(plain)) {
      if (m) { (votes[s] = votes[s] || new Set()).add(m[1]); } else { against[s] = (against[s] || 0) + 1; }
    }
  }
  const ranked = Object.entries(votes).map(([s, ids]) => [s, ids.size - (against[s] || 0) * 0.5, ids.size]).sort((a, b) => b[1] - a[1]);
  // Known marketplaces: their listing path is fixed, so 0-2 results are a valid (empty or tiny)
  // result, not "no listings found" (rare-item monitors: a free trench coat in Vienna, lab L2).
  const KNOWN = {'willhaben.at': 'd', 'kleinanzeigen.de': 's-anzeige', 'ebay.de': 'itm', 'ebay.at': 'itm', 'ebay.com': 'itm', 'amazon.de': 'dp', 'amazon.com': 'dp', 'facebook.com': 'item'};
  const known = KNOWN[(window.__samHost || host)];
  if (known && !(ranked.length && ranked[0][2] >= 3 && ranked[0][0] !== known && ranked[0][2] > ((votes[known] || new Set()).size) * 3))
    return {item: '/' + known + '/', ids: (votes[known] || new Set()).size, known: true};
  return ranked.length && ranked[0][2] >= 3 ? {item: '/' + ranked[0][0] + '/', ids: ranked[0][2], runner_up: ranked.slice(1, 3)} : null;
})()
