# Social media and marketplace connectors: tool spec

Status: **partly built (2026-10-18)**: see §8. It covers X, TikTok, Instagram and Facebook (social), and
Amazon, eBay, Kleinanzeigen and willhaben (marketplaces). The focus is **research, scraping
and monitoring**. Posting, listing and replying are out of scope for now. Each tool below is a
candidate for a `/lab` run (see [Lab plan](#lab-plan)).

## 1. Design rules

These come from what already works in Sam (`monitors.rs`, `listings.rs`, `scrape.rs`,
`reddit.rs`, `leads.rs`, `linkedin.rs`).

1. **Few planner tools, many site adapters.** The planner sees one summary line per tool, so
   per-site tools (`x_search`, `tiktok_search`, …) would bloat the plan prompt and confuse tool
   choice. The tools are **job-shaped** (`social_research`, `creators_find`, `price_track`, …)
   and take a `site` argument. The site-specific code goes in `src/browser/<site>.rs`, in the
   same way as `linkedin.rs` and `reddit.rs`.
2. **Data source ladder, cheapest and most stable first:**
   official/public API → the site's own JSON as the signed-in page receives it (in-page
   `fetch` like `reddit.rs`, or CDP `Network.getResponseBody` on its XHR/GraphQL calls) →
   embedded page data (`__NEXT_DATA__`, JSON-LD, app state, see `listings.rs` plan "embed") →
   rule-based cards → LLM-written CSS recipe (`scrape.rs`). An LLM never reads raw pages in a
   loop.
3. **Constant token cost.** One plan call, then **one batch call** over compact cards
   (≤ 200 chars per item), then optionally one synthesis call. Relevance prefilters use
   embeddings or Laya (local, ~0 tokens). Learned recipes and plans are cached per
   (site, page shape).
4. **Sam's own session, paced and capped.** Signed-in sites use the "accounts" profile
   (Connectors › *Site* › Link). Every page view goes through a `view()` with a human-like
   pause and a daily cap, as `linkedin::view` does. Caps are per site and visible in the
   Connectors window. For Instagram, TikTok and X, recommend a **secondary account**, because
   automated reading can get accounts restricted.
   Instagram, X and TikTok have **hard daily caps** (section 1a).
5. **Results are stored, not just reported.** Monitors, campaigns and price series live in
   SQLite (`monitors.db`, `leads.db`, and a new `prices.db`). Later questions ("what changed
   since last week?") are answered from the store, not by re-scraping.
6. **Read-only.** Everything here has `effects: read`, except creating a monitor or tracker,
   which is `write` with `always_allow`, like `monitor_create`.

## 1a. Daily caps for Instagram, X and TikTok

These three restrict or ban accounts that read automatically. Every request to them goes
through one budget per site, `browser/<site>.rs::view()`, which generalises
`linkedin::count_view` (a usage file per day, `views_today()`). This holds for a page load or
a JSON/GraphQL call alike. When the budget is used up, the call returns an error. The tool then
reports partial results and says when it can continue; it never waits silently or switches
accounts.

| Limit (per site, per Sam account) | Instagram | X | TikTok |
|---|---|---|---|
| Requests per day (page loads + JSON calls) | **150** | **300** (search: see note) | **400** |
| Profile opens per day (subset) | 60 | 100 | 150 |
| Items read per day (posts/tweets/videos/comments) | 1,500 | 3,000 | 4,000 |
| Requests per hour (burst) | 30 | 60 | 80 |
| Pause between requests | 6–15 s | 4–10 s | 4–10 s |
| Active hours (local) | 8:00–22:00 | 7:00–23:00 | 8:00–23:00 |
| Monitors on this site: minimum interval | 6 h | 1 h | 3 h |

**Measured 2026-10-18 (lab L5):** X answered HTTP 429 after ~52 search requests in ~30 min, on a
normal account at 5–15 s pacing. Budget X *search* separately: **≤ 40 search requests per 30 min**,
at most ~150 per day, and spread one research run (~16–20 requests) over 10+ minutes.

Rules that go with the caps:
- **Tripwire.** Any challenge, checkpoint, "unusual activity", login wall or HTTP 429 pauses that
  site for **24 h**. A second one within 7 days pauses it for **7 days**. Both are shown in the
  Connectors window and in a notice. Sam never solves captchas or challenges itself.
- **Fair share.** Monitors may use at most 40 % of the day's budget, so a one-off "research this"
  still works in the evening. Long jobs (`creators_find` campaigns) run in slices spread over
  the day, like `leads.rs`.
- **Plan within the budget.** A tool estimates its requests before it starts and shrinks the
  plan (fewer queries, a shallower enrich) instead of failing halfway. The user sees
  "~120 of 300 X requests today".
- **Defaults are conservative and adjustable.** Environment variables or settings:
  `IG_DAILY`, `X_DAILY`, `TIKTOK_DAILY` (requests) plus the hourly ones. Raising them shows a
  warning in Connectors. Fresh accounts (< 30 days) start at half the budget for their first
  2 weeks.
- Facebook (groups, Marketplace) uses the Instagram limits, because it is the same company with
  the same detection. The official APIs (Ad Library, eBay Browse, Graph API Business Discovery)
  follow their own published rate limits instead.

## 2. Planner tools (new or extended)

| Tool | Sites | What it does | Status |
|---|---|---|---|
| `monitor_create` (extend) | willhaben, kleinanzeigen, ebay_at/de, amazon_de, **fb_marketplace**, **x**, **tiktok**, **instagram** | Watch a search or an account and alert on **new** items that match. Adds category, location/radius, "free", private/commercial seller, condition, and a **semantic match**. | extend |
| `monitor_results` | all monitors | Stored results since a time. | exists |
| `price_track` | amazon_de, ebay_at/de, willhaben, kleinanzeigen, any shop URL | Daily price/stock snapshots for given products or competitor listings. Alerts on drops, undercuts and stock-outs. Keeps a history. | new |
| `market_analysis` | ebay (sold), amazon, willhaben, kleinanzeigen | One-shot market picture for a query: price distribution, sold vs asking, supply, top sellers, deal threshold. | new |
| `social_research` | x, tiktok, instagram, facebook (groups/pages), + existing reddit | What people say about a topic: themes, stance split, quotes with links, volume over time, loudest accounts. | new (generalises `reddit_research`) |
| `social_trends` | tiktok (Creative Center), x, instagram, amazon (Movers & Shakers) | What is rising in a niche or region: hashtags, sounds, topics, products. | new |
| `creators_find` | instagram, tiktok, x, (youtube later) | Accounts that match filters (niche, followers, engagement, language/place, contact in bio), ranked with a reason and an outreach angle, stored as a campaign. | new (reuses the `leads.rs` pipeline) |
| `social_profile` | instagram, tiktok, x, facebook pages | One account in depth: stats, engagement rate, cadence, top posts, content pillars, contact. | new |
| `reviews_analyze` | amazon, (ebay seller feedback) | Review mining for a product or competitor: complaints, praised features, unmet needs. | new |

`scrape` and `details` stay as the generic fallback for any other page.

## 3. Per-site action catalogs

Legend: **P1** = build first, **P2** = next, **P3** = later. "Access" = how Sam reads it.

### 3.1 willhaben (AT)

Access: public pages, no login. Plan "embed" works (`__NEXT_DATA__` holds the search results).
Bot protection is mild, so pace at ≥ 15 min per monitor.

**Search parameters, checked against the live site on 2026-10-18.** They come from the
`navigatorGroups` in `__NEXT_DATA__` on `/iad/kaufen-und-verkaufen/marktplatz`:

| Filter | Parameter | Values |
|---|---|---|
| Search words | `keyword` | text |
| Newest first | `sort=1` | |
| **Only free items ("Nur zu verschenken")** | `PRICE_FROM=0&PRICE_TO=0` | the page's own "Zu verschenken" toggle and link set exactly this |
| Price range | `PRICE_FROM`, `PRICE_TO` | number |
| Bundesland | `areaId` | Burgenland 1, Kärnten 2, NÖ 3, OÖ 4, Salzburg 5, Steiermark 6, Tirol 7, Vorarlberg 8, **Wien 900**, other countries 22000 |
| Vienna district | `areaId` (repeatable) | 01. Innere Stadt 117223 … 23. Liesing 117245 (id = 117222 + district number) |
| Private / dealer | `ISPRIVATE` | 1 private, 0 dealer |
| Last 48 h only | `periode=2` | |
| Shipping with buyer protection | `paylivery=true` | |
| Category | path segment, e.g. `/marktplatz/mode-accessoires-3275` | from the `category` navigator |
| Condition, handover, brand, colour, size | path `/marktplatz/a/<attr>-<id>`, e.g. `zustand-gebraucht-23`, `uebergabe-selbstabholung-2536` | from the navigators (category-dependent) |

Each listing in `advertSummaryList.advertSummary[]` carries `ADID`, `HEADING`, `BODY_DYN`
(description), `PRICE` (0 when free) / `PRICE_FOR_DISPLAY` ("zu verschenken"), `STATE`,
`DISTRICT`, `POSTCODE`, `COORDINATES`, `ISPRIVATE`, `PUBLISHED_String`, `SEO_URL` and
`categorytreeids`, so a monitor needs no CSS recipe and no LLM to read it.

A live check of the trench-coat example shows why the semantic match matters.
`keyword=trenchcoat&areaId=900&PRICE_FROM=0&PRICE_TO=0` had **0 results**. The wider
`keyword=mantel` with the same filters had **30**, and they mix real coats ("Hugo Boss Mantel",
"Wollmantel 42/44") with noise ("Bademantel", "Garderobe", "Milchglastisch").

| # | Action | Example request | Tool | Output | Pri |
|---|---|---|---|---|---|
| W1 | New listing alert with filters | "Notify me when there's a new trench coat in *zu verschenken* in Vienna" | `monitor_create` | alert: title, price/free, district, photo, link | P1 |
| W2 | Free items in a category or area | "Anything free in furniture in 1070/1150?" | `monitor_create` / `scrape` | list | P1 |
| W3 | Deal finder: new listing under the market price | "Tell me if an iPhone 15 Pro shows up under 70 % of the usual price" | `monitor_create` + `market_analysis` baseline | alert with "−34 % vs median" | P2 |
| W4 | Asking-price stats | "What do people ask for a used Brompton?" | `market_analysis` | median, p10/p90, n, spread by condition | P1 |
| W5 | Price drop on a saved listing or search | "Tell me if that sofa gets cheaper" | `price_track` (listing URL) | old → new price | P2 |
| W6 | Competitor dealer watch (shops, car dealers) | "Track new cars and prices from dealer X" | `monitor_create` (seller URL) + `price_track` | new items, price changes | P2 |
| W7 | Flats (exists), cars (`/iad/gebrauchtwagen`), jobs | "BMW 3er under 15k in Wien" | `monitor_create` kind `car` | alert | P2 |
| W8 | Re-listing / bump detection | "Is this the same bike relisted cheaper?" | built into monitor diff ("bumped") | flag | P3 |

**Example W1, end to end.** The planner emits
`monitor_create{site: willhaben, kind: market, query: "trenchcoat", free: true, place: "Wien", match: "trench coat / Trenchcoat, any colour, women or men"}`.
Then:

1. `search_url` builds
   `/iad/kaufen-und-verkaufen/marktplatz?keyword=trenchcoat&areaId=900&PRICE_FROM=0&PRICE_TO=0&sort=1`
   (parameters confirmed above).
2. A keyword search alone misses "Mantel beige Burberry-Style" and lets through "Trenchcoat
   Kinderbuch". So:
   - **recall:** the monitor also runs a broader query (e.g. "mantel" in the free section,
     Vienna) and keeps candidates by embedding similarity to the `match` text;
   - **precision:** borderline cards (title + first 150 chars + category) go to a Laya
     micro-decision or one small batch LLM call per run. A photo check is optional (P3).
3. Only new and matching items alert. Plan and recipe are cached, so a run costs ~0 tokens
   unless the judge is needed.

### 3.2 Kleinanzeigen (DE)

Access: public, no login. Bot protection is stronger than willhaben's (captcha on bursts), so
pace ≥ 15 min, one tab, no parallel monitors on the same host.

| # | Action | Example request | Tool | Pri |
|---|---|---|---|---|
| K1 | New listing alert (category, place + radius, price, private only, "Zu verschenken", shipping) | "New Thinkpad X1 under 400 € within 30 km of Munich, private sellers only" | `monitor_create` | P1 |
| K2 | Free items (*Zu verschenken*) by place | "Free bike in Berlin-Neukölln" | `monitor_create` | P1 |
| K3 | Deal finder vs market median | as W3 | `monitor_create` + `market_analysis` | P2 |
| K4 | Asking-price stats | "What does a used Vitamix sell for?" | `market_analysis` | P1 |
| K5 | A seller's other listings | "What else does this seller have?" | `scrape` (seller page) | P3 |
| K6 | Price drop on a listing | as W5 | `price_track` | P2 |

### 3.3 eBay (ebay.de / ebay.at)

Access: the **official Browse API** (free developer key: search, item, seller) for active
listings is the preferred source, with stable JSON and no scraping. **Sold/completed prices**
are not in the free API, so they come from the web search with the sold and completed
filters. That page is public; pace it.

| # | Action | Example request | Tool | Pri |
|---|---|---|---|---|
| E1 | Market value from **sold** prices | "What does a Leica Q2 actually sell for?" | `market_analysis` (sold) | P1 |
| E2 | New listing alert (incl. Buy It Now only, condition, location, max price) | "New Fujifilm X100V listings under 1,100 €" | `monitor_create` | P1 |
| E3 | Auctions ending soon under a price | "Auctions for X ending in 2 h with bid < 300 €" | `monitor_create` kind `auction` | P2 |
| E4 | Competitor price tracking (seller or listings) | "Track the prices of my 5 competitors' listings daily" | `price_track` | P1 |
| E5 | Deal finder: BIN price < k × sold median | as W3 | `monitor_create` + E1 baseline | P2 |
| E6 | Seller watch: new items from a shop | "Tell me when shop X lists something new" | `monitor_create` (seller URL) | P2 |
| E7 | Sell-through rate / demand | "How fast do refurbished iPads sell?" | `market_analysis` (sold ÷ active, days on market) | P3 |

### 3.4 Amazon (amazon.de, also used for AT)

Access: public pages. Bot protection is strong (captcha, "dogs of Amazon" pages), so use a
low rate, one tab and backoff on captcha. Optional sources: **Keepa API** (paid, full price
history, cheapest reliable source for price tracking at scale) and **PA-API** (needs an
Associates account with sales). Design the adapter so either can replace scraping.

| # | Action | Example request | Tool | Pri |
|---|---|---|---|---|
| A1 | Competitor price tracking per ASIN: price, Buy Box seller, coupon, Prime, stock | "Check daily what my 10 competitors charge for their standing desks" | `price_track` | P1 |
| A2 | Alerts: price under X, undercut by competitor, back in stock, Buy Box lost | "Tell me when anyone undercuts my price" | `price_track` rules | P1 |
| A3 | Best Seller Rank and review-count tracking (proxy for sales velocity) | "Is competitor X growing?" | `price_track` (extra fields) | P2 |
| A4 | Category trends: Bestsellers, Movers & Shakers, New Releases | "What's rising in kitchen gadgets this week?" | `social_trends` site amazon | P2 |
| A5 | New competitor products for a keyword | "Tell me when new 'magnesium glycinate' products appear" | `monitor_create` (exists, sort by date) | P1 |
| A6 | Review mining: complaints, wishes, praised features | "What do people hate about the top 5 air fryers?" | `reviews_analyze` | P2 |
| A7 | Keyword rank of an ASIN in search | "Where does my product rank for 'yoga mat'?" | `price_track` (rank field) | P3 |

### 3.5 X (Twitter)

Access: **search requires a logged-in account**, so it runs in Sam's session (secondary account
recommended). The page loads tweets through GraphQL (`SearchTimeline`, `UserTweets`,
`UserByScreenName`). Read those responses with CDP, not the DOM, so there are no class names
to break. Daily read caps for unverified accounts are tight, so budget about 300–800 tweets per
research run. The official API (paid tiers) is an optional swap-in.

| # | Action | Example request | Tool | Output | Pri |
|---|---|---|---|---|---|
| X1 | **What people say about a topic** | "What are people saying about Trump on X this week?" | `social_research` | themes, stance split with %, quotes + links, volume per day, top accounts, caveats | P1 |
| X2 | Same, with filters | "…only German-language, ≥ 50 likes, no replies" | `social_research` args: `lang`, `min_likes`, `since/until`, `exclude_replies` (X search operators) | | P1 |
| X3 | Brand / competitor mention monitor | "Alert me when someone complains about [my product] on X" | `monitor_create` site `x` + stance/complaint judge | alert with tweet | P2 |
| X4 | Account watch: new posts from a list of accounts | "Tell me when @A, @B post about pricing" | `monitor_create` site `x` (accounts + match) | | P2 |
| X5 | Creator / expert discovery | "Find German fintech voices with 5–50k followers" | `creators_find` site `x` (People search + bio filter) | ranked table | P2 |
| X6 | Account analysis | "Analyse @X: topics, posting frequency, engagement" | `social_profile` | | P2 |
| X7 | Topic volume / trend over time | "Is interest in 'Mietpreisbremse' growing?" | `social_trends` site `x` (daily counts from sampled searches) | chart | P3 |

**Example X1, "what people say about Trump".** The aim is a fair picture at a fixed token cost:

1. **Plan (1 LLM call):** split the topic into 4–8 query angles that cover both sides and the
   current news hooks (`Trump tariffs`, `Trump lang:en min_faves:100`, …), plus a time window
   and a language. Each angle runs in both Top and Latest, so the sample isn't only viral posts.
2. **Collect (no LLM):** paced searches, read from GraphQL, deduplicated. Retweets and
   near-duplicates (embedding similarity > 0.95) are collapsed, so copy-paste campaigns don't
   count many times. Each card keeps text, likes, reposts, replies, author follower count and
   whether the account is new.
3. **Classify (1 batch call or local):** stance (pro / contra / neutral / unrelated) and topic
   label per tweet. Laya or a small model if it is accurate enough (a lab question).
4. **Cluster and synthesise (1 call):** the top themes per stance with 2–3 representative
   quotes each and links, the share by tweet count and by engagement (these differ a lot),
   volume per day, and the most-engaged accounts.
5. **Caveats in the report:** X users aren't representative of the public, bots and
   coordinated accounts are present, and the sample size is stated.

### 3.6 Instagram

Access: almost everything needs a login, so use Sam's session with a secondary account and
strict caps (the account risk is the highest of all these sites). The web app's JSON endpoints
(profile info, user feed, hashtag/location sections) can be read in-page like `reddit.rs` does.
Follower/following lists already work through `scrape` with the dialog URL. Meta's Graph API
(Business Discovery) is an official option when the user has a Business/Creator account linked
to a Facebook page: it gives stats for other business accounts by username, with no scraping.

| # | Action | Example request | Tool | Pri |
|---|---|---|---|---|
| I1 | **Creator discovery for outreach** with filters: niche, follower range, engagement rate, country/language, posting frequency, business/creator account, email or link in bio, not-already-contacted | "Find 50 Vienna food creators with 5–50k followers and > 3 % engagement who have an email in their bio" | `creators_find` | P1 |
| I2 | Seed sources for I1: hashtags, location pages, "similar accounts", followers of a competitor or of a known creator | "…who follow @competitor" | `creators_find` args `seeds` | P1 |
| I3 | Profile deep-dive: followers, avg likes/comments/views over the last 12 posts, engagement rate, cadence, content pillars, brand collabs (paid-partnership tags), contact | "Analyse @creator before we reach out" | `social_profile` | P1 |
| I4 | Competitor content analysis: top posts and formats, what performs | "What works for @competitor?" | `social_profile` + synthesis | P2 |
| I5 | Comment mining on a post or account | "What do people say under @brand's last posts?" | `social_research` site `instagram` (post URLs) | P2 |
| I6 | Account watch: new posts or reels from accounts | "Tell me when these 5 creators post" | `monitor_create` site `instagram` | P3 |
| I7 | Hashtag trend (post counts and top posts over time) | "Is #matcha growing in Vienna?" | `social_trends` | P3 |

### 3.7 TikTok

Access: much is visible without logging in (profiles, videos, search partly). The page holds
JSON (`__UNIVERSAL_DATA_FOR_REHYDRATION__`), and search/feeds come from XHR JSON. **TikTok
Creative Center** (public, no login) has the best trend data: top hashtags, songs, creators and
ads by country and industry. The official Research API is only for academics.

| # | Action | Example request | Tool | Pri |
|---|---|---|---|---|
| T1 | **Trends** by country and industry: hashtags, sounds, creators, rising vs top | "What's trending on TikTok in Austria in beauty this week?" | `social_trends` site `tiktok` (Creative Center / TikTok One: **needs a TikTok for Business login since 2026**) | P1 |
| T2 | Creator discovery (as I1): niche, followers, avg views, engagement, language/region, email in bio | "German fitness creators, 10–100k, avg views > 20k" | `creators_find` | P1 |
| T3 | Profile analysis: avg views, median views (viral outliers skew the mean), engagement, cadence, top videos | "Analyse @creator" | `social_profile` | P1 |
| T4 | Comment mining on videos for a topic or product | "What do people say about [product] on TikTok?" | `social_research` site `tiktok` | P2 |
| T5 | Top videos for a hashtag or keyword, with stats | "Best-performing videos for #mealprep this month" | `scrape` adapter / `social_trends` | P2 |
| T6 | Watch a hashtag or sound for new high-performing videos | "Tell me when a video with #ourbrand passes 50k views" | `monitor_create` site `tiktok` | P3 |

### 3.8 Facebook

Access: logged in for groups and Marketplace. The **Meta Ad Library** is public, and its API
covers *all* ads delivered in the EU (not only political ones). That makes it the cleanest
source for competitor ad research.

| # | Action | Example request | Tool | Pri |
|---|---|---|---|---|
| F1 | **Marketplace new listing alert** (place + radius, price, category, free) | "New free sofas on FB Marketplace in Vienna" | `monitor_create` site `fb_marketplace` | P1 |
| F2 | Competitor ads: active ads, creatives, copy, start dates, platforms (long-running ads are the winners) | "What ads is [competitor] running, and which have run longest?" | `social_research` site `fb_ads` (Ad Library) | P2 |
| F3 | What people say in groups the account is a member of | "What do people in 'Wien Expats' say about Kindergarten costs?" | `social_research` site `facebook` (groups) | P2 |
| F4 | Page analysis: posting cadence, engagement, top posts | "Analyse [brand]'s FB page" | `social_profile` | P3 |
| F5 | Watch a group for posts matching a need ("looking for a web designer") | "Tell me when someone in group X asks for a photographer" | `monitor_create` site `facebook` + match judge | P3 (lead source) |

## 4. Shared data shapes

Cards are compact, so they are cheap to put in a batch prompt. Full data stays in the store.

```text
Listing   {id, site, url, title, price, currency, free, location, posted, seller{id,name,kind}, condition, image}
PricePoint{product_id, site, ts, price, shipping, buybox_seller, in_stock, coupon, rank, reviews}
Post      {id, site, url, author{handle,followers,verified,created}, text, ts, likes, reposts, replies, views, lang}
Creator   {id, site, handle, name, bio, followers, posts, avg_views, median_views, eng_rate, cadence_per_wk,
           place, lang, category, email, links, last_post, score, reason, angle, status}
Trend     {site, region, kind (hashtag|sound|topic|product), label, rank, delta, volume, period, examples[]}
```

`creators_find` stores creators in `leads.db` (campaigns, dedupe, `lead_forget` work as they do
for LinkedIn). `price_track` writes `prices.db` (series per product).

## 5. Risk and compliance (short)

- Site ToS generally prohibit automated access. Sam reads what the user could open by hand, in
  the user's own session, at human pace and with caps, the same stance as LinkedIn today. Never
  run in parallel against one host, and don't evade captchas automatically. On a block, stop and
  tell the user.
- Creator outreach in the EU means processing personal data (GDPR). Store only what outreach
  needs (handle, public stats, public business email), keep a retention limit, and let
  `lead_forget` delete it. `/legal` can draft the notice.
- Public opinion reports state their sample and its limits (see X1).

## 6. Lab plan

Each tool gets a `/lab` with a golden set and fixed metrics. The order follows value ÷ effort:
the marketplace parts reuse the most existing code.

| Lab | Goal | Golden set | Metrics (target) | Optimises |
|---|---|---|---|---|
| L1 `market-monitors` | Filters (free, area, private, radius) + semantic match for W1/K1/E2/F1 | ~30 saved searches × 3 days of real results, hand-labelled match / no match | match recall ≥ 0.95, alert precision ≥ 0.9, tokens/run ≈ 0 for plan hits, false alerts/day | query widening, embedding threshold, judge on/off, Laya vs LLM |
| L2 `listing-parsers` | Deterministic parsers per marketplace | 20 saved result pages per site (+ 5 after markup changes) | field accuracy ≥ 0.98, plan relearn rate, ms/page | embed-map vs cards vs CSS per site |
| L3 `market-analysis` | Price stats + deal threshold | 40 queries with manual "fair price" | median error ≤ 10 %, outlier filter (accessories, broken, "suche") precision | outlier rules, title judge, sold vs asking weighting |
| L4 `price-track` | Amazon/eBay per-product snapshots | 50 ASINs / listings tracked 7 days, compared with Keepa or manual checks | price accuracy ≥ 0.99, captcha rate, cost/product/day | adapter (scrape vs API), pacing, fields |
| L5 `social-research-x` | X1/X2 | 15 topics; human-labelled stance for 300 tweets; reference report | stance accuracy ≥ 0.85, theme coverage judged vs reference, tokens/report ≤ ~15k, tweets read | query planning, dedupe, classifier (Laya / small model / batch), card length |
| L6 `creators-find` | I1/T2 | 10 briefs; hand-checked fit for top 30 each | precision@20 ≥ 0.8, filter accuracy (followers, eng. rate, email), page views per qualified creator | seed strategy, card scoring, enrich depth (same knobs as the leads lab) |
| L7 `social-trends` | T1/A4/X7 | Creative Center snapshots for 3 regions × 2 weeks | parse accuracy, "rising" stability, tokens | sources, delta definition |
| L8 `tool-routing` | Planner picks the right tool and args | ~150 user requests in DE/EN covering every action above, plus near misses ("research" vs "social_research", "monitor" vs "price_track") | tool accuracy ≥ 0.95, arg accuracy ≥ 0.9, plan tokens | tool summaries, `instructions.md`, PLAN_SYS rules |

Run L8 again after each new tool lands, because new summaries can steal requests from existing
tools.

**L1 result (2026-10-18, `lab/market-monitors/`).** 12 real saved searches (willhaben,
Kleinanzeigen) and 2,324 hand-labelled live listings; eBay blocked, still to do. Today's
monitors reach R 0.81 / P 0.76 on newer listings.

The winning pipeline:
1. One filter-aware keyword expansion at creation: 3–5 search URLs.
2. Code rules: postcode inside the city, "verkauft", "Suche…".
3. A per-monitor embedding gate learned from the first run's LLM verdicts.
4. A MiniMax Flash judge (prompt v6, the user's request verbatim) for the uncertain band.

It reaches **R 0.993 / P 0.966** (P 0.995 counting kids' items as acceptable), 7 false alerts
a day across 12 monitors, and ~7k tokens per monitor per day. No zero-token method passed F1
0.71. Details: `lab/market-monitors/experiment-summary.md`.

**L2–L6 results (2026-10-18, `lab/<name>/experiment-summary.md`):**

| Lab | Result | Changes for the build |
|---|---|---|
| L2 listing-parsers | Sam's parsers today: Amazon recall 0.38–0.79 on live pages; searches with 0–2 results cannot be monitored; willhaben empty pages alert on promoted ads. Fixed scripts: ≥ 0.97 on every field, all four sites. | `lab/listing-parsers/best/` (cards.js, itemkey.js diffs + a built-in willhaben plan) |
| L3 market-analysis | eBay sold + Flash judge + log-IQR: 3.6 % median error, 11/11 items within 10 % except one variant-policy case. It abstains correctly when the sold results contain no real item (Brompton: 120 of 120 are parts). | eBay sold prices need a login (done in Sam's accounts profile) |
| L4 price-track | Sam's `details`: Amazon price 18/20 but Buy Box seller 0/20; eBay price 0/13 (an LLM regex assumed "€" after the number). | JSON-LD Offer first, a code money parser, known-site extractors, no login hand-off in scheduled runs |
| L5 social-research-x | luna/Flash stance F1 0.77–0.81; pro/contra share within ~5 pp; full-sample synthesis covers 0.94 of reference themes. ~26–30k tokens per report. **X rate-limits at ~50 searches / 30 min.** | stance conventions in the prompt; ≤ 16 requests per run |
| L6 creators-find | TikTok **user** search: 0 % real fits for "German fitness"; **video** search + caption-language prefilter: 67 %. Median plays of real 10k–100k creators: 1–11k. | discovery via video search; show the views distribution instead of silently filtering |
| L7 trends | TikTok Creative Center moved to TikTok One: logged out it shows only the top 3 hashtags (US); the tiktok.com login does not carry over. | needs a free TikTok for Business login; X / Amazon trends untested |
| Instagram | HTTP 429 on the first API call after a fresh login; paused by the tripwire. | let a new session age with normal page views first |


## 7. Build order

1. **P1 marketplaces:** extend `monitor_create` (filters + semantic match, FB Marketplace),
   `market_analysis` (eBay sold + asking stats), `price_track` (Amazon/eBay). Labs L1–L4.
2. **P1 social research:** `social_research` for X (generalise `reddit_research`), lab L5.
3. **P1 creators:** `creators_find` + `social_profile` for Instagram and TikTok on the
   `leads.rs` pipeline, lab L6.
4. **P2:** `social_trends` (TikTok Creative Center, Amazon Movers & Shakers), `reviews_analyze`,
   FB Ad Library, X/Instagram account monitors. Labs L7, then L8 for the full set.


## 8. Build status (2026-10-18)

| Item | Status | Where |
|---|---|---|
| Listing parsers (L2): Amazon swatches, 0–2-result pages, one-result titles, set numbers as postcodes, "Zu verschenken" price | built | `server/src/browser/listings/{cards,itemkey}.js` |
| Built-in willhaben plan (no LLM learn, empty pages = 0 listings, promos ignored) | built | `browser/listings.rs::builtin_plan` |
| `monitor_create` (L1): filter-aware keyword expansion (3–5 searches), willhaben free / Vienna district / private, Kleinanzeigen free section + city, code rules (postcode, verkauft, Suche), judge v6 on new listings (rejected ones never alert), free items checked every ≤ 30 min | built | `monitors.rs`, `thinker.rs`, `tools/monitor_create` |
| Learned per-monitor gate (−33 % judge calls) | not yet | |
| Routing (L8): 5 core tools always shown + 6 scored; revised summaries; 2 planner rules | built | `tools.rs::select_with_core`, `thinker.rs` |
| Sessions for eBay / X / Instagram / TikTok: Connectors link, daily + 30-min budgets, 429 tripwire (24 h / 7 d) | built | `browser/sessions.rs`, `connectors.rs` |
| `market_analysis` (L3): eBay sold + judge + log-IQR, abstain < 8 sales, asking prices | built | `market.rs`, `tools/market_analysis` |
| `social_research`: X (L5), TikTok comments under the most-watched videos, Meta Ad Library (`facebook_ads`, no login) | built; TikTok + Ad Library tested live; X pending its 24 h pause | `social.rs`, `social_sites.rs` |
| `social_profile`: TikTok, X (UserByScreenName / UserTweets from a profile visit), Instagram (profile page data, og:description fallback) | built; TikTok tested; X / Instagram pending their pauses | `creators.rs`, `social_sites.rs` |
| `creators_find`: TikTok (video search), X (people search), Instagram (hashtag pages, ≤ 10 profiles per run) | built; TikTok tested | `creators.rs`, `social_sites.rs` |
| `social_trends`: TikTok hashtags / sounds by recent plays in a niche (normal search pages, no TikTok for Business), X trending tab, Amazon Movers & Shakers → New Releases → Bestsellers per category | built; TikTok + Amazon tested | `social_sites.rs`, `tools/social_trends` |
| Facebook: session (Connectors › Facebook), Marketplace monitors (`fb_marketplace`) | built; needs the user's Facebook sign-in | `sessions.rs`, `monitors.rs` |
| **Datasets** (long-running collections, daily new rows + enrichment, `.xlsx` with data + changes sheets, `.csv`, `.md`): sources scrape (marketplaces through the listings reader, other pages through scrape + detail fields), creators (TikTok / X / Instagram + profile refresh), social (posts + daily stance row), market (daily price row per item), monitor (a monitor / price tracker as a table) | built; scrape, creators, market, monitor tested | `datasets.rs`, `tools/dataset_create`, `dataset_results`, `dataset_update` |
| `reviews_analyze`, Instagram comments research | not yet | |
