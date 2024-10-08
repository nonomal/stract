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

use crate::{
    config::LiveIndexConfig, entrypoint::search_server, inverted_index, live_index::LiveIndex,
    Result,
};
use std::{net::SocketAddr, sync::Arc};

use file_store::gen_temp_path;

use crate::{
    ampc::dht::ShardId,
    distributed::{
        cluster::Cluster,
        member::{LiveIndexState, Service},
        sonic,
    },
    entrypoint::{indexer::IndexableWebpage, live_index::IndexWebpages},
    free_socket_addr,
};

use super::LiveIndexService;

struct RemoteIndex {
    host: SocketAddr,
    shard: ShardId,
    gossip_addr: SocketAddr,
    underlying_index: Arc<LiveIndex>,
    cluster: Arc<Cluster>,
}

impl RemoteIndex {
    async fn start(shard: ShardId, gossip_seed: Vec<SocketAddr>) -> Result<Self> {
        let path = gen_temp_path();

        let host = free_socket_addr();
        let gossip_addr = free_socket_addr();

        let gossip_seed = if gossip_seed.is_empty() {
            None
        } else {
            Some(gossip_seed)
        };

        let config = LiveIndexConfig {
            user_agent: crate::config::UserAgent {
                full: "TestBot".to_string(),
                token: "TestBot".to_string(),
            },
            robots_txt_cache_sec: 60 * 60,
            min_politeness_factor: 1,
            start_politeness_factor: 3,
            min_crawl_delay_ms: 5_000,
            max_crawl_delay_ms: 60_000,
            max_politeness_factor: 2048,
            max_url_slowdown_retry: 5,
            timeout_seconds: 30,
            host_centrality_store_path: path
                .as_path()
                .join("host_centrality")
                .to_str()
                .unwrap()
                .to_string(),
            page_centrality_store_path: None,
            safety_classifier_path: None,
            host_centrality_threshold: None,
            minimum_clean_words: None,
            cluster_id: "test-cluster".to_string(),
            gossip_seed_nodes: gossip_seed,
            gossip_addr,
            shard_id: shard,
            index_path: path.as_path().join("index").to_str().unwrap().to_string(),
            linear_model_path: None,
            lambda_model_path: None,
            host,
            collector: Default::default(),
            snippet: Default::default(),
        };

        let service = LiveIndexService::new(config).await?;
        let cluster = service.cluster_handle.clone();
        let index = service.index.clone();

        service.background_setup();

        let server = service.bind(&host).await.unwrap();

        tokio::task::spawn(async move {
            loop {
                if let Err(e) = server.accept().await {
                    tracing::error!("{:?}", e);
                }
            }
        });

        Ok(Self {
            host,
            shard,
            gossip_addr,
            underlying_index: index,
            cluster,
        })
    }

    async fn conn(&self) -> Result<sonic::service::Connection<LiveIndexService>> {
        Ok(sonic::service::Connection::create(self.host).await?)
    }

    async fn index_pages(
        &self,
        pages: Vec<IndexableWebpage>,
        consistency_fraction: Option<f64>,
    ) -> Result<()> {
        self.conn()
            .await?
            .send(IndexWebpages {
                pages,
                consistency_fraction,
            })
            .await??;

        Ok(())
    }

    async fn await_ready(&self, cluster: &Cluster) {
        cluster
            .await_member(|member| {
                if let Service::LiveIndex { host, shard, state } = member.service.clone() {
                    self.shard == shard
                        && matches!(state, LiveIndexState::Ready)
                        && host == self.host
                } else {
                    false
                }
            })
            .await;
    }

    async fn search(&self, query: &str) -> Result<Vec<inverted_index::RetrievedWebpage>> {
        let mut conn = self.conn().await?;

        let websites: Vec<inverted_index::WebpagePointer> = conn
            .send(search_server::Search {
                query: query.to_string().into(),
            })
            .await?
            .map(|res| {
                res.websites
                    .into_iter()
                    .map(|page| page.pointer().clone())
                    .collect()
            })
            .unwrap_or_default();

        Ok(conn
            .send(search_server::RetrieveWebsites {
                websites,
                query: query.to_string(),
            })
            .await?
            .unwrap())
    }

    async fn commit_underlying(&self) {
        self.underlying_index.commit();
    }

    async fn kill(self) -> Result<()> {
        self.cluster.remove_service().await?;

        Ok(())
    }
}

const CLUSTER_ID: &str = "test-cluster";

#[tokio::test]
async fn test_shard_without_replica() -> Result<()> {
    let shard1 = RemoteIndex::start(ShardId::new(1), vec![]).await?;
    let shard2 = RemoteIndex::start(ShardId::new(2), vec![shard1.gossip_addr]).await?;

    let cluster = Cluster::join_as_spectator(
        CLUSTER_ID.to_string(),
        free_socket_addr(),
        vec![shard1.gossip_addr],
    )
    .await?;

    shard1.await_ready(&cluster).await;
    shard2.await_ready(&cluster).await;

    shard1
        .index_pages(
            vec![IndexableWebpage {
                url: "https://a.com/".to_string(),
                body: "
                <title>test page</title>
                Example webpage
                "
                .to_string(),
                fetch_time_ms: 100,
            }],
            None,
        )
        .await?;
    shard2
        .index_pages(
            vec![IndexableWebpage {
                url: "https://b.com/".to_string(),
                body: "
                <title>test page</title>
                Example webpage
                "
                .to_string(),
                fetch_time_ms: 100,
            }],
            None,
        )
        .await?;

    shard1.commit_underlying().await;
    shard2.commit_underlying().await;

    let res1 = shard1.search("test").await?;

    assert_eq!(res1.len(), 1);
    assert_eq!(res1[0].url, "https://a.com/");

    let res2 = shard2.search("test").await?;
    assert_eq!(res2.len(), 1);
    assert_eq!(res2[0].url, "https://b.com/");

    Ok(())
}

#[tokio::test]
async fn test_replica_no_fails() -> Result<()> {
    let rep1 = RemoteIndex::start(ShardId::new(1), vec![]).await?;
    let rep2 = RemoteIndex::start(ShardId::new(1), vec![rep1.gossip_addr]).await?;

    let cluster = Cluster::join_as_spectator(
        CLUSTER_ID.to_string(),
        free_socket_addr(),
        vec![rep1.gossip_addr],
    )
    .await?;

    rep1.await_ready(&cluster).await;
    rep2.await_ready(&cluster).await;

    rep1.index_pages(
        vec![IndexableWebpage {
            url: "https://a.com/".to_string(),
            body: "
                <title>test page</title>
                Example webpage
                "
            .to_string(),
            fetch_time_ms: 100,
        }],
        Some(1.0),
    )
    .await?;
    rep2.index_pages(
        vec![IndexableWebpage {
            url: "https://b.com/".to_string(),
            body: "
                <title>test page</title>
                Example webpage
                "
            .to_string(),
            fetch_time_ms: 100,
        }],
        Some(1.0),
    )
    .await?;

    rep1.commit_underlying().await;
    rep2.commit_underlying().await;

    let res1 = rep1.search("test").await?;

    assert_eq!(res1.len(), 2);

    let res2 = rep2.search("test").await?;
    assert_eq!(res2.len(), 2);

    Ok(())
}

#[tokio::test]
async fn test_replica_setup_after_inserts() -> Result<()> {
    let rep1 = RemoteIndex::start(ShardId::new(1), vec![]).await?;

    let cluster = Cluster::join_as_spectator(
        CLUSTER_ID.to_string(),
        free_socket_addr(),
        vec![rep1.gossip_addr],
    )
    .await?;

    rep1.await_ready(&cluster).await;

    rep1.index_pages(
        vec![IndexableWebpage {
            url: "https://a.com/".to_string(),
            body: "
                <title>test page</title>
                Example webpage
                "
            .to_string(),
            fetch_time_ms: 100,
        }],
        Some(1.0),
    )
    .await?;
    rep1.index_pages(
        vec![IndexableWebpage {
            url: "https://b.com/".to_string(),
            body: "
                <title>test page</title>
                Example webpage
                "
            .to_string(),
            fetch_time_ms: 100,
        }],
        Some(1.0),
    )
    .await?;

    rep1.commit_underlying().await;

    let rep2 = RemoteIndex::start(ShardId::new(1), vec![rep1.gossip_addr]).await?;
    rep2.await_ready(&cluster).await;

    rep2.commit_underlying().await;

    let res1 = rep1.search("test").await?;

    assert_eq!(res1.len(), 2);

    let res2 = rep2.search("test").await?;
    assert_eq!(res2.len(), 2);

    Ok(())
}

#[tokio::test]
async fn test_replica_recovery() -> Result<()> {
    let rep1 = RemoteIndex::start(ShardId::new(1), vec![]).await?;
    let rep2 = RemoteIndex::start(ShardId::new(1), vec![rep1.gossip_addr]).await?;

    let cluster = Cluster::join_as_spectator(
        CLUSTER_ID.to_string(),
        free_socket_addr(),
        vec![rep1.gossip_addr],
    )
    .await?;

    rep1.await_ready(&cluster).await;
    rep2.await_ready(&cluster).await;

    rep1.index_pages(
        vec![IndexableWebpage {
            url: "https://a.com/".to_string(),
            body: "
                <title>test page</title>
                Example webpage
                "
            .to_string(),
            fetch_time_ms: 100,
        }],
        Some(1.0),
    )
    .await?;

    rep2.kill().await?;

    loop {
        if let Ok(_) = rep1
            .index_pages(
                vec![IndexableWebpage {
                    url: "https://b.com/".to_string(),
                    body: "
                    <title>test page</title>
                    Example webpage
                    "
                    .to_string(),
                    fetch_time_ms: 100,
                }],
                Some(1.0),
            )
            .await
        {
            break;
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    rep1.commit_underlying().await;

    let rep2 = RemoteIndex::start(ShardId::new(1), vec![rep1.gossip_addr]).await?;
    rep2.await_ready(&cluster).await;

    rep2.commit_underlying().await;

    let res1 = rep1.search("test").await?;

    assert_eq!(res1.len(), 2);

    let res2 = rep2.search("test").await?;
    assert_eq!(res2.len(), 2);

    Ok(())
}
