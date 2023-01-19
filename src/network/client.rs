use std::{
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use anyhow::{Context, Result};
use futures::{future::join_all, stream};
use kate_recovery::{config, data::Cell, matrix::Position};
use libp2p::{
	kad::{record::Key, PeerRecord, Quorum, Record},
	Multiaddr, PeerId,
};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, trace};

use super::Event;

#[derive(Clone)]
pub struct Client {
	sender: mpsc::Sender<Command>,
	/// Number of cells to fetch in parallel
	dht_parallelization_limit: usize,
	/// Cell time to live in DHT (in seconds)
	ttl: u64,
}

struct DHTCell(Cell);

impl DHTCell {
	fn reference(&self, block: u32) -> String {
		self.0.reference(block)
	}

	fn dht_record(&self, block: u32, ttl: u64) -> Record {
		Record {
			key: self.0.reference(block).as_bytes().to_vec().into(),
			value: self.0.content.to_vec(),
			publisher: None,
			expires: Instant::now().checked_add(Duration::from_secs(ttl)),
		}
	}
}

impl Client {
	pub fn new(sender: mpsc::Sender<Command>, dht_parallelization_limit: usize, ttl: u64) -> Self {
		Self {
			sender,
			dht_parallelization_limit,
			ttl,
		}
	}

	pub async fn start_listening(&self, addr: Multiaddr) -> Result<(), anyhow::Error> {
		let (sender, receiver) = oneshot::channel();
		self.sender
			.send(Command::StartListening { addr, sender })
			.await
			.context("Command receiver should not be dropped.")?;
		receiver.await.context("Sender not to be dropped.")?
	}

	pub async fn add_address(
		&self,
		peer_id: PeerId,
		peer_addr: Multiaddr,
	) -> Result<(), anyhow::Error> {
		let (sender, receiver) = oneshot::channel();
		self.sender
			.send(Command::AddAddress {
				peer_id,
				peer_addr,
				sender,
			})
			.await
			.context("Command receiver should not be dropped.")?;
		receiver.await.context("Sender not to be dropped.")?
	}

	// Events stream function creates a new stream of
	// network events and sends a command to the Event loop
	// with a required sender for event output
	pub async fn events_stream(&self) -> ReceiverStream<Event> {
		let (sender, receiver) = mpsc::channel(1000);
		self.sender
			.send(Command::Stream { sender })
			.await
			.expect("Command receiver should not be dropped.");

		ReceiverStream::new(receiver)
	}

	pub async fn bootstrap(&self, nodes: Vec<(PeerId, Multiaddr)>) -> Result<()> {
		let (sender, receiver) = oneshot::channel();
		for (peer, addr) in nodes {
			self.add_address(peer, addr.clone()).await?;
		}

		self.sender
			.send(Command::Bootstrap { sender })
			.await
			.context("Command receiver should not be dropped.")?;
		receiver.await.context("Sender not to be dropped.")?
	}

	async fn get_kad_record(&self, key: Key, quorum: Quorum) -> Result<Vec<PeerRecord>> {
		let (sender, receiver) = oneshot::channel();
		self.sender
			.send(Command::GetKadRecord {
				key,
				quorum,
				sender,
			})
			.await
			.context("Command receiver should not be dropped.")?;
		receiver.await.context("Sender not to be dropped.")?
	}

	async fn put_kad_record(&self, record: Record, quorum: Quorum) -> Result<()> {
		let (sender, receiver) = oneshot::channel();
		self.sender
			.send(Command::PutKadRecord {
				record,
				quorum,
				sender,
			})
			.await
			.context("Command receiver should not be dropped.")?;
		receiver.await.context("Sender not to be dropped.")?
	}

	// Since callers ignores DHT errors, debug logs are used to observe DHT behavior.
	// Return type assumes that cell is not found in case when error is present.
	async fn fetch_cell_from_dht(&self, block_number: u32, position: &Position) -> Option<Cell> {
		let reference = position.reference(block_number);
		let record_key = Key::from(reference.as_bytes().to_vec());

		trace!("Getting DHT record for reference {}", reference);

		match self.get_kad_record(record_key, Quorum::One).await {
			Ok(peer_records) => {
				debug!("Fetched cell {reference} from the DHT");

				// For now, we take only the first record from the list
				let Some(peer_record) = peer_records.into_iter().next() else {
				    return None;
				};

				let try_content: Result<[u8; config::COMMITMENT_SIZE + config::CHUNK_SIZE], _> =
					peer_record.record.value.try_into();

				let Ok(content) = try_content else {
				    debug!("Cannot convert cell {reference} into 80 bytes");
				    return None;
				};

				let position = position.clone();
				Some(Cell { position, content })
			},
			Err(error) => {
				debug!("Cell {reference} not found in the DHT: {error}");
				None
			},
		}
	}

	/// Fetches cells from DHT.
	/// Returns fetched cells and unfetched positions (so we can try RPC fetch).
	///
	/// # Arguments
	///
	/// * `block_number` - Block number
	/// * `positions` - Cell positions to fetch
	pub async fn fetch_cells_from_dht(
		&self,
		block_number: u32,
		positions: &[Position],
	) -> (Vec<Cell>, Vec<Position>) {
		let mut cells = Vec::<Option<Cell>>::with_capacity(positions.len());

		for positions in positions.chunks(self.dht_parallelization_limit) {
			let fetch = |position| self.fetch_cell_from_dht(block_number, position);
			let results = join_all(positions.iter().map(fetch)).await;
			cells.extend(results.into_iter().collect::<Vec<_>>());
		}

		let unfetched = cells
			.iter()
			.zip(positions)
			.filter(|(cell, _)| cell.is_none())
			.map(|(_, position)| position.clone())
			.collect::<Vec<_>>();

		let fetched = cells.into_iter().flatten().collect();

		(fetched, unfetched)
	}

	/// Inserts cells into the DHT.
	/// There is no rollback, and errors will be logged and skipped,
	/// which means that we cannot rely on error logs as alert mechanism.
	/// Returns the success rate of the PUT operations measured by dividing
	/// the number of returned errors with the total number of input cells
	///
	/// # Arguments
	///
	/// * `block` - Block number
	/// * `cells` - Matrix cells to store into DHT
	pub async fn insert_into_dht(&self, block: u32, cells: Vec<Cell>) -> f32 {
		if cells.is_empty() {
			return 1.0;
		}

		let cells: Vec<_> = cells.into_iter().map(DHTCell).collect::<Vec<_>>();
		let failure_counter: &Arc<Mutex<usize>> = &Arc::new(Mutex::new(0));
		let cell_tuples = cells
			.iter()
			.map(move |b| (b, self.clone(), failure_counter.clone()));

		futures::StreamExt::for_each_concurrent(
			stream::iter(cell_tuples),
			self.dht_parallelization_limit,
			|(cell, network_client, failure_counter)| async move {
				let reference = cell.reference(block);
				if let Err(error) = network_client
					.put_kad_record(cell.dht_record(block, self.ttl), Quorum::One)
					.await
				{
					let mut counter = failure_counter.lock().unwrap();
					*counter += 1;
					debug!("Fail to put record for cell {reference} to DHT: {error}");
				}
			},
		)
		.await;

		let counter = failure_counter.lock().unwrap();
		(1.0 - (counter.to_owned() as f32 / cells.len() as f32)) as f32
	}
}

#[derive(Debug)]
pub enum Command {
	StartListening {
		addr: Multiaddr,
		sender: oneshot::Sender<Result<()>>,
	},
	AddAddress {
		peer_id: PeerId,
		peer_addr: Multiaddr,
		sender: oneshot::Sender<Result<()>>,
	},
	Stream {
		sender: mpsc::Sender<Event>,
	},
	Bootstrap {
		sender: oneshot::Sender<Result<()>>,
	},
	GetKadRecord {
		key: Key,
		quorum: Quorum,
		sender: oneshot::Sender<Result<Vec<PeerRecord>>>,
	},
	PutKadRecord {
		record: Record,
		quorum: Quorum,
		sender: oneshot::Sender<Result<()>>,
	},
}