//! `repair snapshots` subcommand
use derive_setters::Setters;
use log::{info, warn};

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    backend::{
        decrypt::{DecryptFullBackend, DecryptWriteBackend},
        node::NodeType,
    },
    blob::{
        BlobId, BlobType,
        packer::Packer,
        tree::{Tree, TreeId},
    },
    error::{ErrorKind, RusticError, RusticResult},
    index::{ReadGlobalIndex, ReadIndex, indexer::Indexer},
    progress::ProgressBars,
    repofile::{SnapshotFile, StringList, snapshotfile::SnapshotId},
    repository::{IndexedFull, IndexedTree, Repository},
};

#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[derive(Debug, Setters)]
#[setters(into)]
#[non_exhaustive]
/// Options for the `repair snapshots` command
pub struct RepairSnapshotsOptions {
    /// Also remove defect snapshots
    ///
    /// # Warning
    ///
    /// * This can result in data loss!
    #[cfg_attr(feature = "clap", clap(long))]
    pub delete: bool,

    /// Append this suffix to repaired directory or file name
    #[cfg_attr(
        feature = "clap",
        clap(long, value_name = "SUFFIX", default_value = ".repaired")
    )]
    pub suffix: String,

    /// Tag list to set on repaired snapshots (can be specified multiple times)
    #[cfg_attr(
        feature = "clap",
        clap(long, value_name = "TAG[,TAG,..]", default_value = "repaired")
    )]
    pub tag: Vec<StringList>,
}

impl Default for RepairSnapshotsOptions {
    fn default() -> Self {
        Self {
            delete: true,
            suffix: ".repaired".to_string(),
            tag: vec![StringList(BTreeSet::from(["repaired".to_string()]))],
        }
    }
}

// TODO: add documentation
#[derive(Clone, Copy)]
pub(crate) enum Changed {
    This,
    SubTree,
    None,
}

#[derive(Default)]
pub(crate) struct RepairState {
    replaced: BTreeMap<TreeId, (Changed, TreeId)>,
    seen: BTreeSet<TreeId>,
    delete: Vec<SnapshotId>,
}

/// Runs the `repair snapshots` command
///
/// # Type Parameters
///
/// * `P` - The progress bar type
/// * `S` - The type of the indexed tree.
///
/// # Arguments
///
/// * `repo` - The repository to repair
/// * `opts` - The repair options to use
/// * `snapshots` - The snapshots to repair
/// * `dry_run` - Whether to actually modify the repository or just print what would be done
pub(crate) fn repair_snapshots<P: ProgressBars, S: IndexedFull>(
    repo: &Repository<P, S>,
    opts: &RepairSnapshotsOptions,
    snapshots: Vec<SnapshotFile>,
    dry_run: bool,
) -> RusticResult<()> {
    let be = repo.dbe();
    let config_file = repo.config();

    if opts.delete && config_file.append_only == Some(true) {
        return Err(RusticError::new(
            ErrorKind::AppendOnly,
            "Removing snapshots is not allowed in append-only repositories. Please disable append-only mode first, if you know what you are doing. Aborting.",
        ));
    }

    let mut state = RepairState::default();

    let indexer = Indexer::new(be.clone()).into_shared();
    let mut packer = Packer::new(
        be.clone(),
        BlobType::Tree,
        indexer.clone(),
        config_file,
        repo.index().total_size(BlobType::Tree),
    )?;

    for mut snap in snapshots {
        let snap_id = snap.id;
        info!("processing snapshot {snap_id}");
        match repair_tree(
            repo.dbe(),
            opts,
            repo.index(),
            &mut packer,
            Some(snap.tree),
            &mut state,
            dry_run,
        )? {
            (Changed::None, _) => {
                info!("snapshot {snap_id} is ok.");
            }
            (Changed::This, _) => {
                warn!("snapshot {snap_id}: root tree is damaged -> marking for deletion!");
                state.delete.push(snap_id);
            }
            (Changed::SubTree, id) => {
                // change snapshot tree
                if snap.original.is_none() {
                    snap.original = Some(snap.id);
                }
                _ = snap.set_tags(opts.tag.clone());
                snap.tree = id;
                if dry_run {
                    info!("would have modified snapshot {snap_id}.");
                } else {
                    let new_id = be.save_file(&snap)?;
                    info!("saved modified snapshot as {new_id}.");
                }
                state.delete.push(snap_id);
            }
        }
    }

    if !dry_run {
        _ = packer.finalize()?;
        indexer.write().unwrap().finalize()?;
    }

    if opts.delete {
        if dry_run {
            info!("would have removed {} snapshots.", state.delete.len());
        } else {
            be.delete_list(
                true,
                state.delete.iter(),
                repo.pb.progress_counter("remove defect snapshots"),
            )?;
        }
    }

    Ok(())
}

/// Repairs a tree
///
/// # Type Parameters
///
/// * `BE` - The type of the backend.
///
/// # Arguments
///
/// * `be` - The backend to use
/// * `opts` - The repair options to use
/// * `packer` - The packer to use
/// * `id` - The id of the tree to repair
/// * `replaced` - A map of already replaced trees
/// * `seen` - A set of already seen trees
/// * `dry_run` - Whether to actually modify the repository or just print what would be done
///
/// # Returns
///
/// A tuple containing the change status and the id of the repaired tree
pub(crate) fn repair_tree<BE: DecryptWriteBackend>(
    be: &impl DecryptFullBackend,
    opts: &RepairSnapshotsOptions,
    index: &impl ReadGlobalIndex,
    packer: &mut Packer<BE>,
    id: Option<TreeId>,
    state: &mut RepairState,
    dry_run: bool,
) -> RusticResult<(Changed, TreeId)> {
    let (tree, changed) = match id {
        None => (Tree::new(), Changed::This),
        Some(id) => {
            if state.seen.contains(&id) {
                return Ok((Changed::None, id));
            }
            if let Some(r) = state.replaced.get(&id) {
                return Ok(*r);
            }

            let (tree, mut changed) = Tree::from_backend(be, index, id).map_or_else(
                |err| {
                    warn!("tree {id} could not be loaded: {}", err.display_log());
                    (Tree::new(), Changed::This)
                },
                |tree| (tree, Changed::None),
            );

            let mut new_tree = Tree::new();

            for mut node in tree {
                match node.node_type {
                    NodeType::File => {
                        let mut file_changed = false;
                        let mut new_content = Vec::new();
                        let mut new_size = 0;
                        for blob in node.content.take().unwrap() {
                            index.get_data(&blob).map_or_else(
                                || {
                                    file_changed = true;
                                },
                                |ie| {
                                    new_content.push(blob);
                                    new_size += u64::from(ie.data_length());
                                },
                            );
                        }
                        if file_changed {
                            warn!("file {}: contents are missing", node.name);
                            node.name += &opts.suffix;
                            changed = Changed::SubTree;
                        } else if new_size != node.meta.size {
                            info!("file {}: corrected file size", node.name);
                            changed = Changed::SubTree;
                        }
                        node.content = Some(new_content);
                        node.meta.size = new_size;
                    }
                    NodeType::Dir => {
                        let (c, tree_id) =
                            repair_tree(be, opts, index, packer, node.subtree, state, dry_run)?;
                        match c {
                            Changed::None => {}
                            Changed::This => {
                                warn!("dir {}: tree is missing", node.name);
                                node.subtree = Some(tree_id);
                                node.name += &opts.suffix;
                                changed = Changed::SubTree;
                            }
                            Changed::SubTree => {
                                node.subtree = Some(tree_id);
                                changed = Changed::SubTree;
                            }
                        }
                    }
                    _ => {} // Other types: no check needed
                }
                new_tree.add(node);
            }
            if matches!(changed, Changed::None) {
                _ = state.seen.insert(id);
            }
            (new_tree, changed)
        }
    };

    match (id, changed) {
        (None, Changed::None) => panic!("this should not happen!"),
        (Some(id), Changed::None) => Ok((Changed::None, id)),
        (_, c) => {
            // the tree has been changed => save it
            let (chunk, new_id) = tree.serialize().map_err(|err| {
                RusticError::with_source(ErrorKind::Internal, "Failed to serialize tree.", err)
                    .ask_report()
            })?;

            if !index.has_tree(&new_id) && !dry_run {
                packer.add(chunk.into(), BlobId::from(*new_id))?;
            }

            if let Some(id) = id {
                _ = state.replaced.insert(id, (c, new_id));
            }

            Ok((c, new_id))
        }
    }
}
