// Copyright 2023 litep2p developers
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

#![allow(unused)]

use crate::{
    error::Error,
    peer_id::PeerId,
    protocol::{
        libp2p::kademlia::{
            bucket::KBucketEntry,
            handle::KademliaCommand,
            message::KademliaMessage,
            query::{QueryEngine, QueryId},
            routing_table::RoutingTable,
            store::MemoryStore,
            types::{ConnectionType, Key},
        },
        Direction, Transport, TransportEvent, TransportService,
    },
    substream::{Substream, SubstreamSet},
    types::SubstreamId,
};

use bytes::BytesMut;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc::{Receiver, Sender};

use std::collections::{hash_map::Entry, HashMap, VecDeque};

pub use {
    config::{Config, ConfigBuilder},
    handle::{KademliaEvent, KademliaHandle},
    record::{Key as RecordKey, Record},
};

/// Logging target for the file.
const LOG_TARGET: &str = "ipfs::kademlia";

/// Kademlia replication factor, `k`.
const _REPLICATION_FACTOR: usize = 20;

/// Parallelism factor, `α`.
const PARALLELISM_FACTOR: usize = 3;

mod bucket;
mod config;
mod handle;
mod message;
mod query;
mod record;
mod routing_table;
mod store;
mod types;

mod schema {
    pub(super) mod kademlia {
        include!(concat!(env!("OUT_DIR"), "/kademlia.rs"));
    }
}

/// Peer context.
struct PeerContext {
    /// Pending query ID, if any.
    query: Option<QueryId>,
}

impl PeerContext {
    /// Add pending query for the peer.
    fn add_pending_query(&mut self, query: QueryId) {
        self.query = Some(query);
    }
}

/// Main Kademlia object.
pub struct Kademlia {
    /// Transport service.
    service: TransportService,

    /// Local Kademlia key.
    _local_key: Key<PeerId>,

    /// Connected peers,
    peers: HashMap<PeerId, PeerContext>,

    /// Substream set.
    substreams: SubstreamSet<PeerId>,

    /// TX channel for sending events to `KademliaHandle`.
    _event_tx: Sender<KademliaEvent>,

    /// RX channel for receiving commands from `KademliaHandle`.
    cmd_rx: Receiver<KademliaCommand>,

    /// Routing table.
    routing_table: RoutingTable,

    /// Record store.
    store: MemoryStore,

    /// Pending dials.
    pending_dials: HashMap<PeerId, QueryId>,

    /// Query engine.
    engine: QueryEngine,
}

impl Kademlia {
    /// Create new [`Kademlia`].
    pub fn new(service: TransportService, config: Config) -> Self {
        let local_key = Key::from(service.local_peer_id);

        Self {
            service,
            peers: HashMap::new(),
            cmd_rx: config.cmd_rx,
            store: MemoryStore::new(),
            _event_tx: config.event_tx,
            _local_key: local_key.clone(),
            pending_dials: HashMap::new(),
            substreams: SubstreamSet::new(),
            routing_table: RoutingTable::new(local_key),
            engine: QueryEngine::new(PARALLELISM_FACTOR),
        }
    }

    /// Connection established to remote peer.
    async fn on_connection_established(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection established");

        match self.peers.entry(peer) {
            Entry::Vacant(entry) => {
                // TODO: add peer to routing table
                // TODO: verify that peer limit is respected
                let query = self.pending_dials.remove(&peer);
                entry.insert(PeerContext { query });

                if query.is_some() {
                    self.service.open_substream(peer).await?;
                }

                Ok(())
            }
            Entry::Occupied(_) => return Err(Error::PeerAlreadyExists(peer)),
        }
    }

    /// Disconnect peer from `Kademlia`.
    ///
    /// Peer is disconnected either because the substream was detected closed
    /// or because the connection was closed.
    ///
    /// The peer is kept in the routing table but its connection state is set
    /// as `NotConnected`, meaning it can be evicted from a k-bucket if another
    /// peer that shares the bucket connects.
    async fn disconnect_peer(&mut self, peer: PeerId, query: Option<QueryId>) {
        tracing::debug!(target: LOG_TARGET, ?peer, "disconnect peer");

        if let Some(_query) = query {
            // self.engine.register_response_failure(query, peer);
        }

        if let Some(mut substream) = self.substreams.remove(&peer) {
            let _ = substream.close().await;
        }

        self.service.disconnect(&peer);
        self.peers.remove(&peer);

        if let KBucketEntry::Occupied(entry) = self.routing_table.entry(Key::from(peer)) {
            entry.connection = ConnectionType::NotConnected;
        }
    }

    /// Poll actions from `QueryEngine`.
    async fn poll_query_engine(&mut self, query_id: QueryId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?query_id, "poll query engine");

        // // TODO: this is such an ugly function, refactor it
        // while let Some(action) = self.engine.next_action(query_id) {
        //     match action {
        //         QueryAction::SendFindNode { peer } => match self.substreams.get_mut(&peer.peer) {
        //             Some(substream) => {
        //                 let message = KademliaMessage::find_node(peer.peer);

        //                 tracing::warn!("SEND FIND NODE TO {}", peer.peer);

        //                 match substream.send(message.into()).await {
        //                     Err(_error) => {
        //                         self.engine.register_response_failure(query_id, peer.peer)
        //                     }
        //                     Ok(_) => self
        //                         .peers
        //                         .get_mut(&peer.peer)
        //                         .ok_or(Error::PeerDoesntExist(peer.peer))?
        //                         .add_pending_query(query_id),
        //                 }
        //             }
        //             None => match self.peers.get_mut(&peer.peer) {
        //                 Some(context) => match self.service.open_substream(peer.peer).await {
        //                     Ok(_substream_id) => context.add_pending_query(query_id),
        //                     Err(_error) => self.disconnect_peer(peer.peer, Some(query_id)).await,
        //                 },
        //                 None => {
        //                     tracing::error!(target: LOG_TARGET, addresses = ?peer.addresses, "open connection to peer");

        //                     self.service
        //                         .dial_address(peer.addresses[0].clone())
        //                         .await
        //                         .unwrap();
        //                     self.pending_dials.insert(peer.peer, query_id);
        //                 }
        //             },
        //         },
        //         QueryAction::QuerySucceeded { target, peers } => {
        //             let peers = peers
        //                 .into_iter()
        //                 .map(|info| (info.peer, info.addresses))
        //                 .collect();
        //             self._event_tx
        //                 .send(KademliaEvent::FindNodeResult { target, peers })
        //                 .await?;
        //         }
        //         QueryAction::QueryFailed { query } => {
        //             tracing::error!(target: LOG_TARGET, ?query, "QUERY FAILED");
        //             // todo!("cancel pending queries");
        //         }
        //     }
        // }

        Ok(())
    }

    /// Local node opened a substream to remote node.
    async fn on_outbound_substream(
        &mut self,
        peer: PeerId,
        substream_id: SubstreamId,
        mut substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::debug!(
            target: LOG_TARGET,
            ?peer,
            ?substream_id,
            "outbound substream opened"
        );

        // if let Some((query, message)) = self.engine.next_peer_action(&peer) {
        //     if let Err(error) = substream.send(message.into()).await {
        //         tracing::debug!(
        //             target: LOG_TARGET,
        //             ?peer,
        //             ?error,
        //             "failed to send `FIND_NODE` message to peer"
        //         );

        //         self.disconnect_peer(peer, Some(query)).await;
        //         return Ok(());
        //     }
        // }

        self.substreams.insert(peer, substream);
        Ok(())

        // match self.engine.next_peer_action(&peer) {
        //     None => {

        //     }
        // }

        // // if the substream was opened but there is no query pending for the peer,
        // // just store the opened substream in the `SubstreamSet` and return early
        // let query = match self
        //     .peers
        //     .get_mut(&peer)
        //     .ok_or(Error::PeerDoesntExist(peer))?
        //     .query
        //     .take()
        // {
        //     Some(query) => query,
        //     None => {
        //         self.substreams.insert(peer, substream);
        //         return Ok(());
        //     }
        // };

        // // attempt to send the pending query to peer and remove the peer if the send fails
        // // TODO: ugly
        // let target_peer = self.engine.target_peer(query).ok_or(Error::InvalidState)?;
        // let message = KademliaMessage::find_node(target_peer);

        // // if the send operation fails, the peer is disconnected and `QueryEngine` is notified
        // // of the failure which makes it updates its internal bookkeeping about the query state.
        // // after its notified, the engine is polled again to make progress on the state of the query
        // if let Err(error) = substream.send(message.into()).await {
        //     tracing::debug!(
        //         target: LOG_TARGET,
        //         ?peer,
        //         ?error,
        //         "failed to send `FIND_NODE` message to peer"
        //     );

        //     self.disconnect_peer(peer, Some(query)).await;
        //     return self.poll_query_engine(query).await;
        // }

        // TODO: ugly
        // self.substreams.insert(peer, substream);
        // self.peers
        //     .get_mut(&peer)
        //     .ok_or(Error::PeerDoesntExist(peer))?
        //     .add_pending_query(query);

        // Ok(())
    }

    /// Remote opened a substream to local node.
    async fn on_inbound_substream(
        &mut self,
        peer: PeerId,
        _substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "inbound substream opened");

        Ok(())
    }

    /// Handle received message.
    async fn on_message_received(&mut self, peer: PeerId, message: BytesMut) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "handle message from peer");

        match KademliaMessage::from_bytes(message).ok_or(Error::InvalidData)? {
            KademliaMessage::FindNodeRequest { target } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?target,
                    "handle `FIND_NODE` request"
                );

                let substream = self
                    .substreams
                    .get_mut(&peer)
                    .ok_or(Error::SubstreamDoesntExist)?;

                let message = KademliaMessage::find_node_response(
                    self.routing_table.closest(Key::from(target), 20),
                );

                if let Err(_error) = substream.send(message.into()).await {
                    // TODO: check if peer has an active query in progress
                    self.disconnect_peer(peer, None).await;
                }
            }
            KademliaMessage::FindNodeResponse { peers } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?peers,
                    "handle `FIND_NODE` response"
                );

                if let Some(query) = self
                    .peers
                    .get_mut(&peer)
                    .ok_or(Error::PeerDoesntExist(peer))?
                    .query
                    .take()
                {
                    // self.engine.register_find_node_response(query, peer, peers);
                    return self.poll_query_engine(query).await;
                } else {
                    tracing::error!(target: LOG_TARGET, "error");
                }
            }
        }

        Ok(())
    }

    /// Execute `FIND_NODE`.
    async fn on_find_node(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "starting `FIND_NODE` query");

        let candidates: VecDeque<_> = self.routing_table.closest(Key::from(peer), 20).into();

        // start new `FIND_NODE` query
        let query_id = self.engine.start_find_node(peer, candidates);

        // poll query engine and send `FIND_NODE` messages to known remote peers
        // self.poll_query_engine(query_id).await
        todo!();
    }

    /// Store value to DHT by executing `PUT_VALUE`.
    async fn on_put_value(&mut self, record: Record) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, key = ?record.key, "store record to DHT");

        self.store.put(record.clone());

        let key = Key::new(record.key);
        let _candidates: VecDeque<_> = self.routing_table.closest(key, 20).into();

        // TODO: what needs to happen here:
        // TODO:  - register start of `PUT_VALUE` query to `QueryEngine`
        // TODO:  - engine records the state of the query in its own state
        // TODO:  - next time query engine is polled, it will
        // TODO:  -
        // TODO:  -
        // TODO:  -
        // TODO:  -
        // TODO:  -

        // self.engine.start_put_record(record.key, candidates);
        // for peer in candidates {
        //     tracing::warn!(
        //         target: LOG_TARGET,
        //         "distance: {:?}",
        //         key.distance(&peer.key)
        //     );
        // }

        Ok(())
    }

    /// Failed to open substream to remote peer.
    fn on_substream_open_failure(&mut self, substream: SubstreamId, error: Error) {
        // TODO: report to query engine
        tracing::debug!(
            target: LOG_TARGET,
            ?substream,
            ?error,
            "failed to open substream"
        );
    }

    /// [`Kademlia`] event loop.
    pub async fn run(mut self) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, "starting kademlia event loop");

        loop {
            // TODO: poll query engine here?

            tokio::select! {
                event = self.service.next_event() => match event {
                    Some(TransportEvent::ConnectionEstablished { peer, .. }) => {
                        if let Err(error) = self.on_connection_established(peer).await {
                            tracing::debug!(target: LOG_TARGET, ?error, "failed to handle established connection");
                        }
                    }
                    Some(TransportEvent::ConnectionClosed { peer }) => {
                        self.disconnect_peer(peer, None).await;
                    }
                    Some(TransportEvent::SubstreamOpened { peer, direction, substream, .. }) => {
                        match direction {
                            Direction::Inbound => {
                                if let Err(error) = self.on_inbound_substream(peer, substream).await {
                                    tracing::debug!(
                                        target: LOG_TARGET,
                                        ?peer,
                                        ?error,
                                        "failed to handle inbound substream",
                                    );
                                }
                            }
                            Direction::Outbound(substream_id) => {
                                if let Err(error) = self.on_outbound_substream(peer, substream_id, substream).await {
                                    tracing::debug!(
                                        target: LOG_TARGET,
                                        ?peer,
                                        ?substream_id,
                                        ?error,
                                        "failed to handle outbound substream",
                                    );
                                }
                            }
                        }
                    },
                    Some(TransportEvent::SubstreamOpenFailure { substream, error }) => {
                        self.on_substream_open_failure(substream, error);
                    }
                    Some(TransportEvent::DialFailure { .. }) => todo!(),
                    None => return Err(Error::EssentialTaskClosed),
                },
                command = self.cmd_rx.recv() => {
                    let result = match command {
                        Some(KademliaCommand::FindNode { peer }) => self.on_find_node(peer).await,
                        Some(KademliaCommand::AddKnownPeer { peer, addresses }) => {
                            self.routing_table.add_known_peer(
                                peer,
                                addresses,
                                self.peers
                                    .get(&peer)
                                    .map_or(ConnectionType::NotConnected, |_| ConnectionType::Connected),
                            );

                            Ok(())
                        }
                        Some(KademliaCommand::PutRecord { record }) => self.on_put_value(record).await,
                        None => Err(Error::EssentialTaskClosed),
                    };

                    if let Err(error) = result {
                        tracing::debug!(target: LOG_TARGET, ?error, "failed to handle command");
                    }
                },
                event = self.substreams.next() => match event {
                    Some((peer, message)) => match message {
                        Ok(message) => {
                            if let Err(error) = self.on_message_received(peer, message).await {
                                tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to handle message");
                            }
                        }
                        Err(error) => return Err(error),
                    },
                    None => return Err(Error::EssentialTaskClosed),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codec::ProtocolCodec, crypto::ed25519::Keypair, transport::manager::TransportManager,
        types::protocol::ProtocolName,
    };
    use tokio::sync::mpsc::channel;

    #[allow(unused)]
    struct Context {
        _cmd_tx: Sender<KademliaCommand>,
        _event_rx: Receiver<KademliaEvent>,
    }

    fn _make_kademlia() -> (Kademlia, Context, TransportManager) {
        let (manager, handle) = TransportManager::new(Keypair::generate());

        let peer = PeerId::random();
        let (transport_service, _tx) = TransportService::new(
            peer,
            ProtocolName::from("/kad/1"),
            Default::default(),
            handle,
        );
        let (event_tx, _event_rx) = channel(64);
        let (_cmd_tx, cmd_rx) = channel(64);

        let config = Config {
            protocol: ProtocolName::from("/kad/1"),
            codec: ProtocolCodec::UnsignedVarint(None),
            replication_factor: 20usize,
            event_tx,
            cmd_rx,
        };

        (
            Kademlia::new(transport_service, config),
            Context { _cmd_tx, _event_rx },
            manager,
        )
    }
}
