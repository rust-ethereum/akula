use crate::{
    dbutils::*,
    kv::{
        tables,
        traits::{Cursor, MutableCursor},
    },
    models::*,
    MutableTransaction, Transaction as ReadTransaction,
};
use anyhow::Context;
use ethereum_types::{Address, H256, U256};
use tokio::pin;
use tokio_stream::StreamExt;
use tracing::*;
use BlockHeader as HeaderType;

pub mod canonical_hash {
    use super::*;

    pub async fn read<'db: 'tx, 'tx, Tx: ReadTransaction<'db>>(
        tx: &'tx Tx,
        block_number: impl Into<BlockNumber>,
    ) -> anyhow::Result<Option<H256>> {
        tx.get(&tables::CanonicalHeader, block_number.into()).await
    }

    pub async fn write<'db: 'tx, 'tx, RwTx: MutableTransaction<'db>>(
        tx: &'tx RwTx,
        block_number: impl Into<BlockNumber>,
        hash: H256,
    ) -> anyhow::Result<()> {
        let block_number = block_number.into();

        trace!("Writing canonical hash of {}", block_number);

        let mut cursor = tx.mutable_cursor(&tables::CanonicalHeader).await?;
        cursor.put((block_number, hash)).await.unwrap();

        Ok(())
    }
}

pub mod header_number {
    use super::*;

    pub async fn read<'db: 'tx, 'tx, Tx: ReadTransaction<'db>>(
        tx: &'tx Tx,
        hash: H256,
    ) -> anyhow::Result<Option<BlockNumber>> {
        trace!("Reading block number for hash {:?}", hash);

        tx.get(&tables::HeaderNumber, hash.to_fixed_bytes().into())
            .await
    }
}

pub mod header {
    use super::*;

    pub async fn read<'db: 'tx, 'tx, Tx: ReadTransaction<'db>>(
        tx: &'tx Tx,
        hash: H256,
        number: impl Into<BlockNumber>,
    ) -> anyhow::Result<Option<HeaderType>> {
        let number = number.into();
        trace!("Reading header for block {}/{:?}", number, hash);

        if let Some(b) = tx
            .get(&tables::Header, header_key(number, hash).into())
            .await?
        {
            return Ok(Some(rlp::decode(&b)?));
        }

        Ok(None)
    }
}

pub mod tx {
    use super::*;

    pub async fn read<'db: 'tx, 'tx, Tx: ReadTransaction<'db>>(
        tx: &'tx Tx,
        base_tx_id: u64,
        amount: u32,
    ) -> anyhow::Result<Vec<Transaction>> {
        trace!(
            "Reading {} transactions starting from {}",
            amount,
            base_tx_id
        );

        Ok(if amount > 0 {
            let mut out = Vec::with_capacity(amount as usize);

            let mut cursor = tx.cursor(&tables::BlockTransaction).await?;

            let start_key = base_tx_id.to_be_bytes().to_vec();
            let walker = cursor.walk(Some(start_key), |_| true);

            pin!(walker);

            while let Some((_, tx_rlp)) = walker.try_next().await? {
                out.push(rlp::decode(&tx_rlp).context("broken tx rlp")?);

                if out.len() >= amount as usize {
                    break;
                }
            }

            out
        } else {
            vec![]
        })
    }

    pub async fn write<'db: 'tx, 'tx, RwTx: MutableTransaction<'db>>(
        tx: &'tx RwTx,
        base_tx_id: u64,
        txs: &[Transaction],
    ) -> anyhow::Result<()> {
        trace!(
            "Writing {} transactions starting from {}",
            txs.len(),
            base_tx_id
        );

        let mut cursor = tx.mutable_cursor(&tables::BlockTransaction).await.unwrap();

        for (i, eth_tx) in txs.iter().enumerate() {
            let key = (base_tx_id + i as u64).to_be_bytes().to_vec();
            let data = rlp::encode(eth_tx).to_vec();
            cursor.put((key, data)).await.unwrap();
        }

        Ok(())
    }
}

pub mod tx_sender {
    use super::*;

    pub async fn read<'db: 'tx, 'tx, Tx: ReadTransaction<'db>>(
        tx: &'tx Tx,
        base_tx_id: impl Into<TxIndex>,
        amount: u32,
    ) -> anyhow::Result<Vec<Address>> {
        let base_tx_id = base_tx_id.into();

        trace!(
            "Reading {} transaction senders starting from {}",
            amount,
            base_tx_id
        );

        Ok(if amount > 0 {
            let mut cursor = tx.cursor(&tables::TxSender).await?;

            let start_key = base_tx_id;
            cursor
                .walk(Some(start_key), |_| true)
                .take(amount as usize)
                .collect::<anyhow::Result<Vec<_>>>()
                .await?
                .into_iter()
                .map(|(_, address)| address)
                .collect()
        } else {
            vec![]
        })
    }

    pub async fn write<'db: 'tx, 'tx, RwTx: MutableTransaction<'db>>(
        tx: &'tx RwTx,
        base_tx_id: impl Into<TxIndex>,
        senders: &[Address],
    ) -> anyhow::Result<()> {
        let base_tx_id = base_tx_id.into();
        trace!(
            "Writing {} transaction senders starting from {}",
            senders.len(),
            base_tx_id
        );

        let mut cursor = tx.mutable_cursor(&tables::TxSender).await.unwrap();

        for (i, &sender) in senders.iter().enumerate() {
            cursor
                .put((TxIndex(base_tx_id.0 + i as u64), sender))
                .await
                .unwrap();
        }

        Ok(())
    }
}

pub mod storage_body {
    use bytes::Bytes;

    use super::*;

    async fn read_raw<'db: 'tx, 'tx, Tx: ReadTransaction<'db>>(
        tx: &'tx Tx,
        hash: H256,
        number: impl Into<BlockNumber>,
    ) -> anyhow::Result<Option<Bytes<'tx>>> {
        let number = number.into();
        trace!("Reading storage body for block {}/{:?}", number, hash);

        if let Some(b) = tx
            .get(&tables::BlockBody, header_key(number, hash).into())
            .await?
        {
            return Ok(Some(b.into()));
        }

        Ok(None)
    }

    pub async fn read<'db: 'tx, 'tx, Tx: ReadTransaction<'db>>(
        tx: &'tx Tx,
        hash: H256,
        number: impl Into<BlockNumber>,
    ) -> anyhow::Result<Option<BodyForStorage>> {
        if let Some(b) = read_raw(tx, hash, number).await? {
            return Ok(Some(rlp::decode(&b)?));
        }

        Ok(None)
    }

    pub async fn has<'db: 'tx, 'tx, Tx: ReadTransaction<'db>>(
        tx: &'tx Tx,
        hash: H256,
        number: impl Into<BlockNumber>,
    ) -> anyhow::Result<bool> {
        Ok(read_raw(tx, hash, number).await?.is_some())
    }

    pub async fn write<'db: 'tx, 'tx, RwTx: MutableTransaction<'db>>(
        tx: &'tx RwTx,
        hash: H256,
        number: impl Into<BlockNumber>,
        body: &BodyForStorage,
    ) -> anyhow::Result<()> {
        let number = number.into();
        trace!("Writing storage body for block {}/{:?}", number, hash);

        let data = rlp::encode(body);
        let mut cursor = tx.mutable_cursor(&tables::BlockBody).await.unwrap();
        cursor
            .put((header_key(number, hash).to_vec(), data.to_vec()))
            .await
            .unwrap();

        Ok(())
    }
}

pub mod td {
    use super::*;

    pub async fn read<'db: 'tx, 'tx, Tx: ReadTransaction<'db>>(
        tx: &'tx Tx,
        hash: H256,
        number: impl Into<BlockNumber>,
    ) -> anyhow::Result<Option<U256>> {
        let number = number.into();
        trace!("Reading total difficulty at block {}/{:?}", number, hash);

        if let Some(b) = tx
            .get(
                &tables::HeadersTotalDifficulty,
                header_key(number, hash).into(),
            )
            .await?
        {
            trace!("Reading TD RLP: {}", hex::encode(&b));

            return Ok(Some(rlp::decode(&b)?));
        }

        Ok(None)
    }
}

pub mod tl {
    use super::*;

    pub async fn read<'db: 'tx, 'tx, Tx: ReadTransaction<'db>>(
        tx: &'tx Tx,
        tx_hash: H256,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        trace!("Reading Block number for a tx_hash {:?}", tx_hash);

        if let Some(b) = tx
            .get(
                &tables::BlockTransactionLookup,
                tx_hash.to_fixed_bytes().into(),
            )
            .await?
        {
            trace!("Reading TL RLP: {}", hex::encode(&b));

            return Ok(Some(rlp::decode(&b)?));
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::{new_mem_database, traits::MutableKV};
    use bytes::Bytes;

    #[tokio::test]
    async fn accessors() {
        let tx1 = Transaction {
            message: TransactionMessage::Legacy {
                chain_id: None,
                nonce: 1,
                gas_price: 20_000.into(),
                gas_limit: 3_000_000,
                action: TransactionAction::Create,
                value: 0.into(),
                input: Bytes::new(),
            },
            signature: TransactionSignature::new(false, H256::repeat_byte(2), H256::repeat_byte(3))
                .unwrap(),
        };
        let tx2 = Transaction {
            message: TransactionMessage::Legacy {
                chain_id: None,
                nonce: 2,
                gas_price: 30_000.into(),
                gas_limit: 1_000_000,
                action: TransactionAction::Create,
                value: 10.into(),
                input: Bytes::new(),
            },
            signature: TransactionSignature::new(true, H256::repeat_byte(6), H256::repeat_byte(9))
                .unwrap(),
        };
        let txs = [tx1, tx2];

        let sender1 = Address::random();
        let sender2 = Address::random();
        let senders = [sender1, sender2];

        let block1_hash = H256::random();
        let body = BodyForStorage {
            base_tx_id: 1,
            tx_amount: 2,
            uncles: vec![],
        };

        let db = new_mem_database().unwrap();
        let rwtx = db.begin_mutable().await.unwrap();
        let rwtx = &rwtx;

        storage_body::write(rwtx, block1_hash, 1, &body)
            .await
            .unwrap();
        canonical_hash::write(rwtx, 1, block1_hash).await.unwrap();
        tx::write(rwtx, 1, &txs).await.unwrap();
        tx_sender::write(rwtx, 1, &senders).await.unwrap();

        let recovered_body = storage_body::read(rwtx, block1_hash, 1)
            .await
            .unwrap()
            .expect("Could not recover storage body.");
        let recovered_hash = canonical_hash::read(rwtx, 1)
            .await
            .unwrap()
            .expect("Could not recover block hash");
        let recovered_txs = tx::read(rwtx, 1, 2).await.unwrap();
        let recovered_senders = tx_sender::read(rwtx, 1, 2).await.unwrap();

        assert_eq!(body, recovered_body);
        assert_eq!(block1_hash, recovered_hash);
        assert_eq!(txs, *recovered_txs);
        assert_eq!(senders, *recovered_senders);
    }
}
