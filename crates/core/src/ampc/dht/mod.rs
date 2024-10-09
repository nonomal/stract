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
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! Simple in-memory key-value store with Raft consensus where keys
//! and values are arbitrary bytes.
//!
//! It is intended to be deployed across multiple nodes
//! with multiple shards. Each shard cluster
//! is a Raft cluster, and each key is then routed to the correct
//! cluster based on hash(key) % number_of_shards. The keys
//! are currently *not* rebalanced if the number of shards change, so
//! if an entire shard becomes unavailable or a new shard is added, all
//! keys in the entire DHT is essentially lost as the
//! keys might hash incorrectly.
//!
//! Heavily inspired by https://github.com/datafuselabs/openraft/blob/main/examples/raft-kv-memstore/

mod client;
pub mod key;
pub mod log_store;
pub mod network;
pub mod store;
pub mod upsert;
pub mod value;

use network::api::{
    AllTables, BatchSet, BatchUpsert, CloneTable, CreateTable, DropTable, Set, Upsert,
};

use std::fmt::Debug;
use std::io::Cursor;

use openraft::TokioRuntime;

pub use key::{Key, KeyTrait};
pub use value::{Value, ValueTrait};

pub use self::network::Server;

pub use network::api::RemoteClient;
pub use network::raft::RemoteClient as RaftClient;

pub use crate::distributed::member::ShardId;
pub use client::{Client, Shard};
pub use store::Table;
pub use upsert::*;

pub type NodeId = u64;

#[derive(
    serde::Serialize,
    serde::Deserialize,
    bincode::Encode,
    bincode::Decode,
    Debug,
    Clone,
    PartialEq,
    Eq,
    Default,
)]
pub struct BasicNode {
    pub addr: String,
}

impl BasicNode {
    /// Creates as [`BasicNode`].
    pub fn new(addr: impl ToString) -> Self {
        Self {
            addr: addr.to_string(),
        }
    }
}

openraft::declare_raft_types!(
    /// Declare the type configuration for example K/V store.
    pub TypeConfig:
        D = Request,
        R = Response,
        NodeId = NodeId,
        Node = BasicNode,
        Entry = openraft::Entry<TypeConfig>,
        SnapshotData = Cursor<Vec<u8>>,
        AsyncRuntime = TokioRuntime,
);

macro_rules! raft_sonic_request_response {
    ($service:ident, [$($req:ident),*$(,)?]) => {
        #[derive(::serde::Serialize, ::serde::Deserialize, ::bincode::Decode, Clone, Debug)]
        pub enum Request {
            $(
                $req($req),
            )*
        }

        #[derive(::serde::Serialize, ::serde::Deserialize, ::bincode::Encode, Clone, Debug)]
        pub enum Response {
            $(
                $req(<$req as $crate::distributed::sonic::service::Message<$service>>::Response),
            )*
            Empty,
        }

        $(
        impl From<$req> for Request {
            fn from(req: $req) -> Self {
                Request::$req(req)
            }
        }
        )*
    }
}

raft_sonic_request_response!(
    Server,
    [
        Set,
        BatchSet,
        Upsert,
        BatchUpsert,
        CreateTable,
        DropTable,
        AllTables,
        CloneTable
    ]
);

#[cfg(test)]
pub mod tests {
    use super::{key::Key, value::Value};
    use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};
    use tokio::sync::Mutex;
    use tracing_test::traced_test;

    use crate::{ampc::dht, distributed::sonic, free_socket_addr};
    use openraft::{error::InitializeError, Config};

    use proptest::prelude::*;

    use futures::{pin_mut, TryStreamExt};
    use rand::seq::SliceRandom;

    use super::*;

    pub async fn server(
        id: u64,
    ) -> anyhow::Result<(
        openraft::Raft<TypeConfig>,
        sonic::service::Server<Server>,
        SocketAddr,
    )> {
        let config = Config {
            heartbeat_interval: 500,
            election_timeout_min: 1500,
            election_timeout_max: 3000,
            ..Default::default()
        };

        let config = Arc::new(config.validate().unwrap());

        let log_store = log_store::LogStore::<TypeConfig>::default();
        let state_machine_store = Arc::new(store::StateMachineStore::default());

        let network = network::Network;

        let raft = openraft::Raft::new(id, config, network, log_store, state_machine_store.clone())
            .await?;

        let addr = free_socket_addr();

        let server = Server::new(raft.clone(), state_machine_store)
            .bind(addr)
            .await?;

        Ok((raft, server, addr))
    }

    #[tokio::test]
    #[traced_test]
    async fn test_simple_set_get() -> anyhow::Result<()> {
        let (raft1, server1, addr1) = server(1).await?;
        let (_, server2, addr2) = server(2).await?;

        let servers = vec![server1, server2];

        for server in servers {
            tokio::spawn(async move {
                loop {
                    server.accept().await.unwrap();
                }
            });
        }

        let members: BTreeMap<u64, _> = vec![(1, addr1), (2, addr2)]
            .into_iter()
            .map(|(id, addr)| (id, BasicNode::new(addr)))
            .collect();

        if let Err(e) = raft1.initialize(members.clone()).await {
            match e {
                openraft::error::RaftError::APIError(e) => match e {
                    InitializeError::NotAllowed(_) => {}
                    InitializeError::NotInMembers(_) => panic!("{:?}", e),
                },
                openraft::error::RaftError::Fatal(_) => panic!("{:?}", e),
            }
        };

        let c1 = RemoteClient::new(addr1);
        let c2 = RemoteClient::new(addr2);

        let table = Table::from("test");

        c1.set(
            table.clone(),
            "hello".to_string().into(),
            "world".to_string().into(),
        )
        .await?;

        let res = c1.get(table.clone(), "hello".to_string().into()).await?;
        assert_eq!(res, Some("world".to_string().into()));

        c2.set(
            table.clone(),
            "ensure-linearized-read".to_string().into(),
            ().into(),
        )
        .await?;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let res = c2.get(table.clone(), "hello".to_string().into()).await?;
        assert_eq!(res, Some("world".to_string().into()));

        c2.set(
            table.clone(),
            "hello".to_string().into(),
            "world2".to_string().into(),
        )
        .await?;

        c1.set(
            table.clone(),
            "ensure-linearized-read".to_string().into(),
            ().into(),
        )
        .await?;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let res = c1.get(table.clone(), "hello".to_string().into()).await?;
        assert_eq!(res, Some("world2".to_string().into()));

        let res = c1.batch_get(table.clone(), vec![]).await?;
        assert!(res.is_empty());

        let res = c1
            .batch_get(table.clone(), vec!["hello".to_string().into()])
            .await?;

        assert!(res.len() == 1);

        let res = c1
            .batch_get(table.clone(), vec!["non-existent-key".to_string().into()])
            .await?;

        assert!(res.is_empty());

        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_member_join() -> anyhow::Result<()> {
        let (raft1, server1, addr1) = server(1).await?;
        let (_, server2, addr2) = server(2).await?;
        let (_, server3, addr3) = server(3).await?;

        let servers = vec![server1, server2, server3];

        for server in servers {
            tokio::spawn(async move {
                loop {
                    server.accept().await.unwrap();
                }
            });
        }

        let members: BTreeMap<u64, _> = vec![(1, addr1)]
            .into_iter()
            .map(|(id, addr)| (id, BasicNode::new(addr)))
            .collect();

        if let Err(e) = raft1.initialize(members.clone()).await {
            match e {
                openraft::error::RaftError::APIError(e) => match e {
                    InitializeError::NotAllowed(_) => {}
                    InitializeError::NotInMembers(_) => panic!("{:?}", e),
                },
                openraft::error::RaftError::Fatal(_) => panic!("{:?}", e),
            }
        };

        let rc1 = network::raft::RemoteClient::new(addr1).await?;

        rc1.join(2, addr2).await?;

        let c1 = RemoteClient::new(addr1);
        let c2 = RemoteClient::new(addr2);

        let table = Table::from("test");

        c1.set(
            table.clone(),
            "hello".to_string().into(),
            "world".to_string().into(),
        )
        .await?;

        c2.set(
            table.clone(),
            "ensure-linearized-read".to_string().into(),
            ().into(),
        )
        .await?;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let res = c2.get(table.clone(), "hello".to_string().into()).await?;

        assert_eq!(res, Some("world".to_string().into()));

        rc1.join(3, addr3).await?;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await; // join is not blocking

        let c3 = RemoteClient::new(addr3);
        let res = c3.get(table.clone(), "hello".to_string().into()).await?;
        assert_eq!(res, Some("world".to_string().into()));

        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_stream() -> anyhow::Result<()> {
        let (raft, server, addr) = server(1).await?;

        let servers = vec![server];

        for server in servers {
            tokio::spawn(async move {
                loop {
                    server.accept().await.unwrap();
                }
            });
        }

        let members: BTreeMap<u64, _> = vec![(1, addr)]
            .into_iter()
            .map(|(id, addr)| (id, BasicNode::new(addr)))
            .collect();

        if let Err(e) = raft.initialize(members.clone()).await {
            match e {
                openraft::error::RaftError::APIError(e) => match e {
                    InitializeError::NotAllowed(_) => {}
                    InitializeError::NotInMembers(_) => panic!("{:?}", e),
                },
                openraft::error::RaftError::Fatal(_) => panic!("{:?}", e),
            }
        };

        let client = dht::client::Node::new(addr);
        let table = Table::from("test");

        client
            .set(
                table.clone(),
                "hello".to_string().into(),
                "world".to_string().into(),
            )
            .await?;

        client
            .set(
                table.clone(),
                "hello2".to_string().into(),
                "world2".to_string().into(),
            )
            .await?;

        let stream = client.stream(table.clone());
        pin_mut!(stream);

        let mut res = Vec::new();

        while let Some((k, v)) = stream.try_next().await? {
            res.push((k, v));
        }

        res.sort_by_key(|(k, _)| k.clone());

        assert_eq!(
            res,
            vec![
                ("hello".to_string().into(), "world".to_string().into()),
                ("hello2".to_string().into(), "world2".to_string().into()),
            ]
        );

        let table = Table::from("test2");

        const N: u64 = 100_000;
        client
            .batch_set(
                table.clone(),
                (0..N)
                    .map(|i| (i.into(), i.into()))
                    .collect::<Vec<(Key, Value)>>(),
            )
            .await?;

        let stream = client.stream(table.clone());
        pin_mut!(stream);

        let mut res = Vec::new();

        while let Some((k, v)) = stream.try_next().await? {
            res.push((k, v));
        }

        res.sort_by_key(|(k, _)| k.clone());
        assert_eq!(res.len(), N as usize);

        for (i, r) in res.into_iter().enumerate() {
            let i = i as u64;
            assert_eq!(r, (i.into(), i.into(),));
        }

        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    #[ignore = "comitted logs must be stored in stable storage for raft to be able to recover from a node crash"]
    // see: https://docs.rs/openraft/latest/openraft/docs/faq/index.html#what-will-happen-when-data-gets-lost
    async fn test_node_crash() -> anyhow::Result<()> {
        let (raft1, server1, addr1) = server(1).await?;
        let (raft2, server2, addr2) = server(2).await?;

        let servers = vec![server1, server2];
        let mut handles = Vec::new();

        for server in servers {
            handles.push(tokio::spawn(async move {
                loop {
                    server.accept().await.unwrap();
                }
            }));
        }

        let members: BTreeMap<u64, _> = vec![(1, addr1)]
            .into_iter()
            .map(|(id, addr)| (id, BasicNode::new(addr)))
            .collect();

        if let Err(e) = raft1.initialize(members.clone()).await {
            match e {
                openraft::error::RaftError::APIError(e) => match e {
                    InitializeError::NotAllowed(_) => {}
                    InitializeError::NotInMembers(_) => panic!("{:?}", e),
                },
                openraft::error::RaftError::Fatal(_) => panic!("{:?}", e),
            }
        };

        let rc1 = network::raft::RemoteClient::new(addr1).await?;
        rc1.join(2, addr2).await?;

        let c1 = RemoteClient::new(addr1);

        let table = Table::from("test");

        c1.set(
            table.clone(),
            "hello".to_string().into(),
            "world".to_string().into(),
        )
        .await?;

        let res = c1.get(table.clone(), "hello".to_string().into()).await?;
        assert_eq!(res, Some("world".to_string().into()));

        // crash node 2
        tracing::info!("crashing node 2");
        handles[1].abort();
        drop(raft2);

        let (raft2, server2, addr2) = server(2).await?;
        handles[1] = tokio::spawn(async move {
            loop {
                server2.accept().await.unwrap();
            }
        });

        rc1.join(2, addr2).await?;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let c2 = RemoteClient::new(addr2);

        let res = c2.get(table.clone(), "hello".to_string().into()).await?;
        assert_eq!(res, Some("world".to_string().into()));

        // crash node 2 again
        tracing::info!("crashing node 2 again");
        handles[1].abort();
        drop(raft2);

        c1.set(
            table.clone(),
            "hello".to_string().into(),
            "world2".to_string().into(),
        )
        .await?;

        let (_raft2, server2, addr2) = server(2).await?;
        handles[1] = tokio::spawn(async move {
            loop {
                server2.accept().await.unwrap();
            }
        });
        rc1.join(2, addr2).await?;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let c2 = RemoteClient::new(addr2);

        let res = c2.get(table.clone(), "hello".to_string().into()).await?;
        assert_eq!(res, Some("world2".to_string().into()));

        Ok(())
    }

    #[derive(
        Debug,
        Clone,
        serde::Serialize,
        serde::Deserialize,
        bincode::Encode,
        bincode::Decode,
        PartialEq,
    )]
    enum Action {
        Set { key: String, value: String },
        // get actions[prev_key % actions.len()]
        // if actions[prev_key % actions.len()] is a get, then get a non-existent key
        Get { prev_key: usize },
    }

    impl Arbitrary for Action {
        type Parameters = ();
        type Strategy = BoxedStrategy<Self>;

        fn arbitrary_with(_args: ()) -> Self::Strategy {
            prop_oneof![
                (".{1,10}", ".{1,10}").prop_map(|(key, value)| Action::Set { key, value }),
                (0..1000).prop_map(|prev_key| Action::Get {
                    prev_key: prev_key as usize
                }),
            ]
            .boxed()
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10))]

        #[test]
        #[traced_test]
        fn proptest_chaos(actions: Vec<Action>) {
            let ground_truth = Arc::new(Mutex::new(BTreeMap::<String, String>::new()));

            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let (raft1, server1, addr1) = server(1).await.unwrap();
                    let (raft2, server2, addr2) = server(2).await.unwrap();

                    let servers = vec![server1, server2];

                    let mut handles = Vec::new();
                    for server in servers {
                        handles.push(tokio::spawn(async move {
                            loop {
                                server.accept().await.unwrap();
                            }
                        }));
                    }

                    let members: BTreeMap<u64, _> = vec![(1, addr1), (2, addr2)]
                        .into_iter()
                        .map(|(id, addr)| (id, BasicNode::new(addr)))
                        .collect();

                    if let Err(e) = raft1.initialize(members.clone()).await {
                        match e {
                            openraft::error::RaftError::APIError(e) => match e {
                                InitializeError::NotAllowed(_) => {}
                                InitializeError::NotInMembers(_) => panic!("{:?}", e),
                            },
                            openraft::error::RaftError::Fatal(_) => panic!("{:?}", e),
                        }
                    };

                    if let Err(e) = raft2.initialize(members.clone()).await {
                        match e {
                            openraft::error::RaftError::APIError(e) => match e {
                                InitializeError::NotAllowed(_) => {}
                                InitializeError::NotInMembers(_) => panic!("{:?}", e),
                            },
                            openraft::error::RaftError::Fatal(_) => panic!("{:?}", e),
                        }
                    };

                    let c1 = RemoteClient::new(addr1);
                    let c2 = RemoteClient::new(addr2);

                    let clients = Arc::new(vec![c1, c2]);

                    let shared_actions = Arc::new(actions.clone());
                    let table = Table::from("test");

                    for (i, action) in actions.into_iter().enumerate() {
                        match action {
                            Action::Set { key, value } => {
                                let client = clients.choose(&mut rand::thread_rng()).unwrap();

                                client.set(table.clone(), key.clone().into(), value.clone().into()).await.unwrap();
                                ground_truth.lock().await.insert(key.clone(), value.clone());
                            }
                            Action::Get { prev_key } => {
                                let client = clients.choose(&mut rand::thread_rng()).unwrap();
                                client.set(table.clone(), "ensure-linearized-read".to_string().into(), ().into()).await.unwrap();

                                let key = if i == 0 {
                                    "non-existent-key".to_string()
                                } else {
                                    match shared_actions[prev_key % i] {
                                        Action::Set { ref key, .. } => {
                                            key.clone()
                                        },
                                        Action::Get { .. } => "non-existent-key".to_string(),
                                    }
                                };

                                let res = client.get(table.clone(), key.clone().into()).await.unwrap();
                                let expected = ground_truth.lock().await.get(&key).cloned();

                                assert_eq!(res.map(|v| v.try_into().unwrap()), expected);
                            }
                        }
                    }
                });
        }
    }
}
