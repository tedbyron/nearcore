#![allow(unused_imports)]
#![allow(unused_variables)]


use near_chain::{near_chain_primitives, ChainStoreAccess, Error};
use std::cmp::min;
use std::collections::{HashMap};
use std::ops::Add;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration as TimeDuration;

use ansi_term::Color::{Purple, Yellow};
use chrono::{DateTime, Duration};
use futures::{future, FutureExt};
use log::{debug, error, info, warn};
use rand::seq::{IteratorRandom, SliceRandom};
use rand::{thread_rng, Rng};

use near_chain::{Chain, RuntimeAdapter};
use near_network::types::{FullPeerInfo, NetworkRequests, NetworkResponses, PeerManagerAdapter};
use near_primitives::block::Tip;
use near_primitives::hash::CryptoHash;
use near_primitives::syncing::get_num_state_parts;
use near_primitives::time::{Clock, Utc};
use near_primitives::types::{
    AccountId, BlockHeight, BlockHeightDelta, ShardId, StateRoot,
};
use near_primitives::utils::to_timestamp;

use near_chain::chain::{ApplyStatePartsRequest, StateSplitRequest};
use near_client_primitives::types::{
    DownloadStatus, ShardSyncDownload, ShardSyncStatus, SyncStatus,
};
use near_network::types::PeerManagerMessageRequest;
use near_network_primitives::types::AccountOrPeerIdOrHash;
use near_primitives::shard_layout::ShardUId;

/// Maximum number of block headers send over the network.
pub const MAX_BLOCK_HEADERS: u64 = 512;

/// Maximum number of block header hashes to send as part of a locator.
pub const MAX_BLOCK_HEADER_HASHES: usize = 20;

/// Maximum number of state parts to request per peer on each round when node is trying to download the state.
pub const MAX_STATE_PART_REQUEST: u64 = 16;
/// Number of state parts already requested stored as pending.
/// This number should not exceed MAX_STATE_PART_REQUEST times (number of peers in the network).
pub const MAX_PENDING_PART: u64 = MAX_STATE_PART_REQUEST * 10000;

pub const NS_PER_SECOND: u128 = 1_000_000_000;

/// Helper to keep track of sync headers.
/// Handles major re-orgs by finding closest header that matches and re-downloading headers from that point.
pub struct HeaderSync {
    network_adapter: Arc<dyn PeerManagerAdapter>,
    history_locator: Vec<(BlockHeight, CryptoHash)>,
    syncing_peer: Option<FullPeerInfo>,
}

impl HeaderSync {
    pub fn new(
        network_adapter: Arc<dyn PeerManagerAdapter>,
        initial_timeout: TimeDuration,
        progress_timeout: TimeDuration,
        stall_ban_timeout: TimeDuration,
        expected_height_per_second: u64,
    ) -> Self {
        HeaderSync {
            network_adapter,
            history_locator: vec![],
            syncing_peer: None,
        }
    }

    pub fn run(
        &mut self,
        chain: &mut Chain,
        highest_height: BlockHeight,
        highest_height_peers: &Vec<FullPeerInfo>,
    ) -> Result<(), near_chain::Error> {
        let header_head = chain.header_head()?;
        self.syncing_peer = None;
        if let Some(peer) = highest_height_peers.choose(&mut thread_rng()).cloned() {
            if peer.chain_info.height > header_head.height {
                self.syncing_peer = self.request_headers(chain, peer);
            }
        }
        Ok(())
    }

    /// Request headers from a given peer to advance the chain.
    fn request_headers(&mut self, chain: &mut Chain, peer: FullPeerInfo) -> Option<FullPeerInfo> {
        if let Ok(locator) = self.get_locator(chain) {
            debug!(target: "sync", "Sync: request headers: asking {} for headers, {:?}", peer.peer_info.id, locator);
            self.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
                NetworkRequests::BlockHeadersRequest {
                    hashes: locator,
                    peer_id: peer.peer_info.id.clone(),
                },
            ));
            return Some(peer);
        }
        None
    }

    fn get_locator(&mut self, chain: &mut Chain) -> Result<Vec<CryptoHash>, near_chain::Error> {
        let tip = chain.header_head()?;
        let genesis_height = chain.genesis().height();
        let heights = get_locator_heights(tip.height - genesis_height)
            .into_iter()
            .map(|h| h + genesis_height)
            .collect::<Vec<_>>();

        // For each height we need, we either check if something is close enough from last locator, or go to the db.
        let mut locator: Vec<(u64, CryptoHash)> = vec![(tip.height, tip.last_block_hash)];
        for h in heights {
            if let Some(x) = close_enough(&self.history_locator, h) {
                locator.push(x);
            } else {
                // Walk backwards to find last known hash.
                let last_loc = *locator.last().unwrap();
                if let Ok(header) = chain.get_header_by_height(h) {
                    if header.height() != last_loc.0 {
                        locator.push((header.height(), *header.hash()));
                    }
                }
            }
        }
        locator.dedup_by(|a, b| a.0 == b.0);
        debug!(target: "sync", "Sync: locator: {:?}", locator);
        self.history_locator = locator.clone();
        Ok(locator.iter().map(|x| x.1).collect())
    }
}

/// Check if there is a close enough value to provided height in the locator.
fn close_enough(locator: &Vec<(u64, CryptoHash)>, height: u64) -> Option<(u64, CryptoHash)> {
    if locator.len() == 0 {
        return None;
    }
    // Check boundaries, if lower than the last.
    if locator.last().unwrap().0 >= height {
        return locator.last().map(|x| *x);
    }
    // Higher than first and first is within acceptable gap.
    if locator[0].0 < height && height.saturating_sub(127) < locator[0].0 {
        return Some(locator[0]);
    }
    for h in locator.windows(2) {
        if height <= h[0].0 && height > h[1].0 {
            if h[0].0 - height < height - h[1].0 {
                return Some(h[0]);
            } else {
                return Some(h[1]);
            }
        }
    }
    None
}

/// Given height stepping back to 0 in powers of 2 steps.
fn get_locator_heights(height: u64) -> Vec<u64> {
    let mut current = height;
    let mut heights = vec![];
    while current > 0 {
        heights.push(current);
        if heights.len() >= MAX_BLOCK_HEADER_HASHES as usize - 1 {
            break;
        }
        let next = 2u64.pow(heights.len() as u32);
        current = if current > next { current - next } else { 0 };
    }
    heights.push(0);
    heights
}

pub struct BlockSyncRequest {
    height: BlockHeight,
    hash: CryptoHash,
}

/// Helper to track block syncing.
pub struct BlockSync {
    network_adapter: Arc<dyn PeerManagerAdapter>,
    last_request: Option<BlockSyncRequest>,
    /// Whether to enforce block sync
    archive: bool,
}

impl BlockSync {
    pub fn new(
        network_adapter: Arc<dyn PeerManagerAdapter>,
        archive: bool,
    ) -> Self {
        BlockSync { network_adapter, last_request: None, archive }
    }

    /// Runs check if block sync is needed, if it's needed and it's too far - sync state is started instead (returning true).
    /// Otherwise requests recent blocks from peers.
    pub fn run(
        &mut self,
        chain: &mut Chain,
        highest_height: BlockHeight,
        highest_height_peers: &[FullPeerInfo],
    ) -> Result<bool, near_chain::Error> {
        if self.block_sync(chain, highest_height_peers)? {
            debug!(target: "sync", "Sync: transition to State Sync.");
            return Ok(true);
        }
        Ok(false)
    }

    /// Returns true if state download is required (last known block is too far).
    /// Otherwise request recent blocks from peers round robin.
    pub fn block_sync(
        &mut self,
        chain: &mut Chain,
        highest_height_peers: &[FullPeerInfo],
    ) -> Result<bool, near_chain::Error> {
        let reference_hash = match &self.last_request {
            Some(request) if chain.is_chunk_orphan(&request.hash) => request.hash,
            _ => chain.head()?.last_block_hash,
        };

        let reference_hash = {
            // Find the most recent block we know on the canonical chain.
            // In practice the forks from the last final block are very short, so it is
            // acceptable to perform this on each request
            let header = chain.get_block_header(&reference_hash)?;
            let mut candidate = (header.height(), *header.hash(), *header.prev_hash());

            // First go back until we find the common block
            while match chain.get_header_by_height(candidate.0) {
                Ok(header) => header.hash() != &candidate.1,
                Err(e) => match e.kind() {
                    near_chain::ErrorKind::DBNotFoundErr(_) => true,
                    _ => return Err(e),
                },
            } {
                let prev_header = chain.get_block_header(&candidate.2)?;
                candidate = (prev_header.height(), *prev_header.hash(), *prev_header.prev_hash());
            }

            // Then go forward for as long as we known the next block
            let mut ret_hash = candidate.1;
            loop {
                match chain.mut_store().get_next_block_hash(&ret_hash) {
                    Ok(hash) => {
                        let hash = *hash;
                        if chain.block_exists(&hash)? {
                            ret_hash = hash;
                        } else {
                            break;
                        }
                    }
                    Err(e) => match e.kind() {
                        near_chain::ErrorKind::DBNotFoundErr(_) => break,
                        _ => return Err(e),
                    },
                }
            }

            ret_hash
        };

        let next_hash = match chain.mut_store().get_next_block_hash(&reference_hash) {
            Ok(hash) => *hash,
            Err(e) => match e.kind() {
                near_chain::ErrorKind::DBNotFoundErr(_) => {
                    return Ok(false);
                }
                _ => return Err(e),
            },
        };
        let next_height = chain.get_block_header(&next_hash)?.height();
        let request = BlockSyncRequest { height: next_height, hash: next_hash };

        let head = chain.head()?;
        let header_head = chain.header_head()?;

        let gc_stop_height = chain.runtime_adapter.get_gc_stop_height(&header_head.last_block_hash);

        let request_from_archival = self.archive && request.height < gc_stop_height;
        let peer = if request_from_archival {
            let archival_peer_iter = highest_height_peers.iter().filter(|p| p.chain_info.archival);
            archival_peer_iter.choose(&mut rand::thread_rng())
        } else {
            let peer_iter = highest_height_peers.iter();
            peer_iter.choose(&mut rand::thread_rng())
        };

        if let Some(peer) = peer {
            debug!(target: "sync", "Block sync: {}/{} requesting block {} from {} (out of {} peers)",
		   head.height, header_head.height, next_hash, peer.peer_info.id, highest_height_peers.len());
            self.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
                NetworkRequests::BlockRequest {
                    hash: request.hash,
                    peer_id: peer.peer_info.id.clone(),
                },
            ));
        } else {
            warn!(target: "sync", "Block sync: {}/{} No available {}peers to request block {} from",
		  head.height, header_head.height, if request_from_archival { "archival " } else { "" }, next_hash);
        }

        self.last_request = Some(request);

        Ok(false)
    }
}

pub enum StateSyncResult {
    /// No shard has changed its status
    Unchanged,
    /// At least one shard has changed its status
    /// Boolean parameter specifies whether the client needs to start fetching the block
    Changed(bool),
    /// The state for all shards was downloaded.
    Completed,
}

struct PendingRequestStatus {
    missing_parts: usize,
    wait_until: DateTime<Utc>,
}

impl PendingRequestStatus {
    fn new(timeout: Duration) -> Self {
        Self { missing_parts: 1, wait_until: Clock::utc().add(timeout) }
    }
    fn expired(&self) -> bool {
        Clock::utc() > self.wait_until
    }
}

/// Helper to track state sync.
pub struct StateSync {
    network_adapter: Arc<dyn PeerManagerAdapter>,

    state_sync_time: HashMap<ShardId, DateTime<Utc>>,
    last_time_block_requested: Option<DateTime<Utc>>,

    last_part_id_requested: HashMap<(AccountOrPeerIdOrHash, ShardId), PendingRequestStatus>,
    /// Map from which part we requested to whom.
    requested_target: lru::LruCache<(u64, CryptoHash), AccountOrPeerIdOrHash>,

    timeout: Duration,

    /// Maps shard_id to result of applying downloaded state
    state_parts_apply_results: HashMap<ShardId, Result<(), near_chain_primitives::error::Error>>,

    /// Maps shard_id to result of splitting state for resharding
    split_state_roots: HashMap<ShardId, Result<HashMap<ShardUId, StateRoot>, Error>>,
}

impl StateSync {
    pub fn new(network_adapter: Arc<dyn PeerManagerAdapter>, timeout: TimeDuration) -> Self {
        StateSync {
            network_adapter,
            state_sync_time: Default::default(),
            last_time_block_requested: None,
            last_part_id_requested: Default::default(),
            requested_target: lru::LruCache::new(MAX_PENDING_PART as usize),
            timeout: Duration::from_std(timeout).unwrap(),
            state_parts_apply_results: HashMap::new(),
            split_state_roots: HashMap::new(),
        }
    }

    pub fn sync_block_status(
        &mut self,
        prev_hash: &CryptoHash,
        chain: &mut Chain,
        now: DateTime<Utc>,
    ) -> Result<(bool, bool), near_chain::Error> {
        let (request_block, have_block) = if !chain.block_exists(prev_hash)? {
            match self.last_time_block_requested {
                None => (true, false),
                Some(last_time) => {
                    if now - last_time >= self.timeout {
                        error!(target: "sync", "State sync: block request for {} timed out in {} seconds", prev_hash, self.timeout.num_seconds());
                        (true, false)
                    } else {
                        (false, false)
                    }
                }
            }
        } else {
            self.last_time_block_requested = None;
            (false, true)
        };
        if request_block {
            self.last_time_block_requested = Some(now);
        };
        Ok((request_block, have_block))
    }

    pub fn sync_shards_status(
        &mut self,
        me: &Option<AccountId>,
        sync_hash: CryptoHash,
        new_shard_sync: &mut HashMap<u64, ShardSyncDownload>,
        chain: &mut Chain,
        runtime_adapter: &Arc<dyn RuntimeAdapter>,
        highest_height_peers: &Vec<FullPeerInfo>,
        tracking_shards: Vec<ShardId>,
        now: DateTime<Utc>,
        state_parts_task_scheduler: &dyn Fn(ApplyStatePartsRequest),
        state_split_scheduler: &dyn Fn(StateSplitRequest),
    ) -> Result<(bool, bool), near_chain::Error> {
        let mut all_done = true;
        let mut update_sync_status = false;
        let init_sync_download = ShardSyncDownload {
            downloads: vec![
                DownloadStatus {
                    start_time: now,
                    prev_update_time: now,
                    run_me: Arc::new(AtomicBool::new(true)),
                    error: false,
                    done: false,
                    state_requests_count: 0,
                    last_target: None,
                };
                1
            ],
            status: ShardSyncStatus::StateDownloadHeader,
        };

        let prev_hash = *chain.get_block_header(&sync_hash)?.prev_hash();
        let prev_epoch_id = chain.get_block_header(&prev_hash)?.epoch_id().clone();
        let epoch_id = chain.get_block_header(&sync_hash)?.epoch_id().clone();
        if chain.runtime_adapter.get_shard_layout(&prev_epoch_id)?
            != chain.runtime_adapter.get_shard_layout(&epoch_id)?
        {
            error!("cannot sync to the first epoch after sharding upgrade");
            panic!("cannot sync to the first epoch after sharding upgrade. Please wait for the next epoch or find peers that are more up to date");
        }
        let split_states = runtime_adapter.will_shard_layout_change_next_epoch(&prev_hash)?;

        for shard_id in tracking_shards {
            let mut download_timeout = false;
            let mut need_shard = false;
            let shard_sync_download = new_shard_sync.entry(shard_id).or_insert_with(|| {
                need_shard = true;
                init_sync_download.clone()
            });
            let mut this_done = false;
            match shard_sync_download.status {
                ShardSyncStatus::StateDownloadHeader => {
                    if shard_sync_download.downloads[0].done {
                        let shard_state_header = chain.get_state_header(shard_id, sync_hash)?;
                        let state_num_parts =
                            get_num_state_parts(shard_state_header.state_root_node().memory_usage);
                        *shard_sync_download = ShardSyncDownload {
                            downloads: vec![
                                DownloadStatus {
                                    start_time: now,
                                    prev_update_time: now,
                                    run_me: Arc::new(AtomicBool::new(true)),
                                    error: false,
                                    done: false,
                                    state_requests_count: 0,
                                    last_target: None,
                                };
                                state_num_parts as usize
                            ],
                            status: ShardSyncStatus::StateDownloadParts,
                        };
                        need_shard = true;
                    } else {
                        let prev = shard_sync_download.downloads[0].prev_update_time;
                        let error = shard_sync_download.downloads[0].error;
                        download_timeout = now - prev > self.timeout;
                        if download_timeout || error {
                            shard_sync_download.downloads[0].run_me.store(true, Ordering::SeqCst);
                            shard_sync_download.downloads[0].error = false;
                            shard_sync_download.downloads[0].prev_update_time = now;
                        }
                        if shard_sync_download.downloads[0].run_me.load(Ordering::SeqCst) {
                            need_shard = true;
                        }
                    }
                }
                ShardSyncStatus::StateDownloadParts => {
                    let mut parts_done = true;
                    for part_download in shard_sync_download.downloads.iter_mut() {
                        if !part_download.done {
                            parts_done = false;
                            let prev = part_download.prev_update_time;
                            let error = part_download.error;
                            let part_timeout = now - prev > self.timeout;
                            if part_timeout || error {
                                download_timeout |= part_timeout;
                                part_download.run_me.store(true, Ordering::SeqCst);
                                part_download.error = false;
                                part_download.prev_update_time = now;
                            }
                            if part_download.run_me.load(Ordering::SeqCst) {
                                need_shard = true;
                            }
                        }
                    }
                    if parts_done {
                        update_sync_status = true;
                        *shard_sync_download = ShardSyncDownload {
                            downloads: vec![],
                            status: ShardSyncStatus::StateDownloadScheduling,
                        };
                    }
                }
                ShardSyncStatus::StateDownloadScheduling => {
                    let shard_state_header = chain.get_state_header(shard_id, sync_hash)?;
                    let state_num_parts =
                        get_num_state_parts(shard_state_header.state_root_node().memory_usage);
                    match chain.schedule_apply_state_parts(
                        shard_id,
                        sync_hash,
                        state_num_parts,
                        state_parts_task_scheduler,
                    ) {
                        Ok(()) => {
                            update_sync_status = true;
                            *shard_sync_download = ShardSyncDownload {
                                downloads: vec![],
                                status: ShardSyncStatus::StateDownloadApplying,
                            }
                        }
                        Err(e) => {
                            // Cannot finalize the downloaded state.
                            // The reasonable behavior here is to start from the very beginning.
                            error!(target: "sync", "State sync finalizing error, shard = {}, hash = {}: {:?}", shard_id, sync_hash, e);
                            update_sync_status = true;
                            *shard_sync_download = init_sync_download.clone();
                            chain.clear_downloaded_parts(shard_id, sync_hash, state_num_parts)?;
                        }
                    }
                }
                ShardSyncStatus::StateDownloadApplying => {
                    let result = self.state_parts_apply_results.remove(&shard_id);
                    if let Some(result) = result {
                        match chain.set_state_finalize(shard_id, sync_hash, result) {
                            Ok(()) => {
                                update_sync_status = true;
                                *shard_sync_download = ShardSyncDownload {
                                    downloads: vec![],
                                    status: ShardSyncStatus::StateDownloadComplete,
                                }
                            }
                            Err(e) => {
                                // Cannot finalize the downloaded state.
                                // The reasonable behavior here is to start from the very beginning.
                                error!(target: "sync", "State sync finalizing error, shard = {}, hash = {}: {:?}", shard_id, sync_hash, e);
                                update_sync_status = true;
                                *shard_sync_download = init_sync_download.clone();
                                let shard_state_header =
                                    chain.get_state_header(shard_id, sync_hash)?;
                                let state_num_parts = get_num_state_parts(
                                    shard_state_header.state_root_node().memory_usage,
                                );
                                chain.clear_downloaded_parts(
                                    shard_id,
                                    sync_hash,
                                    state_num_parts,
                                )?;
                            }
                        }
                    }
                }
                ShardSyncStatus::StateDownloadComplete => {
                    let shard_state_header = chain.get_state_header(shard_id, sync_hash)?;
                    let state_num_parts =
                        get_num_state_parts(shard_state_header.state_root_node().memory_usage);
                    chain.clear_downloaded_parts(shard_id, sync_hash, state_num_parts)?;
                    if split_states {
                        *shard_sync_download = ShardSyncDownload {
                            downloads: vec![],
                            status: ShardSyncStatus::StateSplitScheduling,
                        }
                    } else {
                        *shard_sync_download = ShardSyncDownload {
                            downloads: vec![],
                            status: ShardSyncStatus::StateSyncDone,
                        };
                        this_done = true;
                    }
                }
                ShardSyncStatus::StateSplitScheduling => {
                    debug_assert!(split_states);
                    chain.build_state_for_split_shards_preprocessing(
                        &sync_hash,
                        shard_id,
                        state_split_scheduler,
                    )?;
                    debug!(target: "sync", "State sync split scheduled: me {:?}, shard = {}, hash = {}", me, shard_id, sync_hash);
                    *shard_sync_download = ShardSyncDownload {
                        downloads: vec![],
                        status: ShardSyncStatus::StateSplitApplying,
                    };
                }
                ShardSyncStatus::StateSplitApplying => {
                    debug_assert!(split_states);
                    let result = self.split_state_roots.remove(&shard_id);
                    if let Some(state_roots) = result {
                        chain
                            .build_state_for_split_shards_postprocessing(&sync_hash, state_roots)?;
                        *shard_sync_download = ShardSyncDownload {
                            downloads: vec![],
                            status: ShardSyncStatus::StateSyncDone,
                        };
                        this_done = true;
                    }
                }
                ShardSyncStatus::StateSyncDone => {
                    this_done = true;
                }
            }
            all_done &= this_done;

            if download_timeout {
                warn!(target: "sync", "State sync didn't download the state for shard {} in {} seconds, sending StateRequest again", shard_id, self.timeout.num_seconds());
                info!(target: "sync", "State sync status: me {:?}, sync_hash {}, phase {}",
                      me,
                      sync_hash,
                      match shard_sync_download.status {
                          ShardSyncStatus::StateDownloadHeader => format!("{} requests sent {}, last target {:?}",
                                                                          Purple.bold().paint(format!("HEADER")),
                                                                          shard_sync_download.downloads[0].state_requests_count,
                                                                          shard_sync_download.downloads[0].last_target),
                          ShardSyncStatus::StateDownloadParts => { let mut text = "".to_string();
                              for (i, download) in shard_sync_download.downloads.iter().enumerate() {
                                  text.push_str(&format!("[{}: {}, {}, {:?}] ",
                                                         Yellow.bold().paint(i.to_string()),
                                                         download.done,
                                                         download.state_requests_count,
                                                         download.last_target));
                              }
                              format!("{} [{}: is_done, requests sent, last target] {}",
                                      Purple.bold().paint("PARTS"),
                                      Yellow.bold().paint("part_id"),
                                      text)
                          }
                          _ => unreachable!("timeout cannot happen when all state is downloaded"),
                      },
                );
            }

            // Execute syncing for shard `shard_id`
            if need_shard {
                update_sync_status = true;
                *shard_sync_download = self.request_shard(
                    me,
                    shard_id,
                    chain,
                    runtime_adapter,
                    sync_hash,
                    shard_sync_download.clone(),
                    highest_height_peers,
                )?;
            }
        }

        Ok((update_sync_status, all_done))
    }

    pub fn set_apply_result(&mut self, shard_id: ShardId, apply_result: Result<(), Error>) {
        self.state_parts_apply_results.insert(shard_id, apply_result);
    }

    pub fn set_split_result(
        &mut self,
        shard_id: ShardId,
        result: Result<HashMap<ShardUId, StateRoot>, Error>,
    ) {
        self.split_state_roots.insert(shard_id, result);
    }

    /// Find the hash of the first block on the same epoch (and chain) of block with hash `sync_hash`.
    pub fn get_epoch_start_sync_hash(
        chain: &mut Chain,
        sync_hash: &CryptoHash,
    ) -> Result<CryptoHash, near_chain::Error> {
        let mut header = chain.get_block_header(sync_hash)?;
        let mut epoch_id = header.epoch_id().clone();
        let mut hash = *header.hash();
        let mut prev_hash = *header.prev_hash();
        loop {
            if prev_hash == CryptoHash::default() {
                return Ok(hash);
            }
            header = chain.get_block_header(&prev_hash)?;
            if &epoch_id != header.epoch_id() {
                return Ok(hash);
            }
            epoch_id = header.epoch_id().clone();
            hash = *header.hash();
            prev_hash = *header.prev_hash();
        }
    }

    fn sent_request_part(
        &mut self,
        target: AccountOrPeerIdOrHash,
        part_id: u64,
        shard_id: ShardId,
        sync_hash: CryptoHash,
    ) {
        self.requested_target.put((part_id, sync_hash), target.clone());

        let timeout = self.timeout;
        self.last_part_id_requested
            .entry((target, shard_id))
            .and_modify(|pending_request| {
                pending_request.missing_parts += 1;
            })
            .or_insert_with(|| PendingRequestStatus::new(timeout));
    }

    pub fn received_requested_part(
        &mut self,
        part_id: u64,
        shard_id: ShardId,
        sync_hash: CryptoHash,
    ) {
        let key = (part_id, sync_hash);
        if let Some(target) = self.requested_target.get(&key) {
            if self.last_part_id_requested.get_mut(&(target.clone(), shard_id)).map_or(
                false,
                |request| {
                    request.missing_parts = request.missing_parts.saturating_sub(1);
                    request.missing_parts == 0
                },
            ) {
                self.last_part_id_requested.remove(&(target.clone(), shard_id));
            }
        }
    }

    /// Find possible targets to download state from.
    /// Candidates are validators at current epoch and peers at highest height.
    /// Only select candidates that we have no pending request currently ongoing.
    fn possible_targets(
        &mut self,
        me: &Option<AccountId>,
        shard_id: ShardId,
        chain: &mut Chain,
        runtime_adapter: &Arc<dyn RuntimeAdapter>,
        sync_hash: CryptoHash,
        highest_height_peers: &Vec<FullPeerInfo>,
    ) -> Result<Vec<AccountOrPeerIdOrHash>, Error> {
        // Remove candidates from pending list if request expired due to timeout
        self.last_part_id_requested.retain(|_, request| !request.expired());

        let prev_block_hash = chain.get_block_header(&sync_hash)?.prev_hash();
        let epoch_hash = runtime_adapter.get_epoch_id_from_prev_block(prev_block_hash)?;

        Ok(runtime_adapter
            .get_epoch_block_producers_ordered(&epoch_hash, &sync_hash)?
            .iter()
            .filter_map(|(validator_stake, _slashed)| {
                let account_id = validator_stake.account_id();
                if runtime_adapter.cares_about_shard(
                    Some(account_id),
                    prev_block_hash,
                    shard_id,
                    false,
                ) {
                    if me.as_ref().map(|me| me != account_id).unwrap_or(true) {
                        Some(AccountOrPeerIdOrHash::AccountId(account_id.clone()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .chain(highest_height_peers.iter().filter_map(|peer| {
                if peer.chain_info.tracked_shards.contains(&shard_id) {
                    Some(AccountOrPeerIdOrHash::PeerId(peer.peer_info.id.clone()))
                } else {
                    None
                }
            }))
            .filter(|candidate| {
                !self.last_part_id_requested.contains_key(&(candidate.clone(), shard_id))
            })
            .collect::<Vec<_>>())
    }

    /// Returns new ShardSyncDownload if successful, otherwise returns given shard_sync_download
    pub fn request_shard(
        &mut self,
        me: &Option<AccountId>,
        shard_id: ShardId,
        chain: &mut Chain,
        runtime_adapter: &Arc<dyn RuntimeAdapter>,
        sync_hash: CryptoHash,
        shard_sync_download: ShardSyncDownload,
        highest_height_peers: &Vec<FullPeerInfo>,
    ) -> Result<ShardSyncDownload, near_chain::Error> {
        let possible_targets = self.possible_targets(
            me,
            shard_id,
            chain,
            runtime_adapter,
            sync_hash,
            highest_height_peers,
        )?;

        if possible_targets.is_empty() {
            return Ok(shard_sync_download);
        }

        // Downloading strategy starts here
        let mut new_shard_sync_download = shard_sync_download.clone();

        match shard_sync_download.status {
            ShardSyncStatus::StateDownloadHeader => {
                let target = possible_targets.choose(&mut thread_rng()).cloned().unwrap();
                assert!(new_shard_sync_download.downloads[0].run_me.load(Ordering::SeqCst));
                new_shard_sync_download.downloads[0].run_me.store(false, Ordering::SeqCst);
                new_shard_sync_download.downloads[0].state_requests_count += 1;
                new_shard_sync_download.downloads[0].last_target = Some(target.clone());
                let run_me = new_shard_sync_download.downloads[0].run_me.clone();
                near_performance_metrics::actix::spawn(
                    std::any::type_name::<Self>(),
                    self.network_adapter
                        .send(PeerManagerMessageRequest::NetworkRequests(
                            NetworkRequests::StateRequestHeader { shard_id, sync_hash, target },
                        ))
                        .then(move |result| {
                            if let Ok(NetworkResponses::RouteNotFound) =
                                result.map(|f| f.as_network_response())
                            {
                                // Send a StateRequestHeader on the next iteration
                                run_me.store(true, Ordering::SeqCst);
                            }
                            future::ready(())
                        }),
                );
            }
            ShardSyncStatus::StateDownloadParts => {
                let possible_targets_sampler =
                    SamplerLimited::new(possible_targets, MAX_STATE_PART_REQUEST);

                // Iterate over all parts that needs to be requested (i.e. download.run_me is true).
                // Parts are ordered such that its index match its part_id.
                // Finally, for every part that needs to be requested it is selected one peer (target) randomly
                // to request the part from
                for ((part_id, download), target) in new_shard_sync_download
                    .downloads
                    .iter_mut()
                    .enumerate()
                    .filter(|(_, download)| download.run_me.load(Ordering::SeqCst))
                    .zip(possible_targets_sampler)
                {
                    self.sent_request_part(target.clone(), part_id as u64, shard_id, sync_hash);
                    download.run_me.store(false, Ordering::SeqCst);
                    download.state_requests_count += 1;
                    download.last_target = Some(target.clone());
                    let run_me = download.run_me.clone();

                    near_performance_metrics::actix::spawn(
                        std::any::type_name::<Self>(),
                        self.network_adapter
                            .send(PeerManagerMessageRequest::NetworkRequests(
                                NetworkRequests::StateRequestPart {
                                    shard_id,
                                    sync_hash,
                                    part_id: part_id as u64,
                                    target: target.clone(),
                                },
                            ))
                            .then(move |result| {
                                if let Ok(NetworkResponses::RouteNotFound) =
                                    result.map(|f| f.as_network_response())
                                {
                                    // Send a StateRequestPart on the next iteration
                                    run_me.store(true, Ordering::SeqCst);
                                }
                                future::ready(())
                            }),
                    );
                }
            }
            _ => {}
        }

        Ok(new_shard_sync_download)
    }

    pub fn run(
        &mut self,
        me: &Option<AccountId>,
        sync_hash: CryptoHash,
        new_shard_sync: &mut HashMap<u64, ShardSyncDownload>,
        chain: &mut Chain,
        runtime_adapter: &Arc<dyn RuntimeAdapter>,
        highest_height_peers: &Vec<FullPeerInfo>,
        tracking_shards: Vec<ShardId>,
        state_parts_task_scheduler: &dyn Fn(ApplyStatePartsRequest),
        state_split_scheduler: &dyn Fn(StateSplitRequest),
    ) -> Result<StateSyncResult, near_chain::Error> {
        let prev_hash = *chain.get_block_header(&sync_hash)?.prev_hash();
        let now = Clock::utc();

        let (request_block, have_block) = self.sync_block_status(&prev_hash, chain, now)?;

        if tracking_shards.is_empty() {
            // This case is possible if a validator cares about the same shards in the new epoch as
            //    in the previous (or about a subset of them), return success right away

            return if !have_block {
                Ok(StateSyncResult::Changed(request_block))
            } else {
                Ok(StateSyncResult::Completed)
            };
        }

        let (update_sync_status, all_done) = self.sync_shards_status(
            me,
            sync_hash,
            new_shard_sync,
            chain,
            runtime_adapter,
            highest_height_peers,
            tracking_shards,
            now,
            state_parts_task_scheduler,
            state_split_scheduler,
        )?;

        if have_block && all_done {
            self.state_sync_time.clear();
            return Ok(StateSyncResult::Completed);
        }

        Ok(if update_sync_status || request_block {
            StateSyncResult::Changed(request_block)
        } else {
            StateSyncResult::Unchanged
        })
    }
}

/// Create an abstract collection of elements to be shuffled.
/// Each element will appear in the shuffled output exactly `limit` times.
/// Use it as an iterator to access the shuffled collection.
///
/// ```rust,ignore
/// let sampler = SamplerLimited::new(vec![1, 2, 3], 2);
///
/// let res = sampler.collect::<Vec<_>>();
///
/// assert!(res.len() == 6);
/// assert!(res.iter().filter(|v| v == 1).count() == 2);
/// assert!(res.iter().filter(|v| v == 2).count() == 2);
/// assert!(res.iter().filter(|v| v == 3).count() == 2);
/// ```
///
/// Out of the 90 possible values of `res` in the code above on of them is:
///
/// ```
/// vec![1, 2, 1, 3, 3, 2];
/// ```
struct SamplerLimited<T> {
    data: Vec<T>,
    limit: Vec<u64>,
}

impl<T> SamplerLimited<T> {
    fn new(data: Vec<T>, limit: u64) -> Self {
        if limit == 0 {
            Self { data: vec![], limit: vec![] }
        } else {
            let len = data.len();
            Self { data, limit: vec![limit; len] }
        }
    }
}

impl<T: Clone> Iterator for SamplerLimited<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.limit.is_empty() {
            None
        } else {
            let len = self.limit.len();
            let ix = thread_rng().gen_range(0, len);
            self.limit[ix] -= 1;

            if self.limit[ix] == 0 {
                if ix + 1 != len {
                    self.limit[ix] = self.limit[len - 1];
                    self.data.swap(ix, len - 1);
                }

                self.limit.pop();
                self.data.pop()
            } else {
                Some(self.data[ix].clone())
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;
    use std::thread;

    use near_chain::test_utils::{setup, setup_with_validators};
    use near_chain::{ChainGenesis, Provenance};
    use near_crypto::{KeyType, PublicKey};
    use near_network::test_utils::MockPeerManagerAdapter;
    use near_primitives::block::{Approval, Block, GenesisId};
    use near_primitives::network::PeerId;

    use super::*;
    use crate::test_utils::TestEnv;
    use near_network_primitives::types::{PartialEdgeInfo, PeerInfo};
    use near_primitives::merkle::PartialMerkleTree;
    use near_primitives::types::EpochId;
    use near_primitives::validator_signer::InMemoryValidatorSigner;
    use near_primitives::version::PROTOCOL_VERSION;
    use num_rational::Ratio;
    use std::collections::HashSet;

    #[test]
    fn test_get_locator_heights() {
        assert_eq!(get_locator_heights(0), vec![0]);
        assert_eq!(get_locator_heights(1), vec![1, 0]);
        assert_eq!(get_locator_heights(2), vec![2, 0]);
        assert_eq!(get_locator_heights(3), vec![3, 1, 0]);
        assert_eq!(get_locator_heights(10), vec![10, 8, 4, 0]);
        assert_eq!(get_locator_heights(100), vec![100, 98, 94, 86, 70, 38, 0]);
        assert_eq!(
            get_locator_heights(1000),
            vec![1000, 998, 994, 986, 970, 938, 874, 746, 490, 0]
        );
        // Locator is still reasonable size even given large height.
        assert_eq!(
            get_locator_heights(10000),
            vec![10000, 9998, 9994, 9986, 9970, 9938, 9874, 9746, 9490, 8978, 7954, 5906, 1810, 0,]
        );
    }

    /// Starts two chains that fork of genesis and checks that they can sync heaaders to the longest.
    #[test]
    fn test_sync_headers_fork() {
        let mock_adapter = Arc::new(MockPeerManagerAdapter::default());
        let mut header_sync = HeaderSync::new(
            mock_adapter.clone(),
            TimeDuration::from_secs(10),
            TimeDuration::from_secs(2),
            TimeDuration::from_secs(120),
            1_000_000_000,
        );
        let (mut chain, _, signer) = setup();
        for _ in 0..3 {
            let prev = chain.get_block(&chain.head().unwrap().last_block_hash).unwrap();
            let block = Block::empty(prev, &*signer);
            chain
                .process_block(
                    &None,
                    block.into(),
                    Provenance::PRODUCED,
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                )
                .unwrap();
        }
        let (mut chain2, _, signer2) = setup();
        for _ in 0..5 {
            let prev = chain2.get_block(&chain2.head().unwrap().last_block_hash).unwrap();
            let block = Block::empty(prev, &*signer2);
            chain2
                .process_block(
                    &None,
                    block.into(),
                    Provenance::PRODUCED,
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                    &mut |_| {},
                )
                .unwrap();
        }
        let mut sync_status = SyncStatus::NoSync;
        let peer1 = FullPeerInfo {
            peer_info: PeerInfo::random(),
            chain_info: near_network_primitives::types::PeerChainInfoV2 {
                genesis_id: GenesisId {
                    chain_id: "unittest".to_string(),
                    hash: *chain.genesis().hash(),
                },
                height: chain2.head().unwrap().height,
                tracked_shards: vec![],
                archival: false,
            },
            partial_edge_info: PartialEdgeInfo::default(),
        };
        let head = chain.head().unwrap();
        assert!(header_sync
            .run(&mut sync_status, &mut chain, head.height, &vec![peer1.clone()])
            .is_ok());
        assert!(sync_status.is_syncing());
        // Check that it queried last block, and then stepped down to genesis block to find common block with the peer.

        let item = mock_adapter.pop().unwrap().as_network_requests();
        assert_eq!(
            item,
            NetworkRequests::BlockHeadersRequest {
                hashes: [3, 1, 0]
                    .iter()
                    .map(|i| *chain.get_block_by_height(*i).unwrap().hash())
                    .collect(),
                peer_id: peer1.peer_info.id
            }
        );
    }

    /// Sets up `HeaderSync` with particular tolerance for slowness, and makes sure that a peer that
    /// sends headers below the threshold gets banned, and the peer that sends them faster doesn't get
    /// banned.
    /// Also makes sure that if `header_sync_due` is checked more frequently than the `progress_timeout`
    /// the peer doesn't get banned. (specifically, that the expected height downloaded gets properly
    /// adjusted for time passed)
    #[test]
    fn test_slow_header_sync() {
        let network_adapter = Arc::new(MockPeerManagerAdapter::default());
        let highest_height = 1000;

        // Setup header_sync with expectation of 25 headers/second
        let mut header_sync = HeaderSync::new(
            network_adapter.clone(),
            TimeDuration::from_secs(1),
            TimeDuration::from_secs(1),
            TimeDuration::from_secs(3),
            25,
        );

        let set_syncing_peer = |header_sync: &mut HeaderSync| {
            header_sync.syncing_peer = Some(FullPeerInfo {
                peer_info: PeerInfo {
                    id: PeerId::new(PublicKey::empty(KeyType::ED25519)),
                    addr: None,
                    account_id: None,
                },
                chain_info: Default::default(),
                partial_edge_info: Default::default(),
            });
            header_sync.syncing_peer.as_mut().unwrap().chain_info.height = highest_height;
        };
        set_syncing_peer(&mut header_sync);

        let (mut chain, _, signers) = setup_with_validators(
            vec!["test0", "test1", "test2", "test3", "test4"]
                .iter()
                .map(|x| x.parse().unwrap())
                .collect(),
            1,
            1,
            1000,
            100,
        );
        let genesis = chain.get_block(&chain.genesis().hash().clone()).unwrap().clone();

        let mut last_block = &genesis;
        let mut all_blocks = vec![];
        let mut block_merkle_tree = PartialMerkleTree::default();
        for i in 0..61 {
            let current_height = 3 + i * 5;

            let approvals = [None, None, Some("test3"), Some("test4")]
                .iter()
                .map(|account_id| {
                    account_id.map(|account_id| {
                        let signer = InMemoryValidatorSigner::from_seed(
                            account_id.parse().unwrap(),
                            KeyType::ED25519,
                            account_id,
                        );
                        Approval::new(
                            *last_block.hash(),
                            last_block.header().height(),
                            current_height,
                            &signer,
                        )
                        .signature
                    })
                })
                .collect();
            let (epoch_id, next_epoch_id) =
                if last_block.header().prev_hash() == &CryptoHash::default() {
                    (last_block.header().next_epoch_id().clone(), EpochId(*last_block.hash()))
                } else {
                    (
                        last_block.header().epoch_id().clone(),
                        last_block.header().next_epoch_id().clone(),
                    )
                };
            let block = Block::produce(
                PROTOCOL_VERSION,
                PROTOCOL_VERSION,
                last_block.header(),
                current_height,
                last_block.header().block_ordinal() + 1,
                last_block.chunks().iter().cloned().collect(),
                epoch_id,
                next_epoch_id,
                None,
                approvals,
                Ratio::new(0, 1),
                0,
                100,
                Some(0),
                vec![],
                vec![],
                &*signers[3],
                *last_block.header().next_bp_hash(),
                block_merkle_tree.root(),
            );
            block_merkle_tree.insert(*block.hash());

            all_blocks.push(block);

            last_block = &all_blocks[all_blocks.len() - 1];
        }

        let mut last_added_block_ord = 0;
        // First send 30 heights every second for a while and make sure it doesn't get
        // banned
        for _iter in 0..12 {
            let block = &all_blocks[last_added_block_ord];
            let current_height = block.header().height();
            set_syncing_peer(&mut header_sync);
            header_sync.header_sync_due(
                &SyncStatus::HeaderSync { current_height, highest_height },
                &Tip::from_header(block.header()),
                highest_height,
            );

            last_added_block_ord += 3;

            thread::sleep(TimeDuration::from_millis(500));
        }
        // 6 blocks / second is fast enough, we should not have banned the peer
        assert!(network_adapter.requests.read().unwrap().is_empty());

        // Now the same, but only 20 heights / sec
        for _iter in 0..12 {
            let block = &all_blocks[last_added_block_ord];
            let current_height = block.header().height();
            set_syncing_peer(&mut header_sync);
            header_sync.header_sync_due(
                &SyncStatus::HeaderSync { current_height, highest_height },
                &Tip::from_header(block.header()),
                highest_height,
            );

            last_added_block_ord += 2;

            thread::sleep(TimeDuration::from_millis(500));
        }
        // This time the peer should be banned, because 4 blocks/s is not fast enough
        let ban_peer = network_adapter.requests.write().unwrap().pop_back().unwrap();

        if let NetworkRequests::BanPeer { .. } = ban_peer.as_network_requests() {
            /* expected */
        } else {
            assert!(false);
        }
    }

    /// Helper function for block sync tests
    fn collect_hashes_from_network_adapter(
        network_adapter: Arc<MockPeerManagerAdapter>,
    ) -> HashSet<CryptoHash> {
        let mut requested_block_hashes = HashSet::new();
        let mut network_request = network_adapter.requests.write().unwrap();
        while let Some(request) = network_request.pop_back() {
            match request {
                PeerManagerMessageRequest::NetworkRequests(NetworkRequests::BlockRequest {
                    hash,
                    ..
                }) => {
                    requested_block_hashes.insert(hash);
                }
                _ => panic!("unexpected network request {:?}", request),
            }
        }
        requested_block_hashes
    }

    fn create_peer_infos(num_peers: usize) -> Vec<FullPeerInfo> {
        (0..num_peers)
            .map(|_| FullPeerInfo {
                peer_info: PeerInfo {
                    id: PeerId::new(PublicKey::empty(KeyType::ED25519)),
                    addr: None,
                    account_id: None,
                },
                chain_info: Default::default(),
                partial_edge_info: Default::default(),
            })
            .collect()
    }
}
