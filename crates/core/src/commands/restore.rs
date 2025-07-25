//! `restore` subcommand

use derive_setters::Setters;
use log::{debug, error, info, trace, warn};

use std::{
    cmp::Ordering,
    collections::BTreeMap,
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Mutex,
};

use chrono::{DateTime, Local, Utc};
use ignore::{DirEntry, WalkBuilder};
use itertools::Itertools;
use rayon::ThreadPoolBuilder;

use crate::{
    backend::{
        FileType, ReadBackend,
        decrypt::DecryptReadBackend,
        local_destination::LocalDestination,
        node::{Node, NodeType},
    },
    error::{ErrorKind, RusticError, RusticResult},
    progress::{Progress, ProgressBars},
    repofile::packfile::PackId,
    repository::{IndexedFull, IndexedTree, Open, Repository},
};

pub(crate) mod constants {
    /// The maximum number of reader threads to use for restoring.
    pub(crate) const MAX_READER_THREADS_NUM: usize = 20;
    /// The maximum size of pack-part which is read at once from the backend.
    /// (needed to limit the memory size used for large backends)
    pub(crate) const LIMIT_PACK_READ: u32 = 40 * 1024 * 1024; // 40 MiB
}

type RestoreInfo = BTreeMap<(PackId, BlobLocation), Vec<FileLocation>>;
type Filenames = Vec<PathBuf>;

#[allow(clippy::struct_excessive_bools)]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[derive(Debug, Copy, Clone, Default, Setters)]
#[setters(into)]
#[non_exhaustive]
/// Options for the `restore` command
pub struct RestoreOptions {
    /// Remove all files/dirs in destination which are not contained in snapshot.
    ///
    /// # Warning
    ///
    /// * Use with care, maybe first try this with `--dry-run`?
    #[cfg_attr(feature = "clap", clap(long))]
    pub delete: bool,

    /// Use numeric ids instead of user/group when restoring uid/gui
    #[cfg_attr(feature = "clap", clap(long))]
    pub numeric_id: bool,

    /// Don't restore ownership (user/group)
    #[cfg_attr(feature = "clap", clap(long, conflicts_with = "numeric_id"))]
    pub no_ownership: bool,

    /// Always read and verify existing files (don't trust correct modification time and file size)
    #[cfg_attr(feature = "clap", clap(long))]
    pub verify_existing: bool,
}

#[derive(Default, Debug, Clone, Copy)]
#[non_exhaustive]
/// Statistics for files or directories
pub struct FileDirStats {
    /// Number of files or directories to restore
    pub restore: u64,
    /// Number of files or directories which are unchanged (determined by date, but not verified)
    pub unchanged: u64,
    /// Number of files or directories which are verified and unchanged
    pub verified: u64,
    /// Number of files or directories which are modified
    pub modify: u64,
    /// Number of additional entries
    pub additional: u64,
}

#[derive(Default, Debug, Clone, Copy)]
#[non_exhaustive]
/// Restore statistics
pub struct RestoreStats {
    /// file statistics
    pub files: FileDirStats,
    /// directory statistics
    pub dirs: FileDirStats,
}

/// Restore the repository to the given destination.
///
/// # Type Parameters
///
/// * `P` - The progress bar type
/// * `S` - The type of the indexed tree
///
/// # Arguments
///
/// * `file_infos` - The restore information
/// * `repo` - The repository to restore
/// * `opts` - The restore options
/// * `node_streamer` - The node streamer to use
/// * `dest` - The destination to restore to
///
/// # Errors
///
/// * If the restore failed.
pub(crate) fn restore_repository<P: ProgressBars, S: IndexedTree>(
    file_infos: RestorePlan,
    repo: &Repository<P, S>,
    opts: RestoreOptions,
    node_streamer: impl Iterator<Item = RusticResult<(PathBuf, Node)>>,
    dest: &LocalDestination,
) -> RusticResult<()> {
    repo.warm_up_wait(file_infos.to_packs().into_iter())?;
    restore_contents(repo, dest, file_infos)?;

    let p = repo.pb.progress_spinner("setting metadata...");
    restore_metadata(node_streamer, opts, dest)?;
    p.finish();

    Ok(())
}

/// Collect restore information, scan existing files, create needed dirs and remove superfluous files
///
/// # Type Parameters
///
/// * `P` - The progress bar type.
/// * `S` - The type of the indexed tree.
///
/// # Arguments
///
/// * `repo` - The repository to restore.
/// * `node_streamer` - The node streamer to use.
/// * `dest` - The destination to restore to.
/// * `dry_run` - If true, don't actually restore anything, but only print out what would be done.
///
/// # Errors
///
/// * If a directory could not be created.
/// * If the restore information could not be collected.
#[allow(clippy::too_many_lines)]
pub(crate) fn collect_and_prepare<P: ProgressBars, S: IndexedFull>(
    repo: &Repository<P, S>,
    opts: RestoreOptions,
    mut node_streamer: impl Iterator<Item = RusticResult<(PathBuf, Node)>>,
    dest: &LocalDestination,
    dry_run: bool,
) -> RusticResult<RestorePlan> {
    let p = repo.pb.progress_spinner("collecting file information...");
    let dest_path = dest.path("");

    let mut stats = RestoreStats::default();
    let mut restore_infos = RestorePlan::default();
    let mut additional_existing = false;
    let mut removed_dir = None;

    let mut process_existing = |entry: &DirEntry| -> RusticResult<_> {
        if entry.depth() == 0 {
            // don't process the root dir which should be existing
            return Ok(());
        }

        debug!("additional {}", entry.path().display());
        if entry.file_type().unwrap().is_dir() {
            stats.dirs.additional += 1;
        } else {
            stats.files.additional += 1;
        }
        match (opts.delete, dry_run, entry.file_type().unwrap().is_dir()) {
            (true, true, true) => {
                info!(
                    "would have removed the additional dir: {}",
                    entry.path().display()
                );
            }
            (true, true, false) => {
                info!(
                    "would have removed the additional file: {}",
                    entry.path().display()
                );
            }
            (true, false, true) => {
                let path = entry.path();
                match &removed_dir {
                    Some(dir) if path.starts_with(dir) => {}
                    _ => match dest.remove_dir(path) {
                        Ok(()) => {
                            removed_dir = Some(path.to_path_buf());
                        }
                        Err(err) => {
                            error!("error removing {}: {err}", path.display());
                        }
                    },
                }
            }
            (true, false, false) => {
                if let Err(err) = dest.remove_file(entry.path()) {
                    error!("error removing {}: {err}", entry.path().display());
                }
            }
            (false, _, _) => {
                additional_existing = true;
            }
        }

        Ok(())
    };

    let mut process_node = |path: &PathBuf, node: &Node, exists: bool| -> RusticResult<_> {
        match node.node_type {
            NodeType::Dir => {
                if exists {
                    stats.dirs.modify += 1;
                    trace!("existing dir {}", path.display());
                } else {
                    stats.dirs.restore += 1;
                    debug!("to restore: {}", path.display());
                    if !dry_run {
                        dest.create_dir(path)
                            .map_err(|err| {
                                RusticError::with_source(
                                    ErrorKind::InputOutput,
                                    "Failed to create the directory `{path}`. Please check the path and try again.",
                                    err
                                )
                                .attach_context("path", path.display().to_string())
                            })?;
                    }
                }
            }
            NodeType::File => {
                // collect blobs needed for restoring
                match (
                    exists,
                    restore_infos.add_file(dest, node, path.clone(), repo, opts.verify_existing)?,
                ) {
                    // Note that exists = false and Existing or Verified can happen if the file is changed between scanning the dir
                    // and calling add_file. So we don't care about exists but trust add_file here.
                    (_, AddFileResult::Existing) => {
                        stats.files.unchanged += 1;
                        trace!("identical file: {}", path.display());
                    }
                    (_, AddFileResult::Verified) => {
                        stats.files.verified += 1;
                        trace!("verified identical file: {}", path.display());
                    }
                    // TODO: The differentiation between files to modify and files to create could be done only by add_file
                    // Currently, add_file never returns Modify, but always New, so we differentiate based on exists
                    (true, AddFileResult::Modify) => {
                        stats.files.modify += 1;
                        debug!("to modify: {}", path.display());
                    }
                    (false, AddFileResult::Modify) => {
                        stats.files.restore += 1;
                        debug!("to restore: {}", path.display());
                    }
                }
            }
            _ => {} // nothing to do for symlink, device, etc.
        }
        Ok(())
    };

    let mut dst_iter = WalkBuilder::new(dest_path)
        .follow_links(false)
        .hidden(false)
        .ignore(false)
        .sort_by_file_path(Path::cmp)
        .build()
        .inspect(|r| {
            if let Err(err) = r {
                error!("Error during collection of files: {err:?}");
            }
        })
        .filter_map(Result::ok);

    let mut next_dst = dst_iter.next();

    let mut next_node = node_streamer.next().transpose()?;

    loop {
        match (&next_dst, &next_node) {
            (None, None) => break,

            (Some(destination), None) => {
                process_existing(destination)?;
                next_dst = dst_iter.next();
            }
            (Some(destination), Some((path, node))) => {
                match destination.path().cmp(&dest.path(path)) {
                    Ordering::Less => {
                        process_existing(destination)?;
                        next_dst = dst_iter.next();
                    }
                    Ordering::Equal => {
                        // process existing node
                        if (node.is_dir() && !destination.file_type().unwrap().is_dir())
                            || (node.is_file() && !destination.metadata().unwrap().is_file())
                            || node.is_special()
                        {
                            // if types do not match, first remove the existing file
                            process_existing(destination)?;
                        }
                        process_node(path, node, true)?;
                        next_dst = dst_iter.next();
                        next_node = node_streamer.next().transpose()?;
                    }
                    Ordering::Greater => {
                        process_node(path, node, false)?;
                        next_node = node_streamer.next().transpose()?;
                    }
                }
            }
            (None, Some((path, node))) => {
                process_node(path, node, false)?;
                next_node = node_streamer.next().transpose()?;
            }
        }
    }

    if additional_existing {
        warn!("Note: additional entries exist in destination");
    }

    restore_infos.stats = stats;
    p.finish();

    Ok(restore_infos)
}

/// Restore the metadata of the files and directories.
///
/// # Arguments
///
/// * `node_streamer` - The node streamer to use
/// * `opts` - The restore options to use
/// * `dest` - The destination to restore to
///
/// # Errors
///
/// * If the restore failed.
fn restore_metadata(
    mut node_streamer: impl Iterator<Item = RusticResult<(PathBuf, Node)>>,
    opts: RestoreOptions,
    dest: &LocalDestination,
) -> RusticResult<()> {
    let mut dir_stack = Vec::new();
    while let Some((path, node)) = node_streamer.next().transpose()? {
        match node.node_type {
            NodeType::Dir => {
                // set metadata for all non-parent paths in stack
                while let Some((stackpath, _)) = dir_stack.last() {
                    if path.starts_with(stackpath) {
                        break;
                    }
                    let (path, node) = dir_stack.pop().unwrap();
                    set_metadata(dest, opts, &path, &node);
                }
                // push current path to the stack
                dir_stack.push((path, node));
            }
            _ => set_metadata(dest, opts, &path, &node),
        }
    }

    // empty dir stack and set metadata
    for (path, node) in dir_stack.into_iter().rev() {
        set_metadata(dest, opts, &path, &node);
    }

    Ok(())
}

/// Set the metadata of the given file or directory.
///
/// # Arguments
///
/// * `dest` - The destination to restore to
/// * `opts` - The restore options to use
/// * `path` - The path of the file or directory
/// * `node` - The node information of the file or directory
///
/// # Errors
///
/// If the metadata could not be set.
// TODO: Return a result here, introduce errors and get rid of logging.
pub(crate) fn set_metadata(
    dest: &LocalDestination,
    opts: RestoreOptions,
    path: &PathBuf,
    node: &Node,
) {
    debug!("setting metadata for {}", path.display());
    dest.create_special(path, node)
        .unwrap_or_else(|_| warn!("restore {}: creating special file failed.", path.display()));
    match (opts.no_ownership, opts.numeric_id) {
        (true, _) => {}
        (false, true) => dest
            .set_uid_gid(path, &node.meta)
            .unwrap_or_else(|_| warn!("restore {}: setting UID/GID failed.", path.display())),
        (false, false) => dest
            .set_user_group(path, &node.meta)
            .unwrap_or_else(|_| warn!("restore {}: setting User/Group failed.", path.display())),
    }
    dest.set_permission(path, node)
        .unwrap_or_else(|_| warn!("restore {}: chmod failed.", path.display()));
    dest.set_extended_attributes(path, &node.meta.extended_attributes)
        .unwrap_or_else(|_| {
            warn!(
                "restore {}: setting extended attributes failed.",
                path.display()
            );
        });
    dest.set_times(path, &node.meta)
        .unwrap_or_else(|_| warn!("restore {}: setting file times failed.", path.display()));
}

/// [`restore_contents`] restores all files contents as described by `file_infos`
/// using the [`DecryptReadBackend`] `be` and writing them into the [`LocalDestination`] `dest`.
///
/// # Type Parameters
///
/// * `P` - The progress bar type.
/// * `S` - The state the repository is in.
///
/// # Arguments
///
/// * `repo` - The repository to restore.
/// * `dest` - The destination to restore to.
/// * `file_infos` - The restore information.
///
/// # Errors
///
/// * If the length of a file could not be set.
/// * If the restore failed.
#[allow(clippy::too_many_lines)]
fn restore_contents<P: ProgressBars, S: Open>(
    repo: &Repository<P, S>,
    dest: &LocalDestination,
    file_infos: RestorePlan,
) -> RusticResult<()> {
    let RestorePlan {
        names: filenames,
        file_lengths,
        r: restore_info,
        restore_size: total_size,
        ..
    } = file_infos;
    let filenames = &filenames;
    let be = repo.dbe();

    // first create needed empty files, as they are not created later.
    for (i, size) in file_lengths.iter().enumerate() {
        if *size == 0 {
            let path = &filenames[i];
            dest.set_length(path, *size).map_err(|err| {
                RusticError::with_source(
                    ErrorKind::InputOutput,
                    "Failed to set the length of the file `{path}`. Please check the path and try again.",
                    err,
                )
                .attach_context("path", path.display().to_string())
            })?;
        }
    }

    let sizes = &Mutex::new(file_lengths);

    let p = repo.pb.progress_bytes("restoring file contents...");
    p.set_length(total_size);

    let blobs: Vec<_> = restore_info
        .into_iter()
        .map(|((pack, bl), fls)| {
            let from_file = fls
                .iter()
                .find(|fl| fl.matches)
                .map(|fl| (fl.file_idx, fl.file_start, bl.data_length()));

            let name_dests: Vec<_> = fls
                .iter()
                .filter(|fl| !fl.matches)
                .map(|fl| (bl.clone(), fl.file_idx, fl.file_start))
                .collect();
            (pack, bl.offset, bl.length, from_file, name_dests)
        })
        // optimize reading from backend by reading many blobs in a row
        .coalesce(|mut x, mut y| {
            if x.0 == y.0 // if the pack is identical
                && x.3.is_none() // and we don't read from a present file
                && y.1 == x.1 + x.2 // and the blobs are contiguous
                // and we don't trespass the limit
                && x.2 + y.2 < constants::LIMIT_PACK_READ
            {
                x.2 += y.2;
                x.4.append(&mut y.4);
                Ok(x)
            } else {
                Err((x, y))
            }
        })
        .collect();

    let threads = constants::MAX_READER_THREADS_NUM;

    let pool = ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|err| {
            RusticError::with_source(
                ErrorKind::Internal,
                "Failed to create the thread pool with `{num_threads}` threads. Please try again.",
                err,
            )
            .attach_context("num_threads", threads.to_string())
        })?;

    pool.in_place_scope(|s| {
        for (pack, offset, length, from_file, name_dests) in blobs {
            let p = &p;

            if !name_dests.is_empty() {
                // TODO: error handling!
                s.spawn(move |s1| {
                    let read_data = match &from_file {
                        Some((file_idx, offset_file, length_file)) => {
                            // read from existing file
                            dest.read_at(&filenames[*file_idx], *offset_file, (*length_file).into())
                                .unwrap()
                        }
                        None => {
                            // read needed part of the pack
                            be.read_partial(FileType::Pack, &pack, false, offset, length)
                                .unwrap()
                        }
                    };

                    // save into needed files in parallel
                    for (bl, group) in &name_dests.into_iter().chunk_by(|item| item.0.clone()) {
                        let size = bl.data_length().into();
                        let data = if from_file.is_some() {
                            read_data.clone()
                        } else {
                            let start = usize::try_from(bl.offset - offset)
                                .expect("convert from u32 to usize should not fail!");
                            let end = usize::try_from(bl.offset + bl.length - offset)
                                .expect("convert from u32 to usize should not fail!");
                            be.read_encrypted_from_partial(
                                &read_data[start..end],
                                bl.uncompressed_length,
                            )
                            .unwrap()
                        };
                        for (_, file_idx, start) in group {
                            let data = data.clone();
                            s1.spawn(move |_| {
                                let path = &filenames[file_idx];
                                // Allocate file if it is not yet allocated
                                let mut sizes_guard = sizes.lock().unwrap();
                                let filesize = sizes_guard[file_idx];
                                if filesize > 0 {
                                    dest.set_length(path, filesize).unwrap();
                                    sizes_guard[file_idx] = 0;
                                }
                                drop(sizes_guard);
                                dest.write_at(path, start, &data).unwrap();
                                p.inc(size);
                            });
                        }
                    }
                });
            }
        }
    });

    p.finish();

    Ok(())
}

/// Information about what will be restored.
///
/// Struct that contains information of file contents grouped by
/// 1) pack ID,
/// 2) blob within this pack
/// 3) the actual files and position of this blob within those
/// 4) Statistical information
#[derive(Debug, Default)]
pub struct RestorePlan {
    /// The names of the files to restore
    names: Filenames,
    /// The length of the files to restore
    file_lengths: Vec<u64>,
    /// The restore information
    r: RestoreInfo,
    /// The total restore size
    pub restore_size: u64,
    /// The total size of matched content, i.e. content with needs no restore.
    pub matched_size: u64,
    /// Statistics about the restore.
    pub stats: RestoreStats,
}

/// `BlobLocation` contains information about a blob within a pack
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct BlobLocation {
    /// The offset of the blob within the pack
    offset: u32,
    /// The length of the blob
    length: u32,
    /// The uncompressed length of the blob
    uncompressed_length: Option<NonZeroU32>,
}

impl BlobLocation {
    /// Get the length of the data contained in this blob
    fn data_length(&self) -> u32 {
        self.uncompressed_length.map_or(
            self.length - 32, // crypto overhead
            NonZeroU32::get,
        )
    }
}

/// [`FileLocation`] contains information about a file within a blob
#[derive(Debug)]
struct FileLocation {
    // TODO: The index of the file within ... ?
    file_idx: usize,
    /// The start of the file within the blob
    file_start: u64,
    /// Whether the file matches the blob
    ///
    /// This indicates that the file exists and these contents are already correct.
    matches: bool,
}

/// [`AddFileResult`] indicates the result of adding a file to [`FileLocation`]
// TODO: Add documentation!
enum AddFileResult {
    Existing,
    Verified,
    Modify,
}

impl RestorePlan {
    /// Add the file to [`FileLocation`] using `index` to get blob information.
    ///
    /// # Type Parameters
    ///
    /// * `P` - The progress bar type.
    /// * `S` - The type of the indexed tree.
    ///
    /// # Arguments
    ///
    /// * `dest` - The destination to restore to.
    /// * `file` - The file to add.
    /// * `name` - The name of the file.
    /// * `repo` - The repository to restore.
    /// * `ignore_mtime` - If true, ignore the modification time of the file.
    ///
    /// # Errors
    ///
    /// * If the file could not be added.
    fn add_file<P, S: IndexedFull>(
        &mut self,
        dest: &LocalDestination,
        file: &Node,
        name: PathBuf,
        repo: &Repository<P, S>,
        ignore_mtime: bool,
    ) -> RusticResult<AddFileResult> {
        let mut open_file = dest.get_matching_file(&name, file.meta.size);

        // Empty files which exists with correct size should always return Ok(Existing)!
        if file.meta.size == 0 {
            if let Some(meta) = open_file
                .as_ref()
                .map(std::fs::File::metadata)
                .transpose()
                .map_err(|err|
                    RusticError::with_source(
                        ErrorKind::InputOutput,
                        "Failed to get the metadata of the file `{path}`. Please check the path and try again.",
                        err
                    )
                    .attach_context("path", name.display().to_string())
                )?
            {
                if meta.len() == 0 {
                    // Empty file exists
                    return Ok(AddFileResult::Existing);
                }
            }
        }

        if !ignore_mtime {
            if let Some(meta) = open_file
                .as_ref()
                .map(std::fs::File::metadata)
                .transpose()
                .map_err(|err|
                    RusticError::with_source(
                        ErrorKind::InputOutput,
                        "Failed to get the metadata of the file `{path}`. Please check the path and try again.",
                        err
                    )
                    .attach_context("path", name.display().to_string())
                )?
            {
                // TODO: This is the same logic as in backend/ignore.rs => consolidate!
                let mtime = meta
                    .modified()
                    .ok()
                    .map(|t| DateTime::<Utc>::from(t).with_timezone(&Local));
                if meta.len() == file.meta.size && mtime == file.meta.mtime {
                    // File exists with fitting mtime => we suspect this file is ok!
                    debug!("file {} exists with suitable size and mtime, accepting it!",name.display());
                    self.matched_size += file.meta.size;
                    return Ok(AddFileResult::Existing);
                }
            }
        }

        let file_idx = self.names.len();
        self.names.push(name);
        let mut file_pos = 0;
        let mut has_unmatched = false;
        for id in file.content.iter().flatten() {
            let ie = repo.get_index_entry(id)?;
            let bl = BlobLocation {
                offset: ie.offset,
                length: ie.length,
                uncompressed_length: ie.uncompressed_length,
            };
            let length: u64 = bl.data_length().into();

            let matches = open_file
                .as_mut()
                .is_some_and(|file| id.blob_matches_reader(length, file));

            let blob_location = self.r.entry((ie.pack, bl)).or_default();
            blob_location.push(FileLocation {
                file_idx,
                file_start: file_pos,
                matches,
            });

            if matches {
                self.matched_size += length;
            } else {
                self.restore_size += length;
                has_unmatched = true;
            }

            file_pos += length;
        }

        self.file_lengths.push(file_pos);

        if !has_unmatched && open_file.is_some() {
            Ok(AddFileResult::Verified)
        } else {
            Ok(AddFileResult::Modify)
        }
    }

    /// Get a list of all pack files needed to perform the restore
    ///
    /// This can be used e.g. to warm-up those pack files before doing the atual restore.
    #[must_use]
    pub fn to_packs(&self) -> Vec<PackId> {
        self.r
            .iter()
            // filter out packs which we need
            .filter(|(_, fls)| fls.iter().all(|fl| !fl.matches))
            .map(|((pack, _), _)| *pack)
            .dedup()
            .collect()
    }
}
