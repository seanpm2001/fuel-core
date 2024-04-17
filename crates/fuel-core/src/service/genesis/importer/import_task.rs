use anyhow::bail;
use fuel_core_chain_config::TableEntry;
use fuel_core_storage::{
    kv_store::StorageColumn,
    structured_storage::TableWithBlueprint,
    transactional::{StorageTransaction, WriteTransaction},
    StorageAsRef,
};

use crate::{
    database::{
        database_description::{off_chain::OffChain, on_chain::OnChain},
        genesis_progress::{GenesisMetadata, GenesisProgressMutate},
        Database,
    },
    service::genesis::{
        progress::ProgressReporter,
        task_manager::{MultiCancellationToken, NotifyCancel},
    },
};

pub trait ImportTable<T>
where
    T: TableWithBlueprint,
{
    fn on_chain(
        &mut self,
        _group: Vec<TableEntry<T>>,
        _tx: &mut StorageTransaction<&mut Database<OnChain>>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn off_chain(
        &mut self,
        _group: Vec<TableEntry<T>>,
        _tx: &mut StorageTransaction<&mut Database<OffChain>>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

pub fn import_entries<T>(
    cancel_token: MultiCancellationToken,
    mut handler: impl ImportTable<T>,
    groups: impl IntoIterator<Item = anyhow::Result<Vec<TableEntry<T>>>>,
    mut on_chain_db: Database<OnChain>,
    mut off_chain_db: Database<OffChain>,
    reporter: ProgressReporter,
) -> anyhow::Result<()>
where
    T: TableWithBlueprint,
{
    let on_chain_last_idx = on_chain_db
        .storage::<GenesisMetadata<OnChain>>()
        .get(T::column().name())?
        .map(|x| x.into_owned());

    let off_chain_last_idx = off_chain_db
        .storage::<GenesisMetadata<OffChain>>()
        .get(T::column().name())?
        .map(|x| x.into_owned());

    let num_groups_handled_by_all_dbs = on_chain_last_idx
        .min(off_chain_last_idx)
        .map(|x| x.saturating_add(1))
        .unwrap_or(0);

    for (index, group) in groups
        .into_iter()
        .enumerate()
        .skip(num_groups_handled_by_all_dbs)
    {
        if cancel_token.is_cancelled() {
            bail!("Import cancelled");
        }
        let group = group?;

        if Some(index) > on_chain_last_idx {
            let mut on_chain_tx = on_chain_db.write_transaction();
            handler.on_chain(group.clone(), &mut on_chain_tx)?;

            GenesisProgressMutate::<OnChain>::update_genesis_progress(
                &mut on_chain_tx,
                T::column().name(),
                index,
            )?;
            on_chain_tx.commit()?;
        }

        if Some(index) > off_chain_last_idx {
            let mut off_chain_tx = off_chain_db.write_transaction();
            handler.off_chain(group, &mut off_chain_tx)?;
            GenesisProgressMutate::<OffChain>::update_genesis_progress(
                &mut off_chain_tx,
                T::column().name(),
                index,
            )?;
            off_chain_tx.commit()?;
        }

        reporter.set_progress(index);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{
        database::{
            database_description::{off_chain::OffChain, DatabaseDescription},
            genesis_progress::{GenesisMetadata, GenesisProgressInspect},
        },
        graphql_api::storage::coins::{OwnedCoinKey, OwnedCoins},
        service::genesis::{
            importer::import_task::import_entries, progress::ProgressReporter,
            task_manager::MultiCancellationToken,
        },
    };
    use std::sync::{Arc, Mutex};

    use anyhow::{anyhow, bail};
    use fuel_core_chain_config::{Groups, Randomize, TableEntry};
    use fuel_core_storage::{
        iter::{BoxedIter, IterDirection, IterableStore},
        kv_store::{KVItem, KeyValueInspect, StorageColumn, Value},
        structured_storage::TableWithBlueprint,
        tables::Coins,
        transactional::{Changes, StorageTransaction},
        Result as StorageResult, StorageAsMut, StorageInspect, StorageMutate,
    };
    use fuel_core_types::{
        entities::coins::coin::{CompressedCoin, CompressedCoinV1},
        fuel_tx::UtxoId,
    };
    use rand::{rngs::StdRng, SeedableRng};

    use crate::{
        combined_database::CombinedDatabase,
        database::{
            database_description::on_chain::OnChain,
            genesis_progress::GenesisProgressMutate, Database,
        },
        state::{in_memory::memory_store::MemoryStore, TransactableStorage},
    };

    use super::ImportTable;

    #[derive(Default, Clone)]
    struct Spy {
        on_chain_called_with: Arc<Mutex<Vec<Vec<TableEntry<Coins>>>>>,
        off_chain_called_with: Arc<Mutex<Vec<Vec<TableEntry<Coins>>>>>,
    }

    impl Spy {
        fn new() -> Self {
            Self {
                on_chain_called_with: Default::default(),
                off_chain_called_with: Default::default(),
            }
        }

        pub fn on_chain_called_with(&self) -> Vec<Vec<TableEntry<Coins>>> {
            self.on_chain_called_with.lock().unwrap().clone()
        }

        pub fn off_chain_called_with(&self) -> Vec<Vec<TableEntry<Coins>>> {
            self.off_chain_called_with.lock().unwrap().clone()
        }

        fn default_importer(
            &self,
        ) -> TestTableImporter<
            fn(
                Vec<TableEntry<Coins>>,
                &mut StorageTransaction<&mut Database<OnChain>>,
            ) -> anyhow::Result<()>,
            fn(
                Vec<TableEntry<Coins>>,
                &mut StorageTransaction<&mut Database<OffChain>>,
            ) -> anyhow::Result<()>,
        > {
            TestTableImporter {
                on_chain: |_, _| Ok(()),
                off_chain: |_, _| Ok(()),
                handler: self.clone(),
            }
        }

        fn custom_importer<OnChainCallback, OffChainCallback>(
            &self,
            on_chain: OnChainCallback,
            off_chain: OffChainCallback,
        ) -> TestTableImporter<OnChainCallback, OffChainCallback>
        where
            OnChainCallback: FnMut(
                Vec<TableEntry<Coins>>,
                &mut StorageTransaction<&mut Database<OnChain>>,
            ) -> anyhow::Result<()>,
            OffChainCallback: FnMut(
                Vec<TableEntry<Coins>>,
                &mut StorageTransaction<&mut Database<OffChain>>,
            ) -> anyhow::Result<()>,
        {
            TestTableImporter {
                on_chain,
                off_chain,
                handler: self.clone(),
            }
        }
    }

    struct TestTableImporter<OnChainCallback, OffChainCallback> {
        on_chain: OnChainCallback,
        off_chain: OffChainCallback,
        handler: Spy,
    }

    impl<OnChainCallback, OffChainCallback> ImportTable<Coins>
        for TestTableImporter<OnChainCallback, OffChainCallback>
    where
        OnChainCallback: FnMut(
            Vec<TableEntry<Coins>>,
            &mut StorageTransaction<&mut Database<OnChain>>,
        ) -> anyhow::Result<()>,
        OffChainCallback: FnMut(
            Vec<TableEntry<Coins>>,
            &mut StorageTransaction<&mut Database<OffChain>>,
        ) -> anyhow::Result<()>,
    {
        fn on_chain(
            &mut self,
            group: Vec<TableEntry<Coins>>,
            tx: &mut StorageTransaction<&mut Database<OnChain>>,
        ) -> anyhow::Result<()> {
            self.handler
                .on_chain_called_with
                .lock()
                .unwrap()
                .push(group.clone());
            (self.on_chain)(group, tx)?;
            Ok(())
        }
        fn off_chain(
            &mut self,
            group: Vec<TableEntry<Coins>>,
            tx: &mut StorageTransaction<&mut Database<OffChain>>,
        ) -> anyhow::Result<()> {
            self.handler
                .off_chain_called_with
                .lock()
                .unwrap()
                .push(group.clone());
            (self.off_chain)(group, tx)?;
            Ok(())
        }
    }

    struct TestData {
        batches: Vec<Vec<TableEntry<Coins>>>,
    }

    impl TestData {
        pub fn new(amount: usize) -> Self {
            let mut rng = StdRng::seed_from_u64(0);
            let batches = std::iter::repeat_with(|| TableEntry::randomize(&mut rng))
                .take(amount)
                .map(|el| vec![el])
                .collect();
            Self { batches }
        }

        pub fn as_unwrapped_groups(
            &self,
            skip_batches: usize,
        ) -> Vec<Vec<TableEntry<Coins>>> {
            self.batches.iter().skip(skip_batches).cloned().collect()
        }

        pub fn as_ok_groups(
            &self,
            skip_batches: usize,
        ) -> Vec<anyhow::Result<Vec<TableEntry<Coins>>>> {
            self.as_unwrapped_groups(skip_batches)
                .into_iter()
                .map(Ok)
                .collect()
        }
    }

    #[test]
    fn will_go_through_all_groups() {
        // given
        let data = TestData::new(3);

        let spy = Spy::default();

        // when
        import_entries(
            MultiCancellationToken::default(),
            spy.default_importer(),
            data.as_ok_groups(0),
            Database::default(),
            Database::default(),
            ProgressReporter::default(),
        )
        .unwrap();

        // then
        assert_eq!(spy.on_chain_called_with(), data.as_unwrapped_groups(0));
        assert_eq!(spy.off_chain_called_with(), data.as_unwrapped_groups(0));
    }

    #[test]
    fn will_skip_one_group() {
        // given
        let data = TestData::new(2);

        let mut db = CombinedDatabase::default();
        let spy = Spy::new();
        GenesisProgressMutate::<OnChain>::update_genesis_progress(
            db.on_chain_mut(),
            Coins::column().name(),
            0,
        )
        .unwrap();
        GenesisProgressMutate::<OffChain>::update_genesis_progress(
            db.off_chain_mut(),
            Coins::column().name(),
            0,
        )
        .unwrap();

        // when
        import_entries(
            MultiCancellationToken::default(),
            spy.default_importer(),
            data.as_ok_groups(0),
            db.on_chain().clone(),
            db.off_chain().clone(),
            ProgressReporter::default(),
        )
        .unwrap();

        // then
        assert_eq!(spy.on_chain_called_with(), data.as_unwrapped_groups(1));
        assert_eq!(spy.off_chain_called_with(), data.as_unwrapped_groups(1));
    }

    fn insert_a_coin(tx: &mut StorageTransaction<&mut Database>, utxo_id: &UtxoId) {
        let coin: CompressedCoin = CompressedCoinV1::default().into();

        tx.storage_as_mut::<Coins>().insert(utxo_id, &coin).unwrap();
    }

    fn insert_an_owned_coin(
        tx: &mut StorageTransaction<&mut Database<OffChain>>,
        key: OwnedCoinKey,
    ) {
        tx.storage_as_mut::<OwnedCoins>().insert(&key, &()).unwrap();
    }

    #[test]
    fn tx_reverted_if_on_chain_handler_fails() {
        // given
        let groups = TestData::new(1);
        let on_chain_db = Database::default();
        let utxo_id = UtxoId::new(Default::default(), 0);

        // when
        let _ = import_entries(
            MultiCancellationToken::default(),
            Spy::default().custom_importer(
                move |_, tx| {
                    insert_a_coin(tx, &utxo_id);
                    bail!("Some error")
                },
                |_, _| Ok(()),
            ),
            groups.as_ok_groups(0),
            on_chain_db.clone(),
            Database::default(),
            ProgressReporter::default(),
        )
        .expect_err("should fail");

        // then
        assert!(!StorageInspect::<Coins>::contains_key(&on_chain_db, &utxo_id).unwrap());
    }

    #[test]
    fn tx_reverted_if_off_chain_handler_fails() {
        // given
        let groups = TestData::new(1);
        let off_chain_db = Database::default();
        let key = [0; std::mem::size_of::<OwnedCoinKey>()];

        // when
        let _ = import_entries(
            MultiCancellationToken::default(),
            Spy::default().custom_importer(
                |_, _| Ok(()),
                move |_, tx| {
                    insert_an_owned_coin(tx, key);
                    bail!("Some error")
                },
            ),
            groups.as_ok_groups(0),
            Database::default(),
            off_chain_db.clone(),
            ProgressReporter::default(),
        )
        .expect_err("should fail");

        // then
        assert!(
            !StorageInspect::<OwnedCoins>::contains_key(&off_chain_db, &key).unwrap()
        );
    }

    #[test]
    fn seeing_an_invalid_group_propagates_the_error() {
        // given
        let groups = Groups::new(vec![Err(anyhow!("Some error"))]);

        // when
        let result = import_entries(
            MultiCancellationToken::default(),
            Spy::default().default_importer(),
            groups,
            Database::default(),
            Database::default(),
            ProgressReporter::default(),
        );

        // then
        assert!(result.is_err());
    }

    #[test]
    fn succesfully_processed_batch_updates_the_genesis_progress() {
        // given
        let data = TestData::new(2);
        let on_chain_db = Database::default();
        let off_chain_db = Database::default();

        // when
        import_entries(
            MultiCancellationToken::default(),
            Spy::default().default_importer(),
            data.as_ok_groups(0),
            on_chain_db.clone(),
            off_chain_db.clone(),
            ProgressReporter::default(),
        )
        .unwrap();

        // then
        assert_eq!(
            GenesisProgressInspect::<OnChain>::genesis_progress(
                &on_chain_db,
                Coins::column().name(),
            ),
            Some(1)
        );
        assert_eq!(
            GenesisProgressInspect::<OffChain>::genesis_progress(
                &off_chain_db,
                Coins::column().name(),
            ),
            Some(1)
        );
    }

    #[tokio::test]
    async fn processing_stops_when_cancelled() {
        // given
        let (tx, rx) = std::sync::mpsc::channel();

        let cancel_token = tokio_util::sync::CancellationToken::new();
        let spy = Spy::default();
        let runner_handle = {
            let cancel_token = MultiCancellationToken::from_single(cancel_token.clone());
            let importer = spy.default_importer();
            std::thread::spawn(move || {
                import_entries(
                    cancel_token,
                    importer,
                    rx,
                    Database::default(),
                    Database::default(),
                    ProgressReporter::default(),
                )
            })
        };

        let data = TestData::new(4);
        let take = 3;
        for group in data.as_ok_groups(0).into_iter().take(take) {
            tx.send(group).unwrap();
        }

        while spy.on_chain_called_with().len() < take {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }

        cancel_token.cancel();

        // when
        tx.send(data.as_ok_groups(0).pop().unwrap()).unwrap();

        // then
        // runner should finish
        drop(tx);
        runner_handle
            .join()
            .unwrap()
            .expect_err("Cancelling is a failure");

        // group after signal is not read
        let inserted_groups = &data.as_unwrapped_groups(0)[..take];
        assert_eq!(spy.on_chain_called_with(), inserted_groups);
    }

    #[derive(Debug)]
    struct BrokenTransactions<DbDesc>
    where
        DbDesc: DatabaseDescription,
    {
        store: MemoryStore<DbDesc>,
    }

    impl<DbDesc> Default for BrokenTransactions<DbDesc>
    where
        DbDesc: DatabaseDescription,
    {
        fn default() -> Self {
            Self {
                store: MemoryStore::default(),
            }
        }
    }

    impl<DbDesc> KeyValueInspect for BrokenTransactions<DbDesc>
    where
        DbDesc: DatabaseDescription,
    {
        type Column = DbDesc::Column;

        fn get(&self, key: &[u8], column: Self::Column) -> StorageResult<Option<Value>> {
            self.store.get(key, column)
        }
    }

    impl<DbDesc> IterableStore for BrokenTransactions<DbDesc>
    where
        DbDesc: DatabaseDescription,
    {
        fn iter_store(
            &self,
            _: Self::Column,
            _: Option<&[u8]>,
            _: Option<&[u8]>,
            _: IterDirection,
        ) -> BoxedIter<KVItem> {
            unimplemented!()
        }
    }

    impl<DbDesc> TransactableStorage<DbDesc::Height> for BrokenTransactions<DbDesc>
    where
        DbDesc: DatabaseDescription,
    {
        fn commit_changes(
            &self,
            _: Option<DbDesc::Height>,
            _: Changes,
        ) -> StorageResult<()> {
            Err(anyhow::anyhow!("I refuse to work!").into())
        }
    }

    #[test_case::test_case(
        Database::new(Arc::new(BrokenTransactions::<OnChain>::default())),
        Database::default()
    ; "broken on chain db")]
    #[test_case::test_case(
        Database::default(),
        Database::new(Arc::new(BrokenTransactions::<OffChain>::default()))
    ; "broken off chain db")]
    fn on_chain_tx_commit_failure_is_propagated(
        on_chain: Database<OnChain>,
        off_chain: Database<OffChain>,
    ) {
        // given
        let groups = TestData::new(1);

        // TODO: check off chain as well
        // when
        let result = import_entries(
            MultiCancellationToken::default(),
            Spy::default().default_importer(),
            groups.as_ok_groups(0),
            on_chain,
            off_chain,
            ProgressReporter::default(),
        );

        // then
        assert!(result.is_err());
    }

    #[test_case::test_case(None, Some(0) ; "on chain reverted at start")]
    #[test_case::test_case(Some(0), None; "off chain reverted at start")]
    #[test_case::test_case(Some(0), Some(1); "on chain reverted")]
    #[test_case::test_case(Some(1), Some(0); "off chain reverted")]
    #[test_case::test_case(Some(3), Some(1); "off chain reverted multiple times")]
    #[test_case::test_case(Some(1), Some(3); "on chain reverted multiple times")]
    fn can_recover_when_both_tx_dont_succeed_together(
        last_on_chain: Option<usize>,
        last_off_chain: Option<usize>,
    ) {
        // given
        // Currently the difference is never going to be more than 1, but if we ever change that
        // `import_entries` should be able to handle it.
        let mut on_chain = Database::<OnChain>::default();
        if let Some(last_on_chain_group_processed) = last_on_chain {
            StorageMutate::<GenesisMetadata<OnChain>>::insert(
                &mut on_chain,
                "Coins",
                &last_on_chain_group_processed,
            )
            .unwrap();
        }

        let mut off_chain = Database::<OffChain>::default();
        if let Some(last_off_chain_group_processed) = last_off_chain {
            StorageMutate::<GenesisMetadata<OffChain>>::insert(
                &mut off_chain,
                "Coins",
                &last_off_chain_group_processed,
            )
            .unwrap();
        }

        let spy = Spy::default();
        let groups = TestData::new(5);

        // when
        import_entries(
            MultiCancellationToken::default(),
            spy.default_importer(),
            groups.as_ok_groups(0),
            on_chain,
            off_chain,
            ProgressReporter::default(),
        )
        .unwrap();

        // then
        let on_chain_imports = spy.on_chain_called_with();
        assert_eq!(
            on_chain_imports,
            groups.as_unwrapped_groups(last_on_chain.map(|x| x + 1).unwrap_or(0))
        );

        let off_chain_imports = spy.off_chain_called_with();
        assert_eq!(
            off_chain_imports,
            groups.as_unwrapped_groups(last_off_chain.map(|x| x + 1).unwrap_or(0))
        );
    }
}
