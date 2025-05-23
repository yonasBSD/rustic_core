<p align="center">
<img src="https://raw.githubusercontent.com/rustic-rs/assets/main/logos/readme_header_core.png" height="400" />
</p>
<p align="center"><b>Library for fast, encrypted, and deduplicated backups</b></p>
<p align="center">
<a href="https://crates.io/crates/rustic_core"><img src="https://img.shields.io/crates/msrv/rustic_core" /></a>
<a href="https://crates.io/crates/rustic_core"><img src="https://img.shields.io/crates/v/rustic_core.svg" /></a>
<a href="https://docs.rs/rustic_core/"><img src="https://img.shields.io/docsrs/rustic_core?style=flat&amp;labelColor=1c1d42&amp;color=4f396a&amp;logo=Rust&amp;logoColor=white" /></a>
<a href="https://github.com/rustic-rs/rustic_core/blob/main/"><img src="https://img.shields.io/badge/license-Apache2.0/MIT-blue.svg" /></a>
<a href="https://crates.io/crates/rustic_core"><img src="https://img.shields.io/crates/d/rustic_core.svg" /></a>
<p>

## About

This library is powering [rustic-rs](https://crates.io/crates/rustic-rs). A
backup tool that provides fast, encrypted, deduplicated backups. It reads and
writes the `restic` repository format, which is described in their design
document.

**Note**: `rustic_core` is in an early development stage and its API is subject
to change in the next releases. If you want to give feedback on that, please
open an [issue](https://github.com/rustic-rs/rustic_core/issues).

## Contact

You can ask questions in the
[Discussions](https://github.com/rustic-rs/rustic/discussions) or have a look at
the [FAQ](https://rustic.cli.rs/docs/FAQ.html).

| Contact       | Where?                                                                                                          |
| ------------- | --------------------------------------------------------------------------------------------------------------- |
| Issue Tracker | [GitHub Issues](https://github.com/rustic-rs/rustic_core/issues/choose)                                         |
| Discord       | [![Discord](https://dcbadge.vercel.app/api/server/WRUWENZnzQ?style=flat-square)](https://discord.gg/WRUWENZnzQ) |
| Discussions   | [GitHub Discussions](https://github.com/rustic-rs/rustic/discussions)                                           |

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
rustic_core = "*"
```

## Crate features

This crate exposes a few features for controlling dependency usage:

- **cli** - Enables support for CLI features by enabling `clap` and `merge`
  features. *This feature is disabled by default*.

- **clap** - Enables a dependency on the `clap` crate and enables parsing from
  the commandline. *This feature is disabled by default*.

- **merge** - Enables support for merging multiple values into one, which
  enables the `conflate` dependency. This is needed for parsing commandline
  arguments and merging them into one (e.g. `config`). *This feature is disabled
  by default*.

- **webdav** - Enables a dependency on the `dav-server` and `futures` crate.
  This enables us to run a WebDAV server asynchronously on the commandline.
  *This feature is disabled by default*.

## Examples

### Example: Initializing a new repository

```rust
use rustic_backend::BackendOptions;
use rustic_core::{ConfigOptions, KeyOptions, Repository, RepositoryOptions};
use simplelog::{Config, LevelFilter, SimpleLogger};
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    // Display info logs
    let _ = SimpleLogger::init(LevelFilter::Info, Config::default());

    // Initialize Backends
    let backends = BackendOptions::default()
        .repository("/tmp/repo")
        .to_backends()?;

    // Init repository
    let repo_opts = RepositoryOptions::default().password("test");
    let key_opts = KeyOptions::default();
    let config_opts = ConfigOptions::default();
    let _repo = Repository::new(&repo_opts, backends)?.init(&key_opts, &config_opts)?;

    // -> use _repo for any operation on an open repository
    Ok(())
}
```

### Example: Creating a new snapshot

```rust
use rustic_backend::BackendOptions;
use rustic_core::{BackupOptions, PathList, Repository, RepositoryOptions, SnapshotOptions};
use simplelog::{Config, LevelFilter, SimpleLogger};
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    // Display info logs
    let _ = SimpleLogger::init(LevelFilter::Info, Config::default());

    // Initialize Backends
    let backends = BackendOptions::default()
        .repository("/tmp/repo")
        .repo_hot("/tmp/repo2")
        .to_backends()?;

    // Open repository
    let repo_opts = RepositoryOptions::default().password("test");

    let repo = Repository::new(&repo_opts, backends)?
        .open()?
        .to_indexed_ids()?;

    let backup_opts = BackupOptions::default();
    let source = PathList::from_string(".")?.sanitize()?;
    let snap = SnapshotOptions::default()
        .add_tags("tag1,tag2")?
        .to_snapshot()?;

    // Create snapshot
    let snap = repo.backup(&backup_opts, &source, snap)?;

    println!("successfully created snapshot:\n{snap:#?}");
    Ok(())
```

### Example: Restoring a snapshot

```rust
use rustic_backend::BackendOptions;
use rustic_core::{LocalDestination, LsOptions, Repository, RepositoryOptions, RestoreOptions};
use simplelog::{Config, LevelFilter, SimpleLogger};
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    // Display info logs
    let _ = SimpleLogger::init(LevelFilter::Info, Config::default());

    // Initialize Backends
    let backends = BackendOptions::default()
        .repository("/tmp/repo")
        .to_backends()?;

    // Open repository
    let repo_opts = RepositoryOptions::default().password("test");
    let repo = Repository::new(&repo_opts, backends)?
        .open()?
        .to_indexed()?;

    // use latest snapshot without filtering snapshots
    let node = repo.node_from_snapshot_path("latest", |_| true)?;

    // use list of the snapshot contents using no additional filtering
    let streamer_opts = LsOptions::default();
    let ls = repo.ls(&node, &streamer_opts)?;

    let destination = "./restore/"; // restore to this destination dir
    let create = true; // create destination dir, if it doesn't exist
    let dest = LocalDestination::new(destination, create, !node.is_dir())?;

    let opts = RestoreOptions::default();
    let dry_run = false;
    // create restore infos. Note: this also already creates needed dirs in the destination
    let restore_infos = repo.prepare_restore(&opts, ls.clone(), &dest, dry_run)?;

    repo.restore(restore_infos, &opts, ls, &dest)?;
    Ok(())
}
```

### Example: Checking a repository

```rust
use rustic_backend::BackendOptions;
use rustic_core::{CheckOptions, Repository, RepositoryOptions};
use simplelog::{Config, LevelFilter, SimpleLogger};
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    // Display info logs
    let _ = SimpleLogger::init(LevelFilter::Info, Config::default());

    // Initialize Backends
    let backends = BackendOptions::default()
        .repository("/tmp/repo")
        .to_backends()?;

    // Open repository
    let repo_opts = RepositoryOptions::default().password("test");
    let repo = Repository::new(&repo_opts, backends)?.open()?;

    // Check repository with standard options but omitting cache checks
    let opts = CheckOptions::default().trust_cache(true);
    repo.check(opts)?;
    Ok(())
}
```

## Contributing

Found a bug?
[Open an issue!](https://github.com/rustic-rs/rustic_core/issues/choose)

Got an idea for an improvement? Don't keep it to yourself!

- [Contribute fixes](https://github.com/rustic-rs/rustic_core/contribute) or new
  features via a pull requests!

Please make sure, that you read the
[contribution guide](https://rustic.cli.rs/docs/contributing-to-rustic.html).

## Minimum Rust version policy

This crate's minimum supported `rustc` version is `1.85.0`.

The current policy is that the minimum Rust version required to use this crate
can be increased in minor version updates. For example, if `crate 1.0` requires
Rust 1.20.0, then `crate 1.0.z` for all values of `z` will also require Rust
1.20.0 or newer. However, `crate 1.y` for `y > 0` may require a newer minimum
version of Rust.

In general, this crate will be conservative with respect to the minimum
supported version of Rust.

## License

Licensed under either of:

- [Apache License, Version 2.0](./LICENSE-APACHE)
- [MIT license](./LICENSE-MIT)
