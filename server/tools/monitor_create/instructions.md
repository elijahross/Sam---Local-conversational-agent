Give the filters as fields; Sam builds the search URL for the known sites (newest first, price bounds applied):
- things for sale: site willhaben (Austria) / kleinanzeigen (Germany) / ebay_at / ebay_de / amazon_de, kind market, query = the search words, min_price / max_price
- flats: site willhaben or immoscout_at (Austria), immoscout_de (Germany), kind rent or buy, place = the city, max_price
- LinkedIn people (new leads): site other + url https://www.linkedin.com/search/results/people/?keywords=<role+industry+place> (checks at most every 6 h)
- any other site: site other + url (the site's own search URL with the filters).
Several sites → one monitor_create step per site. Say how often it checks; the user can change it.
