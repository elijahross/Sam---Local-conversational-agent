Give the filters as fields; Sam builds the search URLs for the known sites (newest first, filters applied), adds a few synonym searches and lets a checker decide which new listings really are what the user asked for:
- things for sale: site willhaben (Austria) / kleinanzeigen (Germany) / ebay_at / ebay_de / amazon_de, kind market, query = the item in the user's words (no place, price or 'gratis' in the query), place = city / Vienna district, free = true for 'zu verschenken' / 'gratis' / 'gives away', private = true for 'private sellers only', min_price / max_price
- Facebook Marketplace: site fb_marketplace, kind market, query, place = the city, free / max_price (needs Sam's Facebook sign-in: Connectors › Facebook)
- flats: site willhaben or immoscout_at (Austria), immoscout_de (Germany), kind rent or buy, place = the city, max_price
- LinkedIn people (new leads): site other + url https://www.linkedin.com/search/results/people/?keywords=<role+industry+place> (checks at most every 6 h)
- any other site: site other + url (the site's own search URL with the filters).
Several sites → one monitor_create step per site. Free items go fast: check every 15-30 min. Say how often it checks; the user can change it.
