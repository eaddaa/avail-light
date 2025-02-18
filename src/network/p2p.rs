use anyhow::{Context, Result};
use futures::future::Either;
use kad_mem_store::{MemoryStore, MemoryStoreConfig};
use libp2p::{
	autonat::{self, Behaviour as AutoNat},
	core::{muxing::StreamMuxerBox, transport::OrTransport, upgrade::Version},
	dcutr::Behaviour as Dcutr,
	dns::TokioDnsConfig,
	identify::{self, Behaviour as Identify},
	identity,
	kad::{Kademlia, KademliaCaching, KademliaConfig, Mode},
	mdns::{tokio::Behaviour as Mdns, Config as MdnsConfig},
	noise::Config as NoiseConfig,
	ping::{Behaviour as Ping, Config as PingConfig},
	quic::{tokio::Transport as TokioQuic, Config as QuicConfig},
	relay::{self, client::Behaviour as RelayClient},
	swarm::{NetworkBehaviour, SwarmBuilder},
	PeerId, Transport,
};
use multihash::{self, Hasher};
use tokio::sync::mpsc::{self};
use tracing::info;

#[cfg(feature = "network-analysis")]
pub mod analyzer;
mod client;
mod event_loop;
mod kad_mem_store;
pub use client::Client;
use event_loop::EventLoop;

use crate::types::{LibP2PConfig, SecretKey};

// DHTPutSuccess enum is used to signal back and then
// count the successful DHT Put operations.
// Used for single or batch operations.
#[derive(Clone, Debug, PartialEq)]
pub enum DHTPutSuccess {
	Batch(usize),
	Single,
}

// Behaviour struct is used to derive delegated Libp2p behaviour implementation
#[derive(NetworkBehaviour)]
#[behaviour(event_process = false)]
pub struct Behaviour {
	kademlia: Kademlia<MemoryStore>,
	identify: Identify,
	ping: Ping,
	mdns: Mdns,
	auto_nat: AutoNat,
	relay_client: RelayClient,
	dcutr: Dcutr,
}

// Init function initializes all needed needed configs for the functioning
// p2p network Client and network Event Loop
pub fn init(
	cfg: LibP2PConfig,
	dht_parallelization_limit: usize,
	ttl: u64,
	put_batch_size: usize,
	is_fat_client: bool,
	id_keys: libp2p::identity::Keypair,
) -> Result<(Client, EventLoop)> {
	let local_peer_id = PeerId::from(id_keys.public());
	info!(
		"Local peer id: {:?}. Public key: {:?}.",
		local_peer_id,
		id_keys.public()
	);

	// create Transport
	// init relay transport configuration used in relay clients
	let (relay_client_transport, relay_client_behaviour) = relay::client::new(local_peer_id);
	let transport = {
		let quic_transport = TokioQuic::new(QuicConfig::new(&id_keys));
		// upgrade relay transport to be used with swarm
		let upgraded_relay_transport = relay_client_transport
			.upgrade(Version::V1Lazy)
			.authenticate(NoiseConfig::new(&id_keys)?)
			.multiplex(libp2p::yamux::Config::default());
		// relay transport only handles listening and dialing on relayed [`Multiaddr`]
		// and depends on other transport to do the actual transmission of data, we have to combine the two
		let transport =
			OrTransport::new(upgraded_relay_transport, quic_transport).map(|either_output, _| {
				match either_output {
					Either::Left((peer_id, connection)) => {
						(peer_id, StreamMuxerBox::new(connection))
					},
					Either::Right((peer_id, connection)) => {
						(peer_id, StreamMuxerBox::new(connection))
					},
				}
			});
		// wrap transport for DNS lookups
		TokioDnsConfig::system(transport)?.boxed()
	};

	// Initialize Network Behaviour Struct
	// configure Kademlia Memory Store
	let kad_store = MemoryStore::with_config(
		local_peer_id,
		MemoryStoreConfig {
			max_records: cfg.kademlia.max_kad_record_number, // ~2hrs
			max_value_bytes: cfg.kademlia.max_kad_record_size + 1,
			max_providers_per_key: usize::from(cfg.kademlia.record_replication_factor), // Needs to match the replication factor, per libp2p docs
			max_provided_keys: cfg.kademlia.max_kad_provided_keys,
		},
	);
	// create Kademlia Config
	let mut kad_cfg = KademliaConfig::default();
	kad_cfg
		.set_publication_interval(cfg.kademlia.publication_interval)
		.set_replication_interval(cfg.kademlia.record_replication_interval)
		.set_replication_factor(cfg.kademlia.record_replication_factor)
		.set_connection_idle_timeout(cfg.kademlia.connection_idle_timeout)
		.set_query_timeout(cfg.kademlia.query_timeout)
		.set_parallelism(cfg.kademlia.query_parallelism)
		.set_caching(KademliaCaching::Enabled {
			max_peers: cfg.kademlia.caching_max_peers,
		})
		.disjoint_query_paths(cfg.kademlia.disjoint_query_paths)
		.set_record_filtering(libp2p::kad::KademliaStoreInserts::FilterBoth);

	// create Identify Protocol Config
	let identify_cfg = identify::Config::new(cfg.identify.protocol_version, id_keys.public())
		.with_agent_version(cfg.identify.agent_version);
	// create AutoNAT Client Config
	let autonat_cfg = autonat::Config {
		retry_interval: cfg.autonat.retry_interval,
		refresh_interval: cfg.autonat.refresh_interval,
		boot_delay: cfg.autonat.boot_delay,
		throttle_server_period: cfg.autonat.throttle_server_period,
		only_global_ips: cfg.autonat.only_global_ips,
		..Default::default()
	};

	let mut behaviour = Behaviour {
		ping: Ping::new(PingConfig::new()),
		identify: Identify::new(identify_cfg),
		relay_client: relay_client_behaviour,
		dcutr: Dcutr::new(local_peer_id),
		kademlia: Kademlia::with_config(local_peer_id, kad_store, kad_cfg),
		auto_nat: AutoNat::new(local_peer_id, autonat_cfg),
		mdns: Mdns::new(MdnsConfig::default(), local_peer_id)?,
	};

	if is_fat_client {
		behaviour.kademlia.set_mode(Some(Mode::Server));
	}

	// Build the Swarm, connecting the lower transport logic with the
	// higher layer network behaviour logic
	let swarm = SwarmBuilder::with_tokio_executor(transport, behaviour, local_peer_id).build();

	// create sender channel for Event Loop Commands
	let (command_sender, command_receiver) = mpsc::channel(10000);

	Ok((
		Client::new(
			command_sender,
			dht_parallelization_limit,
			ttl,
			put_batch_size,
		),
		EventLoop::new(
			swarm,
			command_receiver,
			cfg.relays,
			cfg.bootstrap_interval,
			is_fat_client,
		),
	))
}

// Keypair function creates identity Keypair for a local node.
// From such generated keypair it derives multihash identifier of the local peer.
pub fn keypair(cfg: LibP2PConfig) -> Result<(libp2p::identity::Keypair, String)> {
	let keypair = match cfg.secret_key {
		// If seed is provided, generate secret key from seed
		Some(SecretKey::Seed { seed }) => {
			let seed_digest = multihash::Sha3_256::digest(seed.as_bytes());
			identity::Keypair::ed25519_from_bytes(seed_digest)
				.context("error generating secret key from seed")?
		},
		// Import secret key if provided
		Some(SecretKey::Key { key }) => {
			let mut decoded_key = [0u8; 32];
			hex::decode_to_slice(key.into_bytes(), &mut decoded_key)
				.context("error decoding secret key from config")?;
			identity::Keypair::ed25519_from_bytes(decoded_key)
				.context("error importing secret key")?
		},
		// If neither seed nor secret key provided, generate secret key from random seed
		None => identity::Keypair::generate_ed25519(),
	};
	let peer_id = PeerId::from(keypair.public()).to_string();
	Ok((keypair, peer_id))
}
