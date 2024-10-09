// Stract is an open source web search engine.
// Copyright (C) 2024 Stract ApS
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
// along with this program.  If not, see <https://www.gnu.org/licenses/>

use std::{collections::HashMap, sync::Arc};

use bloom::combine_u64s;
use min_max_heap::MinMaxHeap;

use tantivy::{
    collector::{Collector, ScoreSegmentTweaker, ScoreTweaker, SegmentCollector},
    DocId, SegmentOrdinal, SegmentReader,
};

use crate::{
    config::CollectorConfig,
    inverted_index::{DocAddress, WebpagePointer},
    numericalfield_reader,
    prehashed::Prehashed,
    ranking::initial::{InitialScoreTweaker, Score},
    schema::{numerical_field, NumericalFieldEnum},
    simhash,
};

use super::{Doc, Hashes, MainCollector, MaxDocsConsidered};

pub struct TopDocs {
    top_n: usize,
    offset: usize,
    max_docs: Option<MaxDocsConsidered>,
    columnfield_reader: numericalfield_reader::NumericalFieldReader,
    de_rank_similar: bool,
    collector_config: CollectorConfig,
}

impl TopDocs {
    pub fn with_limit(
        top_n: usize,
        columnfield_reader: numericalfield_reader::NumericalFieldReader,
    ) -> Self {
        Self {
            top_n,
            offset: 0,
            max_docs: None,
            de_rank_similar: false,
            columnfield_reader,
            collector_config: CollectorConfig::default(),
        }
    }

    pub fn max_docs(&self) -> Option<&MaxDocsConsidered> {
        self.max_docs.as_ref()
    }

    pub fn and_offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }

    pub fn and_max_docs(mut self, max_docs: MaxDocsConsidered) -> Self {
        self.max_docs = Some(max_docs);
        self
    }

    pub fn and_de_rank_similar(mut self) -> Self {
        self.de_rank_similar = true;
        self
    }

    pub fn and_collector_config(mut self, collector_config: CollectorConfig) -> Self {
        self.collector_config = collector_config;
        self
    }

    pub fn main_collector(self, score_tweaker: InitialScoreTweaker) -> MainCollector {
        MainCollector::new(score_tweaker, self)
    }
}

impl TopDocs {
    fn for_segment(
        &self,
        segment_local_id: tantivy::SegmentOrdinal,
        segment: &tantivy::SegmentReader,
    ) -> tantivy::Result<TopSegmentCollector> {
        let max_docs = self
            .max_docs
            .as_ref()
            .map(|max_docs| max_docs.total_docs / max_docs.segments);

        Ok(TopSegmentCollector {
            columnfield_segment_reader: self.columnfield_reader.get_segment(&segment.segment_id()),
            max_docs,
            num_docs_taken: 0,
            segment_ord: segment_local_id,
            bucket_collector: BucketCollector::new(
                self.top_n + self.offset,
                self.collector_config.clone(),
            ),
        })
    }
}

pub struct TopSegmentCollector {
    columnfield_segment_reader: Arc<numericalfield_reader::SegmentReader>,
    max_docs: Option<usize>,
    num_docs_taken: usize,
    segment_ord: SegmentOrdinal,
    bucket_collector: BucketCollector<SegmentDoc>,
}

impl TopSegmentCollector {
    fn get_hash(
        &self,
        doc: DocId,
        field1: NumericalFieldEnum,
        field2: NumericalFieldEnum,
    ) -> Prehashed {
        let field_reader = self.columnfield_segment_reader.get_field_reader(doc);

        let hash = [
            field_reader.get(field1).unwrap().as_u64().unwrap(),
            field_reader.get(field2).unwrap().as_u64().unwrap(),
        ];
        combine_u64s(hash).into()
    }
}

impl TopSegmentCollector {
    fn is_done(&self) -> bool {
        if let Some(max_docs) = &self.max_docs {
            self.num_docs_taken >= *max_docs
        } else {
            false
        }
    }

    fn collect(&mut self, doc: DocId, score: Score) {
        if self.is_done() {
            return;
        }

        self.num_docs_taken += 1;

        let simhash: Option<u64> = self
            .columnfield_segment_reader
            .get_field_reader(doc)
            .get(numerical_field::SimHash.into())
            .unwrap()
            .into();

        self.bucket_collector.insert(SegmentDoc {
            hashes: Hashes {
                site: self.get_hash(
                    doc,
                    numerical_field::SiteHash1.into(),
                    numerical_field::SiteHash2.into(),
                ),
                title: self.get_hash(
                    doc,
                    numerical_field::TitleHash1.into(),
                    numerical_field::TitleHash2.into(),
                ),
                url: self.get_hash(
                    doc,
                    numerical_field::UrlHash1.into(),
                    numerical_field::UrlHash2.into(),
                ),
                url_without_tld: self.get_hash(
                    doc,
                    numerical_field::UrlWithoutTldHash1.into(),
                    numerical_field::UrlWithoutTldHash2.into(),
                ),
                simhash: simhash.unwrap(),
            },
            id: doc,
            segment: self.segment_ord,
            score,
        });
    }

    fn harvest(self) -> Vec<SegmentDoc> {
        self.bucket_collector.into_sorted_vec(true)
    }
}

#[derive(Debug, Clone)]
struct ScoredDoc<T: Doc> {
    doc: T,
    adjusted_score: f64,
}

impl<T: Doc> PartialOrd for ScoredDoc<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: Doc> PartialEq for ScoredDoc<T> {
    fn eq(&self, other: &Self) -> bool {
        self.adjusted_score == other.adjusted_score
    }
}

impl<T: Doc> Ord for ScoredDoc<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.adjusted_score.total_cmp(&other.adjusted_score)
    }
}

impl<T: Doc> Eq for ScoredDoc<T> {}

impl<T: Doc> From<T> for ScoredDoc<T> {
    fn from(doc: T) -> Self {
        Self {
            adjusted_score: doc.score(),
            doc,
        }
    }
}

struct BucketCount {
    config: CollectorConfig,
    buckets: HashMap<Prehashed, usize>,
}

impl BucketCount {
    fn new(config: CollectorConfig) -> Self {
        Self {
            config,
            buckets: HashMap::new(),
        }
    }

    pub fn adjust_score<T: Doc>(&self, doc: &mut ScoredDoc<T>) {
        let hashes = doc.doc.hashes();

        let taken_sites = *self.buckets.get(&hashes.site).unwrap_or(&0);
        let taken_urls = *self.buckets.get(&hashes.url).unwrap_or(&0);
        let taken_urls_without_tld = *self.buckets.get(&hashes.url_without_tld).unwrap_or(&0);
        let taken_titles = *self.buckets.get(&hashes.title).unwrap_or(&0);

        let adjuster = 1.0
            / (1.0
                + taken_sites as f64 * self.config.site_penalty
                + taken_urls as f64 * self.config.url_penalty
                + taken_urls_without_tld as f64 * self.config.url_without_tld_penalty
                + taken_titles as f64 * self.config.title_penalty);

        doc.adjusted_score = doc.doc.score() * adjuster;
    }

    fn update_counts<T: Doc>(&mut self, doc: &ScoredDoc<T>) {
        let hashes = doc.doc.hashes();

        *self.buckets.entry(hashes.site).or_default() += 1;
        *self.buckets.entry(hashes.url).or_default() += 1;
        *self.buckets.entry(hashes.url_without_tld).or_default() += 1;
        *self.buckets.entry(hashes.title).or_default() += 1;
    }
}

pub struct BucketCollector<T: Doc> {
    count: BucketCount,
    documents: MinMaxHeap<ScoredDoc<T>>,
    top_n: usize,
}

impl<T: Doc> BucketCollector<T> {
    pub fn new(top_n: usize, config: CollectorConfig) -> Self {
        assert!(top_n > 0);

        Self {
            top_n,
            documents: MinMaxHeap::with_capacity(config.max_docs_considered + 1),
            count: BucketCount::new(config),
        }
    }

    pub fn insert(&mut self, doc: T) {
        let mut scored_doc: ScoredDoc<T> = doc.into();
        self.count.adjust_score(&mut scored_doc);
        self.documents.push(scored_doc);
    }

    fn update_best_doc(&mut self) {
        if self.documents.len() <= 1 {
            return;
        }

        while let Some(mut best_doc) = self.documents.peek_max_mut() {
            let current_score = best_doc.adjusted_score;
            self.count.adjust_score(&mut *best_doc);

            if best_doc.adjusted_score == current_score {
                break;
            }
        }
    }

    pub fn into_sorted_vec(mut self, de_rank_similar: bool) -> Vec<T> {
        let mut res = Vec::new();
        let mut simhash_dups = Vec::new();
        let mut simhash = simhash::Table::default();

        while let Some(best_doc) = self.documents.pop_max() {
            let hashes = best_doc.doc.hashes();

            if hashes.simhash != 0 && de_rank_similar {
                if simhash.contains(&hashes.simhash) {
                    simhash_dups.push(best_doc.doc);
                    continue;
                }
                simhash.insert(hashes.simhash);
            }

            if de_rank_similar {
                self.count.update_counts(&best_doc);
                self.update_best_doc();
            }

            res.push(best_doc.doc);

            if res.len() == self.top_n {
                break;
            }
        }

        res.extend(
            simhash_dups
                .into_iter()
                .take(self.top_n.saturating_sub(res.len())),
        );

        res
    }
}

#[derive(Debug, Clone)]
pub struct SegmentDoc {
    hashes: Hashes,
    id: DocId,
    segment: SegmentOrdinal,
    score: Score,
}

impl Doc for SegmentDoc {
    fn score(&self) -> f64 {
        self.score.total
    }

    fn hashes(&self) -> Hashes {
        self.hashes
    }
}

pub struct TweakedScoreTopCollector<TScoreTweaker> {
    score_tweaker: TScoreTweaker,
    top_docs: TopDocs,
}

impl<TScoreTweaker> TweakedScoreTopCollector<TScoreTweaker> {
    pub fn new(
        score_tweaker: TScoreTweaker,
        top_docs: TopDocs,
    ) -> TweakedScoreTopCollector<TScoreTweaker> {
        TweakedScoreTopCollector {
            score_tweaker,
            top_docs,
        }
    }

    pub fn top_docs(&self) -> &TopDocs {
        &self.top_docs
    }
}

impl<TScoreTweaker> Collector for TweakedScoreTopCollector<TScoreTweaker>
where
    TScoreTweaker: ScoreTweaker<Score> + Send + Sync,
{
    type Fruit = Vec<WebpagePointer>;

    type Child = TopTweakedScoreSegmentCollector<TScoreTweaker::Child>;

    fn for_segment(
        &self,
        segment_local_id: u32,
        segment_reader: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let segment_scorer = self.score_tweaker.segment_tweaker(segment_reader)?;

        let segment_collector = self
            .top_docs
            .for_segment(segment_local_id, segment_reader)?;

        Ok(TopTweakedScoreSegmentCollector {
            segment_collector,
            segment_scorer,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        segment_fruits: Vec<<Self::Child as tantivy::collector::SegmentCollector>::Fruit>,
    ) -> tantivy::Result<Self::Fruit> {
        let mut collector = BucketCollector::new(
            self.top_docs.top_n + self.top_docs.offset,
            self.top_docs.collector_config.clone(),
        );

        for docs in segment_fruits {
            for doc in docs {
                collector.insert(doc);
            }
        }

        let docs = collector.into_sorted_vec(self.top_docs.de_rank_similar);

        Ok(docs
            .into_iter()
            .skip(self.top_docs.offset)
            .take(self.top_docs.top_n)
            .map(|doc| WebpagePointer {
                score: doc.score,
                hashes: doc.hashes,
                address: DocAddress {
                    segment: doc.segment,
                    doc_id: doc.id,
                },
            })
            .collect())
    }
}

pub struct TopTweakedScoreSegmentCollector<TSegmentScoreTweaker>
where
    TSegmentScoreTweaker: ScoreSegmentTweaker<Score>,
{
    segment_collector: TopSegmentCollector,
    segment_scorer: TSegmentScoreTweaker,
}

impl<TSegmentScoreTweaker> SegmentCollector
    for TopTweakedScoreSegmentCollector<TSegmentScoreTweaker>
where
    TSegmentScoreTweaker: 'static + ScoreSegmentTweaker<Score>,
{
    type Fruit = Vec<SegmentDoc>;

    fn collect(&mut self, doc: DocId, score: tantivy::Score) {
        if self.segment_collector.is_done() {
            return;
        }

        let score = self.segment_scorer.score(doc, score);
        self.segment_collector.collect(doc, score);
    }

    fn harvest(self) -> Self::Fruit {
        self.segment_collector.harvest()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test(top_n: usize, docs: &[(Hashes, DocId, f64)], expected: &[(f64, DocId)]) {
        let mut collector = BucketCollector::new(top_n, CollectorConfig::default());

        for doc in docs {
            collector.insert(SegmentDoc {
                hashes: doc.0,
                id: doc.1,
                score: Score { total: doc.2 },
                segment: 0,
            });
        }

        let res: Vec<(f64, DocId)> = collector
            .into_sorted_vec(true)
            .into_iter()
            .map(|doc| (doc.score.total, doc.id))
            .collect();

        assert_eq!(&res, expected);
    }

    #[test]
    fn all_different() {
        test(
            3,
            &[
                (
                    Hashes {
                        site: 1.into(),
                        title: 1.into(),
                        url: 1.into(),
                        url_without_tld: 1.into(),
                        simhash: 12,
                    },
                    123,
                    1.0,
                ),
                (
                    Hashes {
                        site: 2.into(),
                        title: 2.into(),
                        url: 2.into(),
                        url_without_tld: 2.into(),
                        simhash: 123,
                    },
                    124,
                    2.0,
                ),
                (
                    Hashes {
                        site: 3.into(),
                        title: 3.into(),
                        url: 3.into(),
                        url_without_tld: 3.into(),
                        simhash: 1234,
                    },
                    125,
                    3.0,
                ),
                (
                    Hashes {
                        site: 4.into(),
                        title: 4.into(),
                        url: 4.into(),
                        url_without_tld: 4.into(),
                        simhash: 12345,
                    },
                    126,
                    4.0,
                ),
                (
                    Hashes {
                        site: 5.into(),
                        title: 5.into(),
                        url: 5.into(),
                        url_without_tld: 5.into(),
                        simhash: 123456,
                    },
                    127,
                    5.0,
                ),
            ],
            &[(5.0, 127), (4.0, 126), (3.0, 125)],
        );
    }

    #[test]
    fn less_than_topn() {
        test(
            10,
            &[
                (
                    Hashes {
                        site: 3.into(),
                        title: 3.into(),
                        url: 3.into(),
                        url_without_tld: 3.into(),
                        simhash: 12,
                    },
                    125,
                    3.0,
                ),
                (
                    Hashes {
                        site: 4.into(),
                        title: 4.into(),
                        url: 4.into(),
                        url_without_tld: 4.into(),
                        simhash: 123,
                    },
                    126,
                    4.0,
                ),
                (
                    Hashes {
                        site: 5.into(),
                        title: 5.into(),
                        url: 5.into(),
                        url_without_tld: 5.into(),
                        simhash: 1234,
                    },
                    127,
                    5.0,
                ),
            ],
            &[(5.0, 127), (4.0, 126), (3.0, 125)],
        );
    }

    #[test]
    fn same_key_de_prioritised() {
        test(
            10,
            &[
                (
                    Hashes {
                        site: 1.into(),
                        title: 1.into(),
                        url: 1.into(),
                        url_without_tld: 1.into(),
                        simhash: 12,
                    },
                    125,
                    3.0,
                ),
                (
                    Hashes {
                        site: 2.into(),
                        title: 2.into(),
                        url: 2.into(),
                        url_without_tld: 2.into(),
                        simhash: 123,
                    },
                    126,
                    3.1,
                ),
                (
                    Hashes {
                        site: 2.into(),
                        title: 2.into(),
                        url: 2.into(),
                        url_without_tld: 2.into(),
                        simhash: 1234,
                    },
                    127,
                    5.0,
                ),
            ],
            &[(5.0, 127), (3.0, 125), (3.1, 126)],
        );

        test(
            2,
            &[
                (
                    Hashes {
                        site: 1.into(),
                        title: 1.into(),
                        url: 1.into(),
                        url_without_tld: 1.into(),
                        simhash: 12,
                    },
                    125,
                    3.0,
                ),
                (
                    Hashes {
                        site: 2.into(),
                        title: 2.into(),
                        url: 2.into(),
                        url_without_tld: 2.into(),
                        simhash: 123,
                    },
                    126,
                    3.1,
                ),
                (
                    Hashes {
                        site: 2.into(),
                        title: 2.into(),
                        url: 2.into(),
                        url_without_tld: 2.into(),
                        simhash: 1234,
                    },
                    127,
                    5.0,
                ),
            ],
            &[(5.0, 127), (3.0, 125)],
        );
    }

    #[test]
    fn simhash_dedup() {
        test(
            10,
            &[
                (
                    Hashes {
                        site: 1.into(),
                        title: 1.into(),
                        url: 1.into(),
                        url_without_tld: 1.into(),
                        simhash: 1234,
                    },
                    125,
                    3.0,
                ),
                (
                    Hashes {
                        site: 2.into(),
                        title: 2.into(),
                        url: 2.into(),
                        url_without_tld: 2.into(),
                        simhash: 1234,
                    },
                    126,
                    3.1,
                ),
                (
                    Hashes {
                        site: 3.into(),
                        title: 3.into(),
                        url: 3.into(),
                        url_without_tld: 3.into(),
                        simhash: 1,
                    },
                    127,
                    5.0,
                ),
            ],
            &[(5.0, 127), (3.1, 126), (3.0, 125)],
        );
    }
}
