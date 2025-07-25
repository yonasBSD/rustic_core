use std::collections::BTreeSet;

use log::trace;
use rayon::prelude::{IntoParallelRefIterator, ParallelBridge, ParallelIterator};

use crate::{
    backend::{decrypt::DecryptWriteBackend, node::NodeType},
    blob::{BlobId, BlobType, packer::Packer, tree::TreeStreamerOnce},
    error::RusticResult,
    index::{ReadIndex, indexer::Indexer},
    progress::{Progress, ProgressBars},
    repofile::SnapshotFile,
    repository::{IndexedFull, IndexedIds, IndexedTree, Open, Repository},
};

/// This struct enhances `[SnapshotFile]` with the attribute `relevant`
/// which indicates if the snapshot is relevant for copying.
#[derive(Debug)]
pub struct CopySnapshot {
    /// The snapshot
    pub sn: SnapshotFile,
    /// Whether it is relevant
    pub relevant: bool,
}

/// Copy the given snapshots to the destination repository.
///
/// # Type Parameters
///
/// * `Q` - The progress bar type.
/// * `R` - The type of the indexed tree.
/// * `P` - The progress bar type.
/// * `S` - The type of the indexed tree.
///
/// # Arguments
///
/// * `repo` - The repository to copy from
/// * `repo_dest` - The repository to copy to
/// * `snapshots` - The snapshots to copy
///
/// # Errors
///
// TODO: Document errors
pub(crate) fn copy<'a, Q, R: IndexedFull, P: ProgressBars, S: IndexedIds>(
    repo: &Repository<Q, R>,
    repo_dest: &Repository<P, S>,
    snapshots: impl IntoIterator<Item = &'a SnapshotFile>,
) -> RusticResult<()> {
    let be_dest = repo_dest.dbe();
    let pb = &repo_dest.pb;

    let (snap_trees, snaps): (Vec<_>, Vec<_>) = snapshots
        .into_iter()
        .cloned()
        .map(|sn| (sn.tree, SnapshotFile::clear_ids(sn)))
        .unzip();

    let be = repo.dbe();
    let index = repo.index();
    let index_dest = repo_dest.index();
    let indexer = Indexer::new(be_dest.clone()).into_shared();

    let data_packer = Packer::new(
        be_dest.clone(),
        BlobType::Data,
        indexer.clone(),
        repo_dest.config(),
        index_dest.total_size(BlobType::Data),
    )?;
    let tree_packer = Packer::new(
        be_dest.clone(),
        BlobType::Tree,
        indexer.clone(),
        repo_dest.config(),
        index_dest.total_size(BlobType::Tree),
    )?;

    let p = pb.progress_bytes("copying blobs...");

    snap_trees
        .par_iter()
        .try_for_each(|id| -> RusticResult<_> {
            trace!("copy tree blob {id}");
            if !index_dest.has_tree(id) {
                let data = index.get_tree(id).unwrap().read_data(be)?;
                p.inc(data.len() as u64);
                tree_packer.add(data, BlobId::from(**id))?;
            }
            Ok(())
        })?;

    let tree_streamer = TreeStreamerOnce::new(be, index, snap_trees, pb.progress_hidden())?;
    tree_streamer
        .par_bridge()
        .try_for_each(|item| -> RusticResult<_> {
            let (_, tree) = item?;
            tree.nodes.par_iter().try_for_each(|node| {
                match node.node_type {
                    NodeType::File => {
                        node.content.par_iter().flatten().try_for_each(
                            |id| -> RusticResult<_> {
                                trace!("copy data blob {id}");
                                if !index_dest.has_data(id) {
                                    let data = index.get_data(id).unwrap().read_data(be)?;
                                    p.inc(data.len() as u64);
                                    data_packer.add(data, BlobId::from(**id))?;
                                }
                                Ok(())
                            },
                        )?;
                    }

                    NodeType::Dir => {
                        let id = node.subtree.unwrap();
                        trace!("copy tree blob {id}");
                        if !index_dest.has_tree(&id) {
                            let data = index.get_tree(&id).unwrap().read_data(be)?;
                            p.inc(data.len() as u64);
                            tree_packer.add(data, BlobId::from(*id))?;
                        }
                    }

                    _ => {} // nothing to copy
                }
                Ok(())
            })
        })?;

    _ = data_packer.finalize()?;
    _ = tree_packer.finalize()?;
    indexer.write().unwrap().finalize()?;

    let p = pb.progress_counter("saving snapshots...");
    be_dest.save_list(snaps.iter(), p)?;
    Ok(())
}

/// Filter out relevant snapshots from the given list of snapshots.
///
/// # Type Parameters
///
/// * `F` - The type of the filter.
/// * `P` - The progress bar type.
/// * `S` - The state of the repository.
///
/// # Arguments
///
/// * `snaps` - The snapshots to filter
/// * `dest_repo` - The destination repository
/// * `filter` - The filter to apply to the snapshots
///
/// # Errors
///
// TODO: Document errors
///
/// # Returns
///
/// A list of snapshots with the attribute `relevant` set to `true` if the snapshot is relevant for copying.
pub(crate) fn relevant_snapshots<F, P: ProgressBars, S: Open>(
    snaps: &[SnapshotFile],
    dest_repo: &Repository<P, S>,
    filter: F,
) -> RusticResult<Vec<CopySnapshot>>
where
    F: FnMut(&SnapshotFile) -> bool,
{
    let p = dest_repo
        .pb
        .progress_counter("finding relevant snapshots...");
    // save snapshots in destination in BTreeSet, as we want to efficiently search within to filter out already existing snapshots before copying.
    let snapshots_dest: BTreeSet<_> =
        SnapshotFile::iter_all_from_backend(dest_repo.dbe(), filter, &p)?.collect();

    let relevant = snaps
        .iter()
        .cloned()
        .map(|sn| CopySnapshot {
            relevant: !snapshots_dest.contains(&sn),
            sn,
        })
        .collect();

    Ok(relevant)
}
