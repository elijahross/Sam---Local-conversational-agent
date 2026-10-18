// Generic listing cards: every link to an item (path contains `item`, id token in the URL)
// climbs to the largest ancestor that still links to only that one item = its card. Each card
// also gets rule-based fields (no LLM): title = the longest text linking to the item (or the
// card's heading), price = the first amount with a currency, location = a postcode + place or
// "aus …", posted = a date / time phrase.
((item) => {
  const site = window.__samHost || location.host.split('.').slice(-2).join('.');
  const idOf = (href) => {
    let u; try { u = new URL(href, location.href); } catch (_) { return null; }
    // Same site only: ad redirects (other hosts) can carry a listing path in their URL.
    if ((u.host && !u.host.endsWith(site)) || !u.pathname.includes(item)) return null;  // empty host: saved page replay (lab)
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
  // Fallback climb for cards that came out empty: colour / size swatches link to other product
  // ids (Amazon) and stop the climb at the image box. Then only title-like links (text, or in a
  // heading) count as other items, and the climb starts at this item's title link (lab L2).
  const isTitle = (a) => { const t = (a.innerText || '').trim(); return (t.length >= 6 && !/^[\d\s.,€$%()+-]+$/.test(t)) || !!a.closest('h1,h2,h3,h4,[role=heading]'); };
  const titledIn = (el) => { const s = new Set(); for (const a of el.querySelectorAll('a[href]')) { if (!isTitle(a)) continue; const i = idOf(a.getAttribute('href')); if (i) s.add(i); if (s.size > 1) break; } return s; };
  const out = [];
  for (const [id, as] of byId) {
    let c = as[0];
    // A page with one result: every ancestor links to only that item, so stop before the card
    // swallows the page (its heading would become the title: "Es wurden keine Ergebnisse …").
    const small = (el) => (el.innerText || '').length <= 2000;
    while (c.parentElement && c.parentElement !== document.body && idsIn(c.parentElement).size <= 1 && small(c.parentElement)) c = c.parentElement;
    if ((c.innerText || '').trim().length < 8) {
      const t = as.find(isTitle);
      if (!t) continue;
      c = t;
      while (c.parentElement && c.parentElement !== document.body && titledIn(c.parentElement).size <= 1 && small(c.parentElement)) c = c.parentElement;
    }
    const lines = (c.innerText || '').split('\n').map(x => x.trim()).filter(Boolean);
    const text = lines.join(' | ');
    if (text.length < 8) continue;
    const href = new URL(as[0].getAttribute('href'), location.href);
    const clean = (t) => (t || '').replace(/\s+/g, ' ').replace(/^(neues angebot|new listing|neu|top|gesponsert|sponsored)\s*/i, '').replace(/wird in neuem fenster oder tab geöffnet|opens in a new window or tab/ig, '').trim();
    const linkText = as.map(a => clean(a.innerText || a.getAttribute('aria-label') || a.getAttribute('title'))).filter(t => t.length >= 6 && !/^[\d\s.,€$]+$/.test(t)).sort((a, b) => b.length - a.length)[0];
    // A heading is the title only if it belongs to this item's link (not a section heading such
    // as "Weitere Ergebnisse in anderen Orten" inside a one-result card).
    const hEl = [...c.querySelectorAll('h1,h2,h3,h4,[role=heading]')].find(h => as.some(a => h.contains(a) || a.contains(h)));
    const heading = clean((hEl || {}).innerText);
    const title = (heading && heading.length >= 6 ? heading : linkText) || '';
    const money = /(?:(?:€|EUR|\$|£|CHF)\s?\d[\d.,']*(?:,-)?|\d[\d.,']*\s?(?:€|EUR|CHF)(?:\s?VB)?)/;
    const priceLine = lines.find(l => money.test(l) && !/versand|shipping|lieferung|delivery|\/m²|\/unit|uvp|statt/i.test(l)) || lines.find(l => money.test(l)) || '';
    const free = lines.find(l => /^(zu verschenken|gratis|kostenlos|free)$/i.test(l));
    const price = (priceLine.match(money) || [''])[0].trim() || (free ? 'Zu verschenken' : '');
    // "aus <Land>" first; a postcode + place never from the title line (set / model numbers:
    // "42143 Ferrari Daytona", lab L2).
    const tl = (title || '').slice(0, 40);
    const loc = lines.map(l => l.match(/^aus\s+(.{3,40})$/i)).find(Boolean)
      || lines.filter(l => !tl || !l.includes(tl.slice(0, 20))).map(l => l.match(/\b\d{4,5}\s+[A-ZÄÖÜ][\wÄÖÜäöüß.\- ]{2,40}/)).find(Boolean);
    const posted = lines.find(l => /^(heute|gestern|today|yesterday)\b|^\d{1,2}\.\s?\d{1,2}\.\s?\d{2,4}|^\d{1,2}\.\s?[A-Za-zä]{3,}\.?\s\d{1,2}:\d{2}|vor \d+ (min|std|stunden|tagen)/i.test(l)) || '';
    out.push({id, url: href.origin + href.pathname, text: text.slice(0, 500), title: title.slice(0, 200), price, location: loc ? (loc[1] || loc[0]).trim() : '', posted});
  }
  return out;
})
