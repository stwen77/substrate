// Copyright 2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Discovery mechanisms of Substrate.
//!
//! The `DiscoveryBehaviour` struct implements the `NetworkBehaviour` trait of libp2p and is
//! responsible for discovering other nodes that are part of the network.
//!
//! Substrate uses the following mechanisms in order to discover nodes that are part of the network:
//!
//! - Bootstrap nodes. These are hard-coded node identities and addresses passed in the constructor
//! of the `DiscoveryBehaviour`. You can also call `add_known_address` later to add an entry.
//!
//! - mDNS. Discovers nodes on the local network by broadcasting UDP packets.
//!
//! - Kademlia random walk. Once connected, we perform random Kademlia `FIND_NODE` requests in
//! order for nodes to propagate to us their view of the network. This is performed automatically
//! by the `DiscoveryBehaviour`.
//!
//! Additionally, the `DiscoveryBehaviour` is also capable of storing and loading value in the
//! network-wide DHT.
//!
//! ## Usage
//!
//! The `DiscoveryBehaviour` generates events of type `DiscoveryOut`, most notably
//! `DiscoveryOut::Discovered` that is generated whenever we discover a node.
//! Only the identity of the node is returned. The node's addresses are stored within the
//! `DiscoveryBehaviour` and can be queried through the `NetworkBehaviour` trait.
//!
//! **Important**: In order for the discovery mechanism to work properly, there needs to be an
//! active mechanism that asks nodes for the addresses they are listening on. Whenever we learn
//! of a node's address, you must call `add_self_reported_address`.
//!

use futures::prelude::*;
use libp2p::core::{Multiaddr, PeerId, ProtocolsHandler, PublicKey};
use libp2p::core::swarm::{ConnectedPoint, NetworkBehaviour, NetworkBehaviourAction};
use libp2p::core::swarm::PollParameters;
#[cfg(not(target_os = "unknown"))]
use libp2p::core::{swarm::toggle::Toggle, nodes::Substream, muxing::StreamMuxerBox};
use libp2p::kad::{GetValueResult, Kademlia, KademliaOut, PutValueResult};
#[cfg(not(target_os = "unknown"))]
use libp2p::mdns::{Mdns, MdnsEvent};
use libp2p::multihash::Multihash;
use libp2p::multiaddr::Protocol;
use log::{debug, info, trace, warn};
use std::{cmp, collections::VecDeque, num::NonZeroU8, time::Duration};
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_timer::{Delay, clock::Clock};

/// Implementation of `NetworkBehaviour` that discovers the nodes on the network.
pub struct DiscoveryBehaviour<TSubstream> {
	/// User-defined list of nodes and their addresses. Typically includes bootstrap nodes and
	/// reserved nodes.
	user_defined: Vec<(PeerId, Multiaddr)>,
	/// Kademlia requests and answers.
	kademlia: Kademlia<TSubstream>,
	/// Discovers nodes on the local network.
	#[cfg(not(target_os = "unknown"))]
	mdns: Toggle<Mdns<Substream<StreamMuxerBox>>>,
	/// Stream that fires when we need to perform the next random Kademlia query.
	next_kad_random_query: Delay,
	/// After `next_kad_random_query` triggers, the next one triggers after this duration.
	duration_to_next_kad: Duration,
	/// Discovered nodes to return.
	discoveries: VecDeque<PeerId>,
	/// `Clock` instance that uses the current execution context's source of time.
	clock: Clock,
	/// Identity of our local node.
	local_peer_id: PeerId,
	/// Number of nodes we're currently connected to.
	num_connections: u64,
}

impl<TSubstream> DiscoveryBehaviour<TSubstream> {
	/// Builds a new `DiscoveryBehaviour`.
	///
	/// `user_defined` is a list of known address for nodes that never expire.
	pub fn new(
		local_public_key: PublicKey,
		user_defined: Vec<(PeerId, Multiaddr)>,
		enable_mdns: bool
	) -> Self {
		if enable_mdns {
			#[cfg(target_os = "unknown")]
			warn!(target: "sub-libp2p", "mDNS is not available on this platform");
		}

		let mut kademlia = Kademlia::new(local_public_key.clone().into_peer_id());
		for (peer_id, addr) in &user_defined {
			kademlia.add_address(peer_id, addr.clone());
		}

		let clock = Clock::new();
		DiscoveryBehaviour {
			user_defined,
			kademlia,
			next_kad_random_query: Delay::new(clock.now()),
			duration_to_next_kad: Duration::from_secs(1),
			discoveries: VecDeque::new(),
			clock,
			local_peer_id: local_public_key.into_peer_id(),
			num_connections: 0,
			#[cfg(not(target_os = "unknown"))]
			mdns: if enable_mdns {
				match Mdns::new() {
					Ok(mdns) => Some(mdns).into(),
					Err(err) => {
						warn!(target: "sub-libp2p", "Failed to initialize mDNS: {:?}", err);
						None.into()
					}
				}
			} else {
				None.into()
			},
		}
	}

	/// Returns the list of nodes that we know exist in the network.
	pub fn known_peers(&mut self) -> impl Iterator<Item = &PeerId> {
		self.kademlia.kbuckets_entries()
	}

	/// Adds a hard-coded address for the given peer, that never expires.
	///
	/// This adds an entry to the parameter that was passed to `new`.
	///
	/// If we didn't know this address before, also generates a `Discovered` event.
	pub fn add_known_address(&mut self, peer_id: PeerId, addr: Multiaddr) {
		if self.user_defined.iter().all(|(p, a)| *p != peer_id && *a != addr) {
			self.discoveries.push_back(peer_id.clone());
			self.user_defined.push((peer_id, addr));
		}
	}

	/// Call this method when a node reports an address for itself.
	///
	/// **Note**: It is important that you call this method, otherwise the discovery mechanism will
	/// not properly work.
	pub fn add_self_reported_address(&mut self, peer_id: &PeerId, addr: Multiaddr) {
		self.kademlia.add_address(peer_id, addr);
	}

	/// Start fetching a record from the DHT.
	///
	/// A corresponding `ValueFound` or `ValueNotFound` event will later be generated.
	pub fn get_value(&mut self, key: &Multihash) {
		self.kademlia.get_value(key, NonZeroU8::new(10)
								.expect("Casting 10 to NonZeroU8 should succeed; qed"));
	}

	/// Start putting a record into the DHT. Other nodes can later fetch that value with
	/// `get_value`.
	///
	/// A corresponding `ValuePut` or `ValuePutFailed` event will later be generated.
	pub fn put_value(&mut self, key: Multihash, value: Vec<u8>) {
		self.kademlia.put_value(key, value);
	}
}

/// Event generated by the `DiscoveryBehaviour`.
pub enum DiscoveryOut {
	/// We have discovered a node. Can be called multiple times with the same identity.
	Discovered(PeerId),

	/// The DHT yeided results for the record request, grouped in (key, value) pairs.
	ValueFound(Vec<(Multihash, Vec<u8>)>),

	/// The record requested was not found in the DHT.
	ValueNotFound(Multihash),

	/// The record with a given key was successfully inserted into the DHT.
	ValuePut(Multihash),

	/// Inserting a value into the DHT failed.
	ValuePutFailed(Multihash),
}

impl<TSubstream> NetworkBehaviour for DiscoveryBehaviour<TSubstream>
where
	TSubstream: AsyncRead + AsyncWrite,
{
	type ProtocolsHandler = <Kademlia<TSubstream> as NetworkBehaviour>::ProtocolsHandler;
	type OutEvent = DiscoveryOut;

	fn new_handler(&mut self) -> Self::ProtocolsHandler {
		NetworkBehaviour::new_handler(&mut self.kademlia)
	}

	fn addresses_of_peer(&mut self, peer_id: &PeerId) -> Vec<Multiaddr> {
		let mut list = self.user_defined.iter()
			.filter_map(|(p, a)| if p == peer_id { Some(a.clone()) } else { None })
			.collect::<Vec<_>>();
		list.extend(self.kademlia.addresses_of_peer(peer_id));
		trace!(target: "sub-libp2p", "Addresses of {:?} are {:?}", peer_id, list);
		if list.is_empty() {
			if self.kademlia.kbuckets_entries().any(|p| p == peer_id) {
				debug!(target: "sub-libp2p", "Requested dialing to {:?} (peer in k-buckets), \
					and no address was found", peer_id);
			} else {
				debug!(target: "sub-libp2p", "Requested dialing to {:?} (peer not in k-buckets), \
					and no address was found", peer_id);
			}
		}
		list
	}

	fn inject_connected(&mut self, peer_id: PeerId, endpoint: ConnectedPoint) {
		self.num_connections += 1;
		NetworkBehaviour::inject_connected(&mut self.kademlia, peer_id, endpoint)
	}

	fn inject_disconnected(&mut self, peer_id: &PeerId, endpoint: ConnectedPoint) {
		self.num_connections -= 1;
		NetworkBehaviour::inject_disconnected(&mut self.kademlia, peer_id, endpoint)
	}

	fn inject_replaced(&mut self, peer_id: PeerId, closed: ConnectedPoint, opened: ConnectedPoint) {
		NetworkBehaviour::inject_replaced(&mut self.kademlia, peer_id, closed, opened)
	}

	fn inject_node_event(
		&mut self,
		peer_id: PeerId,
		event: <Self::ProtocolsHandler as ProtocolsHandler>::OutEvent,
	) {
		NetworkBehaviour::inject_node_event(&mut self.kademlia, peer_id, event)
	}

	fn inject_new_external_addr(&mut self, addr: &Multiaddr) {
		let new_addr = addr.clone()
			.with(Protocol::P2p(self.local_peer_id.clone().into()));
		info!(target: "sub-libp2p", "Discovered new external address for our node: {}", new_addr);
	}

	fn inject_expired_listen_addr(&mut self, addr: &Multiaddr) {
		info!(target: "sub-libp2p", "No longer listening on {}", addr);
	}

	fn poll(
		&mut self,
		params: &mut impl PollParameters,
	) -> Async<
		NetworkBehaviourAction<
			<Self::ProtocolsHandler as ProtocolsHandler>::InEvent,
			Self::OutEvent,
		>,
	> {
		// Immediately process the content of `discovered`.
		if let Some(peer_id) = self.discoveries.pop_front() {
			let ev = DiscoveryOut::Discovered(peer_id);
			return Async::Ready(NetworkBehaviourAction::GenerateEvent(ev));
		}

		// Poll the stream that fires when we need to start a random Kademlia query.
		loop {
			match self.next_kad_random_query.poll() {
				Ok(Async::NotReady) => break,
				Ok(Async::Ready(_)) => {
					let random_peer_id = PeerId::random();
					debug!(target: "sub-libp2p", "Libp2p <= Starting random Kademlia request for \
						{:?}", random_peer_id);
					self.kademlia.find_node(random_peer_id);

					// Reset the `Delay` to the next random.
					self.next_kad_random_query.reset(self.clock.now() + self.duration_to_next_kad);
					self.duration_to_next_kad = cmp::min(self.duration_to_next_kad * 2,
						Duration::from_secs(60));
				},
				Err(err) => {
					warn!(target: "sub-libp2p", "Kademlia query timer errored: {:?}", err);
					break
				}
			}
		}

		// Poll Kademlia.
		loop {
			match self.kademlia.poll(params) {
				Async::NotReady => break,
				Async::Ready(NetworkBehaviourAction::GenerateEvent(ev)) => {
					match ev {
						KademliaOut::Discovered { .. } => {}
						KademliaOut::KBucketAdded { peer_id, .. } => {
							let ev = DiscoveryOut::Discovered(peer_id);
							return Async::Ready(NetworkBehaviourAction::GenerateEvent(ev));
						}
						KademliaOut::FindNodeResult { key, closer_peers } => {
							trace!(target: "sub-libp2p", "Libp2p => Query for {:?} yielded {:?} results",
								key, closer_peers.len());
							if closer_peers.is_empty() && self.num_connections != 0 {
								warn!(target: "sub-libp2p", "Libp2p => Random Kademlia query has yielded empty \
									results");
							}
						}
						KademliaOut::GetValueResult(res) => {
							let ev = match res {
								GetValueResult::Found { results } => {
									let results = results
											.into_iter()
											.map(|r| (r.key, r.value))
											.collect();

									DiscoveryOut::ValueFound(results)
								}
								GetValueResult::NotFound { key, .. } => {
									DiscoveryOut::ValueNotFound(key)
								}
							};
							return Async::Ready(NetworkBehaviourAction::GenerateEvent(ev));
						}
						KademliaOut::PutValueResult(res) => {
							let ev = match res {
								PutValueResult::Ok{ key, .. } =>  {
									DiscoveryOut::ValuePut(key)
								}
								PutValueResult::Err { key, .. } => {
									DiscoveryOut::ValuePutFailed(key)
								}
							};
							return Async::Ready(NetworkBehaviourAction::GenerateEvent(ev));
						}
						// We never start any other type of query.
						KademliaOut::GetProvidersResult { .. } => {}
					}
				},
				Async::Ready(NetworkBehaviourAction::DialAddress { address }) =>
					return Async::Ready(NetworkBehaviourAction::DialAddress { address }),
				Async::Ready(NetworkBehaviourAction::DialPeer { peer_id }) =>
					return Async::Ready(NetworkBehaviourAction::DialPeer { peer_id }),
				Async::Ready(NetworkBehaviourAction::SendEvent { peer_id, event }) =>
					return Async::Ready(NetworkBehaviourAction::SendEvent { peer_id, event }),
				Async::Ready(NetworkBehaviourAction::ReportObservedAddr { address }) =>
					return Async::Ready(NetworkBehaviourAction::ReportObservedAddr { address }),
			}
		}

		// Poll mDNS.
		#[cfg(not(target_os = "unknown"))]
		loop {
			match self.mdns.poll(params) {
				Async::NotReady => break,
				Async::Ready(NetworkBehaviourAction::GenerateEvent(event)) => {
					match event {
						MdnsEvent::Discovered(list) => {
							self.discoveries.extend(list.into_iter().map(|(peer_id, _)| peer_id));
							if let Some(peer_id) = self.discoveries.pop_front() {
								let ev = DiscoveryOut::Discovered(peer_id);
								return Async::Ready(NetworkBehaviourAction::GenerateEvent(ev));
							}
						},
						MdnsEvent::Expired(_) => {}
					}
				},
				Async::Ready(NetworkBehaviourAction::DialAddress { address }) =>
					return Async::Ready(NetworkBehaviourAction::DialAddress { address }),
				Async::Ready(NetworkBehaviourAction::DialPeer { peer_id }) =>
					return Async::Ready(NetworkBehaviourAction::DialPeer { peer_id }),
				Async::Ready(NetworkBehaviourAction::SendEvent { event, .. }) =>
					match event {},		// `event` is an enum with no variant
				Async::Ready(NetworkBehaviourAction::ReportObservedAddr { address }) =>
					return Async::Ready(NetworkBehaviourAction::ReportObservedAddr { address }),
			}
		}

		Async::NotReady
	}
}

#[cfg(test)]
mod tests {
	use futures::prelude::*;
	use libp2p::identity::Keypair;
	use libp2p::Multiaddr;
	use libp2p::core::{upgrade, Swarm};
	use libp2p::core::transport::{Transport, MemoryTransport};
	use libp2p::core::upgrade::{InboundUpgradeExt, OutboundUpgradeExt};
	use std::collections::HashSet;
	use super::{DiscoveryBehaviour, DiscoveryOut};

	#[test]
	fn discovery_working() {
		let mut user_defined = Vec::new();

		// Build swarms whose behaviour is `DiscoveryBehaviour`.
		let mut swarms = (0..25).map(|_| {
			let keypair = Keypair::generate_ed25519();

			let transport = MemoryTransport
				.with_upgrade(libp2p::secio::SecioConfig::new(keypair.clone()))
				.and_then(move |out, endpoint| {
					let peer_id = out.remote_key.into_peer_id();
					let peer_id2 = peer_id.clone();
					let upgrade = libp2p::yamux::Config::default()
						.map_inbound(move |muxer| (peer_id, muxer))
						.map_outbound(move |muxer| (peer_id2, muxer));
					upgrade::apply(out.stream, upgrade, endpoint)
				});

			let behaviour = DiscoveryBehaviour::new(keypair.public(), user_defined.clone(), false);
			let mut swarm = Swarm::new(transport, behaviour, keypair.public().into_peer_id());
			let listen_addr: Multiaddr = format!("/memory/{}", rand::random::<u64>()).parse().unwrap();

			if user_defined.is_empty() {
				user_defined.push((keypair.public().into_peer_id(), listen_addr.clone()));
			}

			Swarm::listen_on(&mut swarm, listen_addr.clone()).unwrap();
			(swarm, listen_addr)
		}).collect::<Vec<_>>();

		// Build a `Vec<HashSet<PeerId>>` with the list of nodes remaining to be discovered.
		let mut to_discover = (0..swarms.len()).map(|n| {
			(0..swarms.len()).filter(|p| *p != n)
				.map(|p| Swarm::local_peer_id(&swarms[p].0).clone())
				.collect::<HashSet<_>>()
		}).collect::<Vec<_>>();

		let fut = futures::future::poll_fn(move || -> Result<_, ()> {
			loop {
				let mut keep_polling = false;

				for swarm_n in 0..swarms.len() {
					if let Async::Ready(Some(DiscoveryOut::Discovered(other))) =
						swarms[swarm_n].0.poll().unwrap() {
						if to_discover[swarm_n].remove(&other) {
							keep_polling = true;
							// Call `add_self_reported_address` to simulate identify happening.
							let addr = swarms.iter()
								.find(|s| *Swarm::local_peer_id(&s.0) == other)
								.unwrap()
								.1.clone();
							swarms[swarm_n].0.add_self_reported_address(&other, addr);
						}
					}
				}

				if !keep_polling {
					break;
				}
			}

			if to_discover.iter().all(|l| l.is_empty()) {
				Ok(Async::Ready(()))
			} else {
				Ok(Async::NotReady)
			}
		});

		tokio::runtime::Runtime::new().unwrap().block_on(fut).unwrap();
	}
}
