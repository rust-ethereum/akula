#![allow(unreachable_code)]

use crate::{
    consensus::Consensus,
    kv::{mdbx::*, tables},
    models::{BlockHeader, BlockNumber, H256},
    p2p::{
        collections::Graph,
        node::Node,
        types::{BlockId, HeaderRequest, Message, Status},
    },
    stagedsync::{stage::*, stages::HEADERS},
    TaskGuard,
};
use anyhow::format_err;
use async_trait::async_trait;
use dashmap::{mapref::entry::Entry, DashMap};
use ethereum_types::H512;
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use std::{
    hash::Hash,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};
use tokio_stream::StreamExt;
use tracing::*;

const HEADERS_UPPER_BOUND: usize = 1 << 10;

const STAGE_UPPER_BOUND: usize = 3 << 15;
const REQUEST_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct HeaderDownload<E: EnvironmentKind> {
    pub node: Arc<Node<E>>,
    pub consensus: Arc<dyn Consensus>,
    pub max_block: BlockNumber,
    pub graph: Graph,
}

#[async_trait]
impl<'db, E> Stage<'db, E> for HeaderDownload<E>
where
    E: EnvironmentKind,
{
    fn id(&self) -> crate::StageId {
        HEADERS
    }

    async fn execute<'tx>(
        &mut self,
        txn: &'tx mut MdbxTransaction<'db, RW, E>,
        input: StageInput,
    ) -> Result<ExecOutput, StageError>
    where
        'db: 'tx,
    {
        let prev_progress = input.stage_progress.unwrap_or_default();
        if prev_progress != 0 {
            self.update_head(txn, prev_progress).await?;
        }

        let prev_progress_hash = txn
            .get(tables::CanonicalHeader, prev_progress)?
            .ok_or_else(|| {
                StageError::Internal(format_err!("no canonical hash for block #{prev_progress}"))
            })?;

        let mut starting_block: BlockNumber = prev_progress + 1;
        let current_chain_tip = loop {
            let n = self.node.chain_tip.read().0;
            if n > starting_block {
                break n;
            }
            tokio::time::sleep(Self::BACK_OFF).await;
        };

        debug!("Chain tip={}", current_chain_tip);

        let (mut target_block, mut reached_tip) =
            if starting_block + STAGE_UPPER_BOUND > current_chain_tip {
                (current_chain_tip, true)
            } else {
                (starting_block + STAGE_UPPER_BOUND, false)
            };
        if target_block >= self.max_block {
            target_block = self.max_block;
            reached_tip = true;
        }

        let headers_cap = (target_block.0 - starting_block.0) as usize;
        let mut headers = Vec::<(H256, BlockHeader)>::with_capacity(headers_cap);

        while headers.len() < headers_cap {
            if !headers.is_empty() {
                starting_block = headers.last().map(|(_, h)| h).unwrap().number;
            }

            headers.extend(self.download_headers(starting_block, target_block).await?);
            if let Some((_, h)) = headers.first() {
                if h.parent_hash != prev_progress_hash {
                    return Ok(ExecOutput::Unwind {
                        unwind_to: BlockNumber(prev_progress.saturating_sub(1)),
                    });
                }
            }
        }
        let mut stage_progress = prev_progress;

        let mut cursor_header_number = txn.cursor(tables::HeaderNumber)?;
        let mut cursor_header = txn.cursor(tables::Header)?;
        let mut cursor_canonical = txn.cursor(tables::CanonicalHeader)?;
        let mut cursor_td = txn.cursor(tables::HeadersTotalDifficulty)?;
        let mut td = cursor_td.last()?.map(|((_, _), v)| v).unwrap();

        for (hash, header) in headers {
            if header.number == 0 {
                continue;
            }
            if header.number > self.max_block {
                break;
            }

            let block_number = header.number;
            td += header.difficulty;

            cursor_header_number.put(hash, block_number)?;
            cursor_header.put((block_number, hash), header)?;
            cursor_canonical.put(block_number, hash)?;
            cursor_td.put((block_number, hash), td)?;

            stage_progress = block_number;
        }

        Ok(ExecOutput::Progress {
            stage_progress,
            done: true,
            reached_tip,
        })
    }

    async fn unwind<'tx>(
        &mut self,
        txn: &'tx mut MdbxTransaction<'db, RW, E>,
        input: UnwindInput,
    ) -> anyhow::Result<UnwindOutput>
    where
        'db: 'tx,
    {
        self.graph.clear();
        let mut cur = txn.cursor(tables::CanonicalHeader)?;

        if let Some(bad_block) = input.bad_block {
            if let Some((_, hash)) = cur.seek_exact(bad_block)? {
                self.node.mark_bad_block(hash);
            }
        }

        let mut stage_progress = BlockNumber(0);
        while let Some((number, _)) = cur.last()? {
            if number <= input.unwind_to {
                stage_progress = number;
                break;
            }

            cur.delete_current()?;
        }

        Ok(UnwindOutput { stage_progress })
    }
}

#[inline]
fn dummy_check_headers(headers: &[BlockHeader]) -> bool {
    let mut block_num = headers[0].number;
    for header in headers.iter().skip(1) {
        if header.number != block_num + 1 {
            return false;
        }
        block_num += 1u8;
    }
    true
}

#[inline]
fn spin_entry<'a, K: Eq + Hash + Copy, V>(map: &'a DashMap<K, V>, key: K) -> Entry<'a, K, V> {
    loop {
        match map.try_entry(key) {
            Some(entry) => break entry,
            None => continue,
        }
    }
}

impl<E> HeaderDownload<E>
where
    E: EnvironmentKind,
{
    const BACK_OFF: Duration = Duration::from_secs(5);

    fn prepare_requests(
        starting_block: BlockNumber,
        target: BlockNumber,
    ) -> DashMap<BlockNumber, HeaderRequest> {
        assert!(starting_block < target);

        let cap = (target.0 - starting_block.0) as usize / HEADERS_UPPER_BOUND;
        let requests = DashMap::with_capacity(cap + 1);
        for start in (starting_block..target).step_by(HEADERS_UPPER_BOUND) {
            let limit = if start + HEADERS_UPPER_BOUND < target {
                HEADERS_UPPER_BOUND as u64
            } else {
                *target - *start
            };

            let request = HeaderRequest {
                start: BlockId::Number(start),
                limit,
                ..Default::default()
            };
            requests.insert(start, request);
        }
        requests
    }

    pub async fn download_headers(
        &mut self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> anyhow::Result<Vec<(H256, BlockHeader)>> {
        let requests = Arc::new(Self::prepare_requests(start, end));

        let mut stream = self.node.stream_headers().await;

        let is_bounded = |block_number: BlockNumber| block_number >= start && block_number <= end;

        let mut took = Instant::now();

        {
            let _g = TaskGuard(tokio::task::spawn({
                let node = self.node.clone();
                let requests = requests.clone();

                async move {
                    loop {
                        let reqs = requests
                            .iter()
                            .map(|entry| entry.value().clone())
                            .collect::<Vec<_>>();
                        node.clone().send_many_header_requests(reqs).await?;
                        tokio::time::sleep(Self::BACK_OFF).await;
                    }

                    Ok::<_, anyhow::Error>(())
                }
            }));

            let (tx, mut rx) = mpsc::channel::<H512>(128);

            let _guard = TaskGuard(tokio::task::spawn({
                let node = self.node.clone();
                async move {
                    while let Some(penalty) = rx.recv().await {
                        node.penalize_peer(penalty).await?;
                    }

                    Ok::<_, anyhow::Error>(())
                }
            }));

            while !requests.is_empty() {
                if let Some(msg) = stream.next().await {
                    let peer_id = msg.peer_id;

                    if let Message::BlockHeaders(inner) = msg.msg {
                        if inner.headers.is_empty() {
                            continue;
                        }

                        let is_valid = dummy_check_headers(&inner.headers);
                        if is_valid {
                            let num = inner.headers[0].number;
                            let last_hash = inner.headers[inner.headers.len() - 1].hash();

                            if let Entry::Occupied(entry) = spin_entry(&requests, num) {
                                let limit = entry.get().limit as usize;

                                if inner.headers.len() == limit {
                                    entry.remove();
                                    self.graph.extend(inner.headers);
                                }
                            } else if !self.graph.contains(&last_hash) && is_bounded(num) {
                                self.graph.extend(inner.headers);
                            }
                        } else {
                            tx.send(peer_id).await?;
                            continue;
                        }
                    }
                }
            }
        }

        info!(
            "Downloaded {} headers, elapsed={:?}... Starting to build canonical chain...",
            self.graph.len(),
            took.elapsed(),
        );

        took = Instant::now();

        let tail = self.graph.dfs().expect("unreachable");
        let mut headers = self.graph.backtrack(&tail);

        info!(
            "Built canonical chain with={} headers, elapsed={:?}",
            headers.len(),
            took.elapsed()
        );

        let cur_size = headers.len();
        let took = Instant::now();

        self.verify_seal(&mut headers);

        if cur_size == headers.len() {
            info!(
                "Seal verification took={:?} all headers are valid.",
                took.elapsed()
            );
        } else {
            info!(
                "Seal verification took={:?} {} headers are invalidated.",
                took.elapsed(),
                cur_size - headers.len()
            );
        }

        Ok(headers)
    }

    fn verify_seal(&self, headers: &mut Vec<(H256, BlockHeader)>) {
        let valid_till = AtomicUsize::new(0);

        headers.par_iter().enumerate().skip(1).for_each(|(i, _)| {
            if self
                .consensus
                .validate_block_header(&headers[i].1, &headers[i - 1].1, false)
                .is_err()
            {
                let mut value = valid_till.load(Ordering::SeqCst);
                while i < value {
                    if valid_till.compare_exchange(value, i, Ordering::SeqCst, Ordering::SeqCst)
                        == Ok(value)
                    {
                        break;
                    } else {
                        value = valid_till.load(Ordering::SeqCst);
                    }
                }
            }
        });

        let valid_till = valid_till.load(Ordering::SeqCst);
        if valid_till != 0 {
            headers.truncate(valid_till);
        }

        if let Some(index) = self.node.position_bad_block(headers.iter().map(|(h, _)| h)) {
            headers.truncate(index);
        }
    }

    async fn update_head<'tx>(
        &self,
        txn: &'tx mut MdbxTransaction<'_, RW, E>,
        height: BlockNumber,
    ) -> anyhow::Result<()> {
        let hash = txn.get(tables::CanonicalHeader, height)?.unwrap();
        let td = txn
            .get(tables::HeadersTotalDifficulty, (height, hash))?
            .unwrap();
        let status = Status::new(height, hash, td);
        self.node.update_chain_head(Some(status)).await?;

        Ok(())
    }
}
