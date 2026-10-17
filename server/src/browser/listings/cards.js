// Generic listing cards: every link to an item (path contains `item`, id token in the URL)
// climbs to the largest ancestor that still links to only that one item = its card. Each card
// also gets rule-based fields (no LLM): title = the longest text linking to the item (or the
// card's heading), price = the first amount with a currency, location = a postcode + place or
// "aus …", posted = a date / time phrase.
((item) => {
  const site = location.host.split('.').slice(-2).join('.');
  const idOf = (href) => {
    let u; try { u = new URL(href, location.href); } catch (_) { return null; }
    // Same site only: ad redirects (other hosts) can carry a listing path in their URL.
    if (!u.host.endsWith(site) || !u.pathname.includes(item)) return null;
    const p = u.pathname;
    // Amazon-style /dp/<ASIN>: the token right after /dp/ (slugs may hold model numbers).
    const dp = p.match(/\/dp\/([A-Z0-9]{10})(?:[\/?]|$)/);
    if (dp) return dp[1];
    const m = p.match(/(?:^|[\/-])([A-Z0-9]{10}|\d{6,}|[0-9a-f]{20,})(?=[\/-]|$)/g);
    if (m) return m[m.length - 1].replace(/^[\/-]/, '');
    const s = p.split(item)[1] || ''; const seg = s.split('/').filter(Boolean)[0];
    return seg ? decodeURIComponent(seg) : null;
  };
  const byId = new Map();
  for (const a of document.querySelectorAll('a[href]')) {
    const id = idOf(a.getAttribute('href'));
    if (!id) continue;
    if (!byId.has(id)) byId.set(id, []);
    byId.get(id).push(a);
  }
  const idsIn = (el) => { const s = new Set(); for (const a of el.querySelectorAll('a[href]')) { const i = idOf(a.getAttribute('href')); if (i) s.add(i); if (s.size > 1) break; } return s; };
  const out = [];
  for (const [id, as] of byId) {
    let c = as[0];
    while (c.parentElement && c.parentElement !== document.body && idsIn(c.parentElement).size <= 1) c = c.parentElement;
    const lines = (c.innerText || '').split('\n').map(x => x.trim()).filter(Boolean);
    const text = lines.join(' | ');
    if (text.length < 8) continue;
    const href = new URL(as[0].getAttribute('href'), location.href);
    const clean = (t) => (t || '').replace(/\s+/g, ' ').replace(/^(neues angebot|new listing|neu|top|gesponsert|sponsored)\s*/i, '').replace(/wird in neuem fenster oder tab geöffnet|opens in a new window or tab/ig, '').trim();
    const linkText = as.map(a => clean(a.innerText || a.getAttribute('aria-label') || a.getAttribute('title'))).filter(t => t.length >= 6 && !/^[\d\s.,€$]+$/.test(t)).sort((a, b) => b.length - a.length)[0];
    const heading = clean((c.querySelector('h1,h2,h3,h4,[role=heading]') || {}).innerText);
    const title = (heading && heading.length >= 6 ? heading : linkText) || '';
    const money = /(?:(?:€|EUR|\$|£|CHF)\s?\d[\d.,']*(?:,-)?|\d[\d.,']*\s?(?:€|EUR|CHF)(?:\s?VB)?)/;
    const priceLine = lines.find(l => money.test(l) && !/versand|shipping|lieferung|delivery|\/m²|\/unit|uvp|statt/i.test(l)) || lines.find(l => money.test(l)) || '';
    const price = (priceLine.match(money) || [''])[0].trim();
    const loc = lines.map(l => l.match(/\b\d{4,5}\s+[A-ZÄÖÜ][\wÄÖÜäöüß.\- ]{2,40}/) || l.match(/^aus\s+(.{3,40})$/i)).find(Boolean);
    const posted = lines.find(l => /^(heute|gestern|today|yesterday)\b|^\d{1,2}\.\s?\d{1,2}\.\s?\d{2,4}|^\d{1,2}\.\s?[A-Za-zä]{3,}\.?\s\d{1,2}:\d{2}|vor \d+ (min|std|stunden|tagen)/i.test(l)) || '';
    out.push({id, url: href.origin + href.pathname, text: text.slice(0, 500), title: title.slice(0, 200), price, location: loc ? (loc[1] || loc[0]).trim() : '', posted});
  }
  return out;
})
