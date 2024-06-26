# Stract Crawler

Stract is an [open source](https://github.com/StractOrg/stract/) web search engine. StractBot is the name of our crawler that collects pages from the web in order to build the index.
It is written in Rust and the source code can be found [here](https://github.com/StractOrg/stract/tree/main/crates/core/src/crawler).
The crawler uses the user agent `Mozilla/5.0 (compatible; StractBot/0.2; open source search engine; +https://stract.com/webmasters)`.

## Politeness

StractBot is a polite crawler. It respects the [robots.txt](https://en.wikipedia.org/wiki/Robots.txt) file of the website it is crawling and tries to not overload the server.
It does this by waiting a certain amount of time between requests to the same domain. The waiting time is calculated by _min(politeness \* max(fetchtime, 5 sec), 60 sec)_ where _fetchtime_ is the time it took to fetch the page. The crawler will wait at least 5 sec between requests and at most 60 seconds.
This dynamic waiting time tries to prevent us from disrupting servers that cannot handle the load, while not giving unnecessary politeness to servers that can. The politeness factor starts at 1 and is doubled every time the crawler gets a 429 response from the server (to at most 2048).

The crawler looks for the token `StractBot` in the robots.txt file to determine which pages (if any) the crawler is allowed to crawl.
The robots.txt file is cached for 1 hour, so changes to the file should be respected quite quickly.

## Contact us

If you have any concerns or bad experiences with our crawler, please don't hesitate to reach out to us at [crawler@stract.com](mailto:crawler@stract.com). Chances are that others experience the same problems and we would love to fix them.
