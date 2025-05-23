use std::{
    collections::BTreeSet,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime},
};

use log::warn;

use crate::{
    backend::decrypt::DecryptWriteBackend,
    blob::BlobId,
    error::RusticResult,
    repofile::indexfile::{IndexFile, IndexPack},
};

pub(super) mod constants {
    use std::time::Duration;

    /// The maximum number of blobs to index before saving the index.
    pub(super) const MAX_COUNT: usize = 50_000;
    /// The maximum age of an index before saving the index.
    pub(super) const MAX_AGE: Duration = Duration::from_secs(300);
}

pub(crate) type SharedIndexer<BE> = Arc<RwLock<Indexer<BE>>>;

/// The `Indexer` is responsible for indexing blobs.
#[derive(Debug)]
pub struct Indexer<BE>
where
    BE: DecryptWriteBackend,
{
    /// The backend to write to.
    be: BE,
    /// The index file.
    file: IndexFile,
    /// The number of blobs indexed.
    count: usize,
    /// The time the indexer was created.
    created: SystemTime,
    /// The set of indexed blob ids.
    indexed: Option<BTreeSet<BlobId>>,
}

impl<BE: DecryptWriteBackend> Indexer<BE> {
    /// Creates a new `Indexer`.
    ///
    /// # Type Parameters
    ///
    /// * `BE` - The backend type.
    ///
    /// # Arguments
    ///
    /// * `be` - The backend to write to.
    pub fn new(be: BE) -> Self {
        Self {
            be,
            file: IndexFile::default(),
            count: 0,
            created: SystemTime::now(),
            indexed: Some(BTreeSet::new()),
        }
    }

    /// Creates a new `Indexer` without an index.
    ///
    /// # Type Parameters
    ///
    /// * `BE` - The backend type.
    ///
    /// # Arguments
    ///
    /// * `be` - The backend to write to.
    pub fn new_unindexed(be: BE) -> Self {
        Self {
            be,
            file: IndexFile::default(),
            count: 0,
            created: SystemTime::now(),
            indexed: None,
        }
    }

    /// Resets the indexer.
    pub fn reset(&mut self) {
        self.file = IndexFile::default();
        self.count = 0;
        self.created = SystemTime::now();
    }

    /// Returns a `SharedIndexer` to use in multiple threads.
    ///
    /// # Type Parameters
    ///
    /// * `BE` - The backend type.
    pub fn into_shared(self) -> SharedIndexer<BE> {
        Arc::new(RwLock::new(self))
    }

    /// Finalizes the `Indexer`.
    ///
    /// # Errors
    ///
    /// * If the index file could not be serialized.
    pub fn finalize(&self) -> RusticResult<()> {
        self.save()
    }

    /// Save file if length of packs and `packs_to_delete` is greater than `0`.
    ///
    /// # Errors
    ///
    /// * If the index file could not be serialized.
    pub fn save(&self) -> RusticResult<()> {
        if (self.file.packs.len() + self.file.packs_to_delete.len()) > 0 {
            _ = self.be.save_file(&self.file)?;
        }
        Ok(())
    }

    /// Adds a pack to the `Indexer`.
    ///
    /// # Arguments
    ///
    /// * `pack` - The pack to add.
    ///
    /// # Errors
    ///
    /// * If the index file could not be serialized.
    pub fn add(&mut self, pack: IndexPack) -> RusticResult<()> {
        self.add_with(pack, false)
    }

    /// Adds a pack to the `Indexer` and removes it from the backend.
    ///
    /// # Arguments
    ///
    /// * `pack` - The pack to add.
    ///
    /// # Errors
    ///
    /// * If the index file could not be serialized.
    pub fn add_remove(&mut self, pack: IndexPack) -> RusticResult<()> {
        self.add_with(pack, true)
    }

    /// Adds a pack to the `Indexer`.
    ///
    /// # Arguments
    ///
    /// * `pack` - The pack to add.
    /// * `delete` - Whether to delete the pack from the backend.
    ///
    /// # Errors
    ///
    /// * If the index file could not be serialized.
    pub fn add_with(&mut self, pack: IndexPack, delete: bool) -> RusticResult<()> {
        self.count += pack.blobs.len();

        if let Some(indexed) = &mut self.indexed {
            for blob in &pack.blobs {
                _ = indexed.insert(blob.id);
            }
        }

        self.file.add(pack, delete);

        // check if IndexFile needs to be saved
        let elapsed = self.created.elapsed().unwrap_or_else(|err| {
            warn!("couldn't get elapsed time from system time: {err:?}");
            Duration::ZERO
        });
        if self.count >= constants::MAX_COUNT || elapsed >= constants::MAX_AGE {
            self.save()?;
            self.reset();
        }
        Ok(())
    }

    /// Returns whether the given id is indexed.
    ///
    /// # Arguments
    ///
    /// * `id` - The id to check.
    pub fn has(&self, id: &BlobId) -> bool {
        self.indexed
            .as_ref()
            .is_some_and(|indexed| indexed.contains(id))
    }
}
