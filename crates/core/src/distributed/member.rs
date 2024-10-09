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

use std::net::SocketAddr;

use crate::config::WebgraphGranularity;

#[derive(
    serde::Serialize,
    serde::Deserialize,
    bincode::Encode,
    bincode::Decode,
    PartialEq,
    Eq,
    Hash,
    Clone,
    Copy,
    Debug,
    PartialOrd,
    Ord,
)]
pub struct ShardId(u64);

impl std::fmt::Display for ShardId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ShardId({})", self.0)
    }
}

impl ShardId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }

    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl From<u64> for ShardId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl From<ShardId> for u64 {
    fn from(id: ShardId) -> u64 {
        id.0
    }
}

#[derive(
    serde::Serialize,
    serde::Deserialize,
    bincode::Encode,
    bincode::Decode,
    PartialEq,
    Eq,
    Hash,
    Clone,
    Debug,
)]
pub enum LiveIndexState {
    InSetup,
    Ready,
}

impl std::fmt::Display for LiveIndexState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiveIndexState::InSetup => write!(f, "setup"),
            LiveIndexState::Ready => write!(f, "ready"),
        }
    }
}

#[derive(
    serde::Serialize,
    serde::Deserialize,
    bincode::Encode,
    bincode::Decode,
    PartialEq,
    Eq,
    Hash,
    Clone,
    Debug,
)]
pub enum Service {
    Searcher {
        host: SocketAddr,
        shard: ShardId,
    },
    EntitySearcher {
        host: SocketAddr,
    },
    LiveIndex {
        host: SocketAddr,
        shard: ShardId,
        state: LiveIndexState,
    },
    Api {
        host: SocketAddr,
    },
    Webgraph {
        host: SocketAddr,
        shard: ShardId,
        granularity: WebgraphGranularity,
    },
    Dht {
        host: SocketAddr,
        shard: ShardId,
    },
    HarmonicWorker {
        host: SocketAddr,
        shard: ShardId,
    },
    HarmonicCoordinator {
        host: SocketAddr,
    },
    ApproxHarmonicWorker {
        host: SocketAddr,
        shard: ShardId,
    },
    ApproxHarmonicCoordinator {
        host: SocketAddr,
    },
}

impl std::fmt::Display for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Searcher { host, shard } => write!(f, "Searcher {} {}", host, shard),
            Self::EntitySearcher { host } => write!(f, "EntitySearcher {}", host),
            Self::LiveIndex { host, shard, state } => {
                write!(f, "LiveIndex {} {} {}", host, shard, state)
            }
            Self::Api { host } => write!(f, "Api {}", host),
            Self::Webgraph {
                host,
                shard,
                granularity,
            } => {
                write!(f, "Webgraph {} {} {}", host, shard, granularity)
            }
            Self::Dht { host, shard } => write!(f, "Dht {} {}", host, shard),
            Self::HarmonicWorker { host, shard } => write!(f, "HarmonicWorker {} {}", host, shard),
            Self::HarmonicCoordinator { host } => write!(f, "HarmonicCoordinator {}", host),
            Self::ApproxHarmonicWorker { host, shard } => {
                write!(f, "ApproxHarmonicWorker {} {}", host, shard)
            }
            Self::ApproxHarmonicCoordinator { host } => {
                write!(f, "ApproxHarmonicCoordinator {}", host)
            }
        }
    }
}

impl Service {
    pub fn is_searcher(&self) -> bool {
        matches!(self, Self::Searcher { .. })
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Debug, bincode::Encode, bincode::Decode)]
pub struct Member {
    pub id: String,
    pub service: Service,
}

impl Member {
    pub fn new(service: Service) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        Self { id, service }
    }
}
