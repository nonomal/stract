// Stract is an open source web search engine.
// Copyright (C) 2023 Stract ApS
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::RwLockReadGuard;

use itertools::Itertools;
use url::Url;

use crate::collector::approx_count;
use crate::config::{CollectorConfig, SnippetConfig};
use crate::index::Index;
use crate::inverted_index::{InvertedIndex, KeyPhrase, RetrievedWebpage};
use crate::models::dual_encoder::DualEncoder;
use crate::query::Query;
use crate::ranking::models::linear::LinearRegression;
use crate::ranking::pipeline::{
    LocalRecallRankingWebpage, PrecisionRankingWebpage, RankableWebpage, RecallRankingWebpage,
};
use crate::ranking::{LocalRanker, SignalComputer, SignalEnum, SignalScore};
use crate::search_ctx::Ctx;
use crate::search_prettifier::DisplayedWebpage;
use crate::{inverted_index, live_index, Result};

use super::WebsitesResult;
use super::{InitialWebsiteResult, SearchQuery};

pub trait SearchableIndex {
    type SearchGuard<'a>: SearchGuard<'a>
    where
        Self: 'a;

    fn guard(&self) -> impl Future<Output = Self::SearchGuard<'_>>;
    fn set_snippet_config(&mut self, config: SnippetConfig) -> impl Future<Output = ()>;
}

pub trait SearchGuard<'a> {
    fn search_index(&self) -> &'_ Index;
    fn inverted_index(&self) -> &'_ InvertedIndex {
        &self.search_index().inverted_index
    }
}

impl SearchableIndex for Index {
    type SearchGuard<'a> = NormalIndexSearchGuard<'a>;

    async fn guard(&self) -> Self::SearchGuard<'_> {
        NormalIndexSearchGuard { search_index: self }
    }

    async fn set_snippet_config(&mut self, config: SnippetConfig) {
        self.inverted_index.set_snippet_config(config);
    }
}

pub struct NormalIndexSearchGuard<'a> {
    search_index: &'a Index,
}

impl<'a> SearchGuard<'a> for NormalIndexSearchGuard<'a> {
    fn search_index(&self) -> &'_ Index {
        self.search_index
    }
}

impl SearchableIndex for Arc<live_index::LiveIndex> {
    type SearchGuard<'a> = LiveIndexSearchGuard<'a>;

    async fn guard(&self) -> Self::SearchGuard<'_> {
        LiveIndexSearchGuard {
            lock_guard: self.read().await,
        }
    }

    async fn set_snippet_config(&mut self, config: SnippetConfig) {
        live_index::LiveIndex::set_snippet_config(self, config).await
    }
}

pub struct LiveIndexSearchGuard<'a> {
    lock_guard: RwLockReadGuard<'a, live_index::index::InnerIndex>,
}

impl<'a> SearchGuard<'a> for LiveIndexSearchGuard<'a> {
    fn search_index(&self) -> &'_ Index {
        self.lock_guard.index()
    }
}

pub struct LocalSearcher<I: SearchableIndex> {
    index: I,
    linear_regression: Option<Arc<LinearRegression>>,
    dual_encoder: Option<Arc<DualEncoder>>,
    collector_config: CollectorConfig,
}

impl<I> From<I> for LocalSearcher<I>
where
    I: SearchableIndex,
{
    fn from(index: I) -> Self {
        Self::new(index)
    }
}

struct InvertedIndexResult {
    webpages: Vec<LocalRecallRankingWebpage>,
    num_hits: approx_count::Count,
}

impl<I> LocalSearcher<I>
where
    I: SearchableIndex,
{
    pub fn new(index: I) -> Self {
        LocalSearcher {
            index,
            linear_regression: None,
            dual_encoder: None,
            collector_config: CollectorConfig::default(),
        }
    }

    pub fn set_linear_model(&mut self, model: LinearRegression) {
        self.linear_regression = Some(Arc::new(model));
    }

    pub fn set_dual_encoder(&mut self, dual_encoder: DualEncoder) {
        self.dual_encoder = Some(Arc::new(dual_encoder));
    }

    pub fn set_collector_config(&mut self, config: CollectorConfig) {
        self.collector_config = config;
    }

    pub async fn set_snippet_config(&mut self, config: SnippetConfig) {
        self.index.set_snippet_config(config).await;
    }

    fn parse_query<'a, G: SearchGuard<'a>>(
        &'a self,
        ctx: &Ctx,
        guard: &G,
        query: &SearchQuery,
    ) -> Result<Query> {
        Query::parse(ctx, query, guard.inverted_index())
    }

    fn ranker<'a, G: SearchGuard<'a>>(
        &'a self,
        query: &Query,
        guard: &G,
        de_rank_similar: bool,
        computer: SignalComputer,
    ) -> Result<LocalRanker> {
        let mut ranker = LocalRanker::new(
            computer,
            guard.inverted_index().columnfield_reader(),
            self.collector_config.clone(),
        );

        ranker.de_rank_similar(de_rank_similar);

        Ok(ranker
            .with_max_docs(
                self.collector_config.max_docs_considered,
                guard.inverted_index().num_segments(),
            )
            .with_num_results(query.num_results())
            .with_offset(query.offset()))
    }

    fn search_inverted_index<'a, G: SearchGuard<'a>>(
        &'a self,
        ctx: &Ctx,
        guard: &G,
        query: &SearchQuery,
        de_rank_similar: bool,
    ) -> Result<InvertedIndexResult> {
        let parsed_query = self.parse_query(ctx, guard, query)?;

        let mut computer = SignalComputer::new(Some(&parsed_query));

        computer.set_region_count(
            guard
                .search_index()
                .region_count
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        );

        if let Some(model) = self.linear_regression.as_ref() {
            computer.set_linear_model(model.clone());
        }

        let ranker = self.ranker(&parsed_query, guard, de_rank_similar, computer)?;

        let res = guard.inverted_index().search_initial(
            &parsed_query,
            ctx,
            ranker.collector(ctx.clone()),
        )?;

        let columnfield_reader = guard.inverted_index().columnfield_reader();

        let ranking_websites = guard.inverted_index().retrieve_ranking_websites(
            ctx,
            res.top_websites,
            ranker.computer(),
            &columnfield_reader,
        )?;

        Ok(InvertedIndexResult {
            webpages: ranking_websites,
            num_hits: res.num_websites,
        })
    }

    pub fn index(&self) -> &I {
        &self.index
    }

    pub async fn search_initial(
        &self,
        query: &SearchQuery,
        de_rank_similar: bool,
    ) -> Result<InitialWebsiteResult> {
        let guard = self.index.guard().await;
        let ctx = guard.inverted_index().local_search_ctx();
        let inverted_index_result =
            self.search_inverted_index(&ctx, &guard, query, de_rank_similar)?;

        Ok(InitialWebsiteResult {
            websites: inverted_index_result.webpages,
            num_websites: inverted_index_result.num_hits,
        })
    }

    pub async fn retrieve_websites(
        &self,
        websites: &[inverted_index::WebpagePointer],
        query: &str,
    ) -> Result<Vec<inverted_index::RetrievedWebpage>> {
        let guard = self.index.guard().await;
        let ctx = guard.inverted_index().local_search_ctx();
        let query = SearchQuery {
            query: query.to_string(),
            ..Default::default()
        };
        let query = Query::parse(&ctx, &query, guard.inverted_index())?;

        guard.inverted_index().retrieve_websites(websites, &query)
    }

    pub async fn search(&self, query: &SearchQuery) -> Result<WebsitesResult> {
        use std::time::Instant;

        let start = Instant::now();
        let search_query = query.clone();

        let search_result = self.search_initial(&search_query, true).await?;

        let pointers: Vec<_> = search_result
            .websites
            .iter()
            .map(|website| website.pointer().clone())
            .collect();

        let websites: Vec<_> = self
            .retrieve_websites(&pointers, &query.query)
            .await?
            .into_iter()
            .zip_eq(search_result.websites)
            .map(|(webpage, ranking)| {
                let ranking = RecallRankingWebpage::new(ranking, Default::default());
                PrecisionRankingWebpage::new(webpage, ranking)
            })
            .collect();

        let pointers: Vec<_> = websites
            .iter()
            .map(|website| website.ranking().pointer().clone())
            .collect();

        let retrieved_sites = self
            .retrieve_websites(&pointers, &search_query.query)
            .await?;

        let coefficients = query.signal_coefficients();

        let mut webpages: Vec<_> = retrieved_sites
            .into_iter()
            .map(|webpage| DisplayedWebpage::new(webpage, query))
            .collect();

        for (webpage, ranking) in webpages.iter_mut().zip(websites) {
            let mut ranking_signals = HashMap::new();

            for signal in SignalEnum::all() {
                if let Some(calc) = ranking.ranking().signals().get(signal) {
                    ranking_signals.insert(
                        signal.into(),
                        SignalScore {
                            value: calc.score,
                            coefficient: coefficients.get(&signal),
                        },
                    );
                }
            }

            webpage.ranking_signals = Some(ranking_signals);
        }

        Ok(WebsitesResult {
            num_hits: search_result.num_websites,
            webpages,
            search_duration_ms: start.elapsed().as_millis(),
            has_more_results: (search_result.num_websites.as_u64() as usize)
                > query.offset() + query.num_results(),
        })
    }

    /// This function is mainly used for tests and benchmarks
    pub fn search_sync(&self, query: &SearchQuery) -> Result<WebsitesResult> {
        crate::block_on(self.search(query))
    }

    pub async fn get_webpage(&self, url: &str) -> Option<RetrievedWebpage> {
        self.index.guard().await.inverted_index().get_webpage(url)
    }

    pub async fn get_homepage(&self, url: &Url) -> Option<RetrievedWebpage> {
        self.index.guard().await.inverted_index().get_homepage(url)
    }

    pub async fn top_key_phrases(&self, top_n: usize) -> Vec<KeyPhrase> {
        self.index
            .guard()
            .await
            .inverted_index()
            .top_key_phrases(top_n)
    }

    pub async fn get_site_urls(&self, site: &str, offset: usize, limit: usize) -> Vec<Url> {
        self.index
            .guard()
            .await
            .inverted_index()
            .get_site_urls(site, offset, limit)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        searcher::NUM_RESULTS_PER_PAGE,
        webpage::{Html, Webpage},
    };

    use super::*;

    #[test]
    fn offset_page() {
        const NUM_PAGES: usize = 50;
        const NUM_WEBSITES: usize = NUM_PAGES * NUM_RESULTS_PER_PAGE;

        let (mut index, _dir) = Index::temporary().expect("Unable to open index");

        for i in 0..NUM_WEBSITES {
            index
                .insert(&Webpage {
                    html: Html::parse(
                        r#"
            <html>
                <head>
                    <title>Example website</title>
                </head>
                <body>
                    test
                </body>
            </html>
            "#,
                        &format!("https://www.{i}.com"),
                    )
                    .unwrap(),
                    host_centrality: (NUM_WEBSITES - i) as f64,
                    fetch_time_ms: 500,
                    ..Default::default()
                })
                .expect("failed to insert webpage");
        }

        index.commit().unwrap();

        let searcher = LocalSearcher::new(index);

        for p in 0..NUM_PAGES {
            let urls: Vec<_> = searcher
                .search_sync(&SearchQuery {
                    query: "test".to_string(),
                    page: p,
                    ..Default::default()
                })
                .unwrap()
                .webpages
                .into_iter()
                .map(|page| page.url)
                .collect();

            assert!(!urls.is_empty());

            for (i, url) in urls.into_iter().enumerate() {
                assert_eq!(
                    url,
                    format!("https://www.{}.com/", i + (p * NUM_RESULTS_PER_PAGE))
                )
            }
        }
    }
}
