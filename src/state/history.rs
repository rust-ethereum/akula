use crate::{changeset::*, dbutils, dbutils::*, kv::*, models::*, Cursor, Transaction};
use bytes::Bytes;
use ethereum_types::*;

pub async fn get_account_data_as_of<'db: 'tx, 'tx, Tx: Transaction<'db>>(
    tx: &'tx Tx,
    address: Address,
    timestamp: BlockNumber,
) -> anyhow::Result<Option<Bytes<'tx>>> {
    if let Some(v) = find_data_by_history(tx, address, timestamp).await? {
        return Ok(Some(v));
    }

    tx.get(&tables::PlainState, address.as_fixed_bytes().to_vec())
        .await
        .map(|opt| opt.map(From::from))
}

pub async fn get_storage_as_of<'db: 'tx, 'tx, Tx: Transaction<'db>>(
    tx: &'tx Tx,
    address: Address,
    incarnation: Incarnation,
    key: H256,
    block_number: impl Into<BlockNumber>,
) -> anyhow::Result<Option<Bytes<'tx>>> {
    let key = plain_generate_composite_storage_key(address, incarnation, key);
    if let Some(v) = find_storage_by_history(tx, key, block_number.into()).await? {
        return Ok(Some(v));
    }

    tx.get(&tables::PlainState, key.to_vec())
        .await
        .map(|opt| opt.map(From::from))
}

pub async fn find_data_by_history<'db: 'tx, 'tx, Tx: Transaction<'db>>(
    tx: &'tx Tx,
    address: Address,
    block_number: BlockNumber,
) -> anyhow::Result<Option<Bytes<'tx>>> {
    let mut ch = tx.cursor(&tables::AccountHistory).await?;
    if let Some((k, v)) = ch
        .seek(AccountHistory::index_chunk_key(address, block_number).to_vec())
        .await?
    {
        if k.starts_with(address.as_fixed_bytes()) {
            let change_set_block = v.iter().find(|n| *n >= *block_number);

            let data = {
                if let Some(change_set_block) = change_set_block {
                    let data = {
                        let mut c = tx.cursor_dup_sort(&tables::AccountChangeSet).await?;
                        AccountHistory::find(&mut c, BlockNumber(change_set_block), &address)
                            .await?
                    };

                    if let Some(data) = data {
                        data
                    } else {
                        return Ok(None);
                    }
                } else {
                    return Ok(None);
                }
            };

            //restore codehash
            if let Some(mut acc) = Account::decode_for_storage(&*data)? {
                if acc.incarnation.0 > 0 && acc.code_hash == EMPTY_HASH {
                    if let Some(code_hash) = tx
                        .get(
                            &tables::PlainCodeHash,
                            dbutils::plain_generate_storage_prefix(address, acc.incarnation)
                                .to_vec(),
                        )
                        .await?
                    {
                        acc.code_hash = code_hash;
                    }

                    let data = acc.encode_for_storage(false);

                    return Ok(Some(data.into()));
                }
            }

            return Ok(Some(data));
        }
    }

    Ok(None)
}

pub async fn find_storage_by_history<'db: 'tx, 'tx, Tx: Transaction<'db>>(
    tx: &'tx Tx,
    key: PlainCompositeStorageKey,
    timestamp: BlockNumber,
) -> anyhow::Result<Option<Bytes<'tx>>> {
    let mut ch = tx.cursor(&tables::StorageHistory).await?;
    if let Some((k, v)) = ch
        .seek(StorageHistory::index_chunk_key(key, timestamp).to_vec())
        .await?
    {
        if k[..ADDRESS_LENGTH] != key[..ADDRESS_LENGTH]
            || k[ADDRESS_LENGTH..ADDRESS_LENGTH + KECCAK_LENGTH]
                != key[ADDRESS_LENGTH + INCARNATION_LENGTH..]
        {
            return Ok(None);
        }
        let change_set_block = v.iter().find(|n| *n >= *timestamp);

        let data = {
            if let Some(change_set_block) = change_set_block {
                let data = {
                    let mut c = tx.cursor_dup_sort(&tables::StorageChangeSet).await?;
                    find_storage_with_incarnation(&mut c, BlockNumber(change_set_block), &key)
                        .await?
                };

                if let Some(data) = data {
                    data
                } else {
                    return Ok(None);
                }
            } else {
                return Ok(None);
            }
        };

        return Ok(Some(data));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bitmapdb, crypto, kv::traits::MutableKV, state::database::*, MutableTransaction};
    use pin_utils::pin_mut;
    use std::collections::HashMap;
    use tokio_stream::StreamExt;

    fn random_account() -> (Account, Address) {
        let acc = Account {
            balance: rand::random::<u64>().into(),
            ..Default::default()
        };
        let addr = crypto::pubkey_to_address(&crypto::to_pubkey(&crypto::generate_key()));

        (acc, addr)
    }

    #[tokio::test]
    async fn mutation_delete_timestamp() {
        let db = new_mem_database().unwrap();
        let tx = db.begin_mutable().await.unwrap();

        let mut block_writer = PlainStateWriter::new(&tx, 1);

        let empty_account = Account::default();

        let mut addrs = vec![];
        for _ in 0..10 {
            let (acc, addr) = random_account();

            block_writer
                .update_account_data(addr, &empty_account, &acc)
                .await
                .unwrap();
            addrs.push(addr);
        }

        block_writer.write_changesets().await.unwrap();
        block_writer.write_history().await.unwrap();

        let mut cursor = tx.cursor(&tables::AccountChangeSet).await.unwrap();
        let s = cursor.walk(vec![], |_, _| true);

        pin_mut!(s);

        let mut i = 0;
        while s.try_next().await.unwrap().is_some() {
            i += 1;
        }

        assert_eq!(addrs.len(), i);

        let index = bitmapdb::get(
            &tx,
            &tables::AccountHistory,
            &addrs[0].to_fixed_bytes(),
            0,
            u64::MAX,
        )
        .await
        .unwrap();

        assert_eq!(index.iter().next(), Some(1));

        let mut cursor = tx.cursor(&tables::StorageChangeSet).await.unwrap();
        let bn = BlockNumber(1).db_key().to_vec();
        let s = cursor.walk(bn.clone(), |key, _| key.starts_with(&bn));

        pin_mut!(s);

        let mut count = 0;
        while s.try_next().await.unwrap().is_some() {
            count += 1;
        }

        assert_eq!(count, 0, "changeset must be deleted");

        assert_eq!(
            tx.get(&tables::AccountHistory, addrs[0].to_fixed_bytes().to_vec())
                .await
                .unwrap(),
            None,
            "account must be deleted"
        );
    }

    #[tokio::test]
    async fn mutation_commit() {
        let db = new_mem_database().unwrap();
        let tx = db.begin_mutable().await.unwrap();

        let num_of_accounts = 5;
        let num_of_state_keys = 5;

        let (addrs, acc_state, acc_state_storage, acc_history, acc_history_state_storage) =
            generate_accounts_with_storage_and_history(
                &mut PlainStateWriter::new(&tx, 2),
                num_of_accounts,
                num_of_state_keys,
            )
            .await;

        let mut plain_state = tx.cursor(&tables::PlainState).await.unwrap();

        for i in 0..num_of_accounts {
            let address = addrs[i];
            let acc = read_account_data(&tx, address).await.unwrap().unwrap();

            assert_eq!(acc_state[i], acc);

            let index = bitmapdb::get(
                &tx,
                &tables::AccountHistory,
                address.as_bytes(),
                0,
                u64::MAX,
            )
            .await
            .unwrap();

            assert_eq!(index.iter().next().unwrap(), 2);
            assert_eq!(index.len(), 1);

            let prefix = plain_generate_storage_prefix(address, acc.incarnation).to_vec();
            let res_account_storage = plain_state
                .walk(prefix.clone(), |key, _| key.starts_with(&prefix))
                .fold(HashMap::new(), |mut accum, res| {
                    let (k, v) = res.unwrap();
                    accum.insert(
                        H256::from_slice(&k[ADDRESS_LENGTH + 8..]),
                        U256::from_big_endian(&v),
                    );
                    accum
                })
                .await;

            assert_eq!(res_account_storage, acc_state_storage[i]);

            for (&key, &v) in &acc_history_state_storage[i] {
                assert_eq!(
                    v,
                    U256::from_big_endian(
                        &get_storage_as_of(&tx, address, acc.incarnation, key, 1)
                            .await
                            .unwrap()
                            .unwrap()
                    )
                );
            }
        }

        let bn = encode_block_number(2).to_vec();
        let changeset_in_db = tx
            .cursor(&tables::AccountChangeSet)
            .await
            .unwrap()
            .walk(bn.clone(), |key, _| key.starts_with(&bn))
            .map(|res| {
                let (k, v) = res.unwrap();
                AccountHistory::decode(k.into(), v.into()).1
            })
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<AccountChangeSet>();

        let mut expected_changeset = AccountChangeSet::new();
        for i in 0..num_of_accounts {
            let b = acc_history[i].encode_for_storage(true);
            expected_changeset.insert(Change::new(addrs[i], b.into()));
        }

        assert_eq!(changeset_in_db, expected_changeset);

        let bn = encode_block_number(2).to_vec();
        let cs = tx
            .cursor(&tables::StorageChangeSet)
            .await
            .unwrap()
            .walk(bn.clone(), |key, _| key.starts_with(&bn))
            .map(|res| {
                let (k, v) = res.unwrap();
                StorageHistory::decode(k.into(), v.into()).1
            })
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<StorageChangeSet>();

        assert_eq!(cs.len(), num_of_accounts * num_of_state_keys);

        let mut expected_changeset = StorageChangeSet::new();

        for (i, &address) in addrs.iter().enumerate() {
            for j in 0..num_of_state_keys {
                let key = H256::from_slice(&hex::decode(format!("{:0>64}", i * 100 + j)).unwrap());
                let value = value_to_bytes(U256::from(10 + j as u64)).to_vec().into();

                expected_changeset.insert(Change::new(
                    plain_generate_composite_storage_key(address, acc_history[i].incarnation, key),
                    value,
                ));
            }
        }

        assert_eq!(cs, expected_changeset);
    }

    async fn generate_accounts_with_storage_and_history<
        'db: 'tx,
        'tx,
        Tx: MutableTransaction<'db>,
    >(
        block_writer: &mut PlainStateWriter<'db, 'tx, Tx>,
        num_of_accounts: usize,
        num_of_state_keys: usize,
    ) -> (
        Vec<Address>,
        Vec<Account>,
        Vec<HashMap<H256, U256>>,
        Vec<Account>,
        Vec<HashMap<H256, U256>>,
    ) {
        let mut addrs = vec![];
        let mut acc_state = vec![];
        let mut acc_state_storage = vec![];
        let mut acc_history = vec![];
        let mut acc_history_state_storage = vec![];

        for i in 0..num_of_accounts {
            let (acc_history_e, addrs_e) = random_account();
            acc_history.push(acc_history_e);
            addrs.push(addrs_e);

            acc_history[i].balance = 100.into();
            acc_history[i].code_hash =
                H256::from_slice(&hex::decode(format!("{:0>64}", 10 + i)).unwrap());
            // acc_history[i].root = Some(Hash::from_slice(
            //     &hex::decode(format!("{:0>64}", 10 + i)).unwrap(),
            // ));
            acc_history[i].incarnation = Incarnation(i as u64 + 1);

            acc_state.push(acc_history[i].clone());
            acc_state[i].nonce += 1;
            acc_state[i].balance = 200.into();

            acc_state_storage.push(HashMap::new());
            acc_history_state_storage.push(HashMap::new());
            for j in 0..num_of_state_keys {
                let key = H256::from_slice(&hex::decode(format!("{:0>64}", i * 100 + j)).unwrap());
                let new_value = U256::from(j as usize);
                if !new_value.is_zero() {
                    // Empty value is not considered to be present
                    acc_state_storage[i].insert(key, new_value);
                }

                let value = (10 + j as u64).into();
                acc_history_state_storage[i].insert(key, value);
                block_writer
                    .write_account_storage(
                        addrs[i],
                        acc_history[i].incarnation,
                        key,
                        value,
                        new_value,
                    )
                    .await
                    .unwrap();
            }
            block_writer
                .update_account_data(addrs[i], &acc_history[i] /* original */, &acc_state[i])
                .await
                .unwrap();
        }
        block_writer.write_changesets().await.unwrap();
        block_writer.write_history().await.unwrap();

        (
            addrs,
            acc_state,
            acc_state_storage,
            acc_history,
            acc_history_state_storage,
        )
    }
}
