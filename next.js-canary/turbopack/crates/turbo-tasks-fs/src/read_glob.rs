use anyhow::{Result, bail};
use futures::try_join;
use rustc_hash::FxHashMap;
use turbo_rcstr::RcStr;
use turbo_tasks::{Completion, ResolvedVc, TryJoinIterExt, Vc};

use crate::{DirectoryContent, DirectoryEntry, FileSystem, FileSystemPath, glob::Glob};

#[turbo_tasks::value]
#[derive(Default, Debug)]
pub struct ReadGlobResult {
    pub results: FxHashMap<String, DirectoryEntry>,
    pub inner: FxHashMap<String, ResolvedVc<ReadGlobResult>>,
}

/// Reads matches of a glob pattern.
///
/// DETERMINISM: Result is in random order. Either sort result or do not depend
/// on the order.
#[turbo_tasks::function(fs)]
pub async fn read_glob(directory: FileSystemPath, glob: Vc<Glob>) -> Result<Vc<ReadGlobResult>> {
    read_glob_internal("", directory, glob).await
}

#[turbo_tasks::function(fs)]
async fn read_glob_inner(
    prefix: RcStr,
    directory: FileSystemPath,
    glob: Vc<Glob>,
) -> Result<Vc<ReadGlobResult>> {
    read_glob_internal(&prefix, directory, glob).await
}

// The `prefix` represents the relative directory path where symlinks are not resolve.
async fn read_glob_internal(
    prefix: &str,
    directory: FileSystemPath,
    glob: Vc<Glob>,
) -> Result<Vc<ReadGlobResult>> {
    let dir = directory.read_dir().await?;
    let mut result = ReadGlobResult::default();
    let glob_value = glob.await?;
    match &*dir {
        DirectoryContent::Entries(entries) => {
            for (segment, entry) in entries.iter() {
                // This is redundant with logic inside of `read_dir` but here we track it separately
                // so we don't follow symlinks.
                let entry_path: RcStr = if prefix.is_empty() {
                    segment.clone()
                } else {
                    format!("{prefix}/{segment}").into()
                };
                let entry = resolve_symlink_safely(entry.clone()).await?;
                if glob_value.matches(&entry_path) {
                    result.results.insert(entry_path.to_string(), entry.clone());
                }
                if let DirectoryEntry::Directory(path) = entry
                    && glob_value.can_match_in_directory(&entry_path)
                {
                    result.inner.insert(
                        entry_path.to_string(),
                        read_glob_inner(entry_path, path.clone(), glob)
                            .to_resolved()
                            .await?,
                    );
                }
            }
        }
        DirectoryContent::NotFound => {}
    }
    Ok(ReadGlobResult::cell(result))
}

// Resolve a symlink checking for recursion.
async fn resolve_symlink_safely(entry: DirectoryEntry) -> Result<DirectoryEntry> {
    let resolved_entry = entry.clone().resolve_symlink().await?;
    if resolved_entry != entry && matches!(&resolved_entry, DirectoryEntry::Directory(_)) {
        // We followed a symlink to a directory
        // To prevent an infinite loop, which in the case of turbo-tasks would simply
        // exhaust RAM or go into an infinite loop with the GC we need to check for a
        // recursive symlink, we need to check for recursion.

        // Recursion can only occur if the symlink is a directory and points to an
        // ancestor of the current path, which can be detected via a simple prefix
        // match.
        let source_path = entry.path().unwrap();
        if source_path.is_inside_or_equal(&resolved_entry.clone().path().unwrap()) {
            bail!(
                "'{}' is a symlink causes that causes an infinite loop!",
                source_path.path.to_string()
            )
        }
    }
    Ok(resolved_entry)
}

/// Traverses all directories that match the given `glob`.
///
/// This ensures that the calling task will be invalidated
/// whenever the directories or contents of the directories change,
///  but unlike read_glob doesn't accumulate data.
#[turbo_tasks::function(fs)]
pub async fn track_glob(
    directory: FileSystemPath,
    glob: Vc<Glob>,
    include_dot_files: bool,
) -> Result<Vc<Completion>> {
    track_glob_internal("", directory, glob, include_dot_files).await
}

#[turbo_tasks::function(fs)]
async fn track_glob_inner(
    prefix: RcStr,
    directory: FileSystemPath,
    glob: Vc<Glob>,
    include_dot_files: bool,
) -> Result<Vc<Completion>> {
    track_glob_internal(&prefix, directory, glob, include_dot_files).await
}

async fn track_glob_internal(
    prefix: &str,
    directory: FileSystemPath,
    glob: Vc<Glob>,
    include_dot_files: bool,
) -> Result<Vc<Completion>> {
    let dir = directory.read_dir().await?;
    let glob_value = glob.await?;
    let fs = directory.fs().to_resolved().await?;
    let mut reads = Vec::new();
    let mut completions = Vec::new();
    let mut types = Vec::new();
    match &*dir {
        DirectoryContent::Entries(entries) => {
            for (segment, entry) in entries.iter() {
                if !include_dot_files && segment.starts_with('.') {
                    continue;
                }
                // This is redundant with logic inside of `read_dir` but here we track it separately
                // so we don't follow symlinks.
                let entry_path = if prefix.is_empty() {
                    segment.clone()
                } else {
                    format!("{prefix}/{segment}").into()
                };

                match resolve_symlink_safely(entry.clone()).await? {
                    DirectoryEntry::Directory(path) => {
                        if glob_value.can_match_in_directory(&entry_path) {
                            completions.push(track_glob_inner(
                                entry_path,
                                path.clone(),
                                glob,
                                include_dot_files,
                            ));
                        }
                    }
                    DirectoryEntry::File(path) => {
                        if glob_value.matches(&entry_path) {
                            reads.push(fs.read(path.clone()))
                        }
                    }
                    DirectoryEntry::Symlink(symlink_path) => unreachable!(
                        "resolve_symlink_safely() should have resolved all symlinks, but found \
                         unresolved symlink at path: '{}'. Found path: '{}'. Please report this \
                         as a bug.",
                        entry_path, symlink_path
                    ),
                    DirectoryEntry::Other(path) => {
                        if glob_value.matches(&entry_path) {
                            types.push(path.get_type())
                        }
                    }
                    DirectoryEntry::Error => {}
                }
            }
        }
        DirectoryContent::NotFound => {}
    }
    try_join!(
        reads.iter().try_join(),
        types.iter().try_join(),
        completions.iter().try_join()
    )?;
    Ok(Completion::new())
}

#[cfg(test)]
pub mod tests {

    use std::{
        fs::{File, create_dir},
        io::prelude::*,
    };

    use turbo_rcstr::{RcStr, rcstr};
    use turbo_tasks::{Completion, ReadRef, Vc, apply_effects};
    use turbo_tasks_backend::{BackendOptions, TurboTasksBackend, noop_backing_storage};

    use crate::{
        DirectoryEntry, DiskFileSystem, FileContent, FileSystem, FileSystemPath, glob::Glob,
    };

    #[tokio::test]
    async fn read_glob_basic() {
        crate::register();
        let scratch = tempfile::tempdir().unwrap();
        {
            // Create a simple directory with 2 files, a subdirectory and a dotfile
            let path = scratch.path();
            File::create_new(path.join("foo"))
                .unwrap()
                .write_all(b"foo")
                .unwrap();
            create_dir(path.join("sub")).unwrap();
            File::create_new(path.join("sub/bar"))
                .unwrap()
                .write_all(b"bar")
                .unwrap();
        }
        let tt = turbo_tasks::TurboTasks::new(TurboTasksBackend::new(
            BackendOptions::default(),
            noop_backing_storage(),
        ));
        let path: RcStr = scratch.path().to_str().unwrap().into();
        tt.run_once(async {
            let fs = Vc::upcast::<Box<dyn FileSystem>>(DiskFileSystem::new(rcstr!("temp"), path));
            let read_dir = fs
                .root()
                .await?
                .read_glob(Glob::new(rcstr!("**")))
                .await
                .unwrap();
            assert_eq!(read_dir.results.len(), 2);
            assert_eq!(
                read_dir.results.get("foo"),
                Some(&DirectoryEntry::File(fs.root().await?.join("foo")?))
            );
            assert_eq!(
                read_dir.results.get("sub"),
                Some(&DirectoryEntry::Directory(fs.root().await?.join("sub")?))
            );
            assert_eq!(read_dir.inner.len(), 1);
            let inner = &*read_dir.inner.get("sub").unwrap().await?;
            assert_eq!(inner.results.len(), 1);
            assert_eq!(
                inner.results.get("sub/bar"),
                Some(&DirectoryEntry::File(fs.root().await?.join("sub/bar")?))
            );
            assert_eq!(inner.inner.len(), 0);

            // Now with a more specific pattern
            let read_dir = fs
                .root()
                .await?
                .read_glob(Glob::new(rcstr!("**/bar")))
                .await
                .unwrap();
            assert_eq!(read_dir.results.len(), 0);
            assert_eq!(read_dir.inner.len(), 1);
            let inner = &*read_dir.inner.get("sub").unwrap().await?;
            assert_eq!(inner.results.len(), 1);
            assert_eq!(
                inner.results.get("sub/bar"),
                Some(&DirectoryEntry::File(fs.root().await?.join("sub/bar")?))
            );
            assert_eq!(inner.inner.len(), 0);

            anyhow::Ok(())
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_glob_symlinks() {
        crate::register();
        let scratch = tempfile::tempdir().unwrap();
        {
            use std::os::unix::fs::symlink;

            // Create a simple directory with 1 file and a symlink pointing at at a file in a
            // subdirectory
            let path = scratch.path();
            create_dir(path.join("sub")).unwrap();
            let foo = path.join("sub/foo.js");
            File::create_new(&foo).unwrap().write_all(b"foo").unwrap();
            symlink(&foo, path.join("link.js")).unwrap();
        }
        let tt = turbo_tasks::TurboTasks::new(TurboTasksBackend::new(
            BackendOptions::default(),
            noop_backing_storage(),
        ));
        let path: RcStr = scratch.path().to_str().unwrap().into();
        tt.run_once(async {
            let fs = Vc::upcast::<Box<dyn FileSystem>>(DiskFileSystem::new(rcstr!("temp"), path));
            let read_dir = fs
                .root()
                .await?
                .read_glob(Glob::new(rcstr!("*.js")))
                .await
                .unwrap();
            assert_eq!(read_dir.results.len(), 1);
            assert_eq!(
                read_dir.results.get("link.js"),
                Some(&DirectoryEntry::File(fs.root().await?.join("sub/foo.js")?))
            );
            assert_eq!(read_dir.inner.len(), 0);

            anyhow::Ok(())
        })
        .await
        .unwrap();
    }

    #[turbo_tasks::function(operation)]
    pub async fn delete(path: FileSystemPath) -> anyhow::Result<()> {
        path.write(FileContent::NotFound.cell()).await?;
        Ok(())
    }

    #[turbo_tasks::function(operation)]
    pub async fn write(path: FileSystemPath, contents: RcStr) -> anyhow::Result<()> {
        path.write(
            FileContent::Content(crate::File::from_bytes(contents.to_string().into_bytes())).cell(),
        )
        .await?;
        Ok(())
    }

    #[turbo_tasks::function(operation)]
    pub fn track_star_star_glob(path: FileSystemPath) -> Vc<Completion> {
        path.track_glob(Glob::new(rcstr!("**")), false)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn track_glob_invalidations() {
        use std::os::unix::fs::symlink;
        crate::register();
        let scratch = tempfile::tempdir().unwrap();

        // Create a simple directory with 2 files, a subdirectory and a dotfile
        let path = scratch.path();
        let dir = path.join("dir");
        create_dir(&dir).unwrap();
        File::create_new(dir.join("foo"))
            .unwrap()
            .write_all(b"foo")
            .unwrap();
        create_dir(dir.join("sub")).unwrap();
        File::create_new(dir.join("sub/bar"))
            .unwrap()
            .write_all(b"bar")
            .unwrap();
        // Add a dotfile
        create_dir(dir.join("sub/.vim")).unwrap();
        let gitignore = dir.join("sub/.vim/.gitignore");
        File::create_new(&gitignore)
            .unwrap()
            .write_all(b"ignore")
            .unwrap();
        // put a link in the dir that points at a file in the root.
        let link_target = path.join("link_target.js");
        File::create_new(&link_target)
            .unwrap()
            .write_all(b"link_target")
            .unwrap();
        symlink(&link_target, dir.join("link.js")).unwrap();

        let tt = turbo_tasks::TurboTasks::new(TurboTasksBackend::new(
            BackendOptions::default(),
            noop_backing_storage(),
        ));
        let path: RcStr = scratch.path().to_str().unwrap().into();
        tt.run_once(async {
            let fs = Vc::upcast::<Box<dyn FileSystem>>(DiskFileSystem::new(rcstr!("temp"), path));
            let dir = fs.root().await?.join("dir")?;
            let read_dir = track_star_star_glob(dir.clone())
                .read_strongly_consistent()
                .await?;

            // Delete a file that we shouldn't be tracking
            let delete_result = delete(fs.root().await?.join("dir/sub/.vim/.gitignore")?);
            delete_result.read_strongly_consistent().await?;
            apply_effects(delete_result).await?;

            let read_dir2 = track_star_star_glob(dir.clone())
                .read_strongly_consistent()
                .await?;
            assert!(ReadRef::ptr_eq(&read_dir, &read_dir2));

            // Delete a file that we should be tracking
            let delete_result = delete(fs.root().await?.join("dir/foo")?);
            delete_result.read_strongly_consistent().await?;
            apply_effects(delete_result).await?;

            let read_dir2 = track_star_star_glob(dir.clone())
                .read_strongly_consistent()
                .await?;

            assert!(!ReadRef::ptr_eq(&read_dir, &read_dir2));

            // Modify a symlink target file
            let write_result = write(
                fs.root().await?.join("link_target.js")?,
                rcstr!("new_contents"),
            );
            write_result.read_strongly_consistent().await?;
            apply_effects(write_result).await?;
            let read_dir3 = track_star_star_glob(dir.clone())
                .read_strongly_consistent()
                .await?;

            assert!(!ReadRef::ptr_eq(&read_dir3, &read_dir2));

            anyhow::Ok(())
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn track_glob_symlinks_loop() {
        crate::register();
        let scratch = tempfile::tempdir().unwrap();
        {
            use std::os::unix::fs::symlink;

            // Create a simple directory with 1 file and a symlink pointing at at a file in a
            // subdirectory
            let path = scratch.path();
            let sub = &path.join("sub");
            create_dir(sub).unwrap();
            let foo = sub.join("foo.js");
            File::create_new(&foo).unwrap().write_all(b"foo").unwrap();
            // put a link in sub that points back at its parent director
            symlink(sub, sub.join("link")).unwrap();
        }
        let tt = turbo_tasks::TurboTasks::new(TurboTasksBackend::new(
            BackendOptions::default(),
            noop_backing_storage(),
        ));
        let path: RcStr = scratch.path().to_str().unwrap().into();
        tt.run_once(async {
            use turbo_rcstr::rcstr;

            let fs = Vc::upcast::<Box<dyn FileSystem>>(DiskFileSystem::new(rcstr!("temp"), path));
            let err = fs
                .root()
                .await?
                .track_glob(Glob::new(rcstr!("**")), false)
                .await
                .expect_err("Should have detected an infinite loop");

            assert_eq!(
                "'sub/link' is a symlink causes that causes an infinite loop!",
                format!("{}", err.root_cause())
            );

            // Same when calling track glob
            let err = fs
                .root()
                .await?
                .track_glob(Glob::new(rcstr!("**")), false)
                .await
                .expect_err("Should have detected an infinite loop");

            assert_eq!(
                "'sub/link' is a symlink causes that causes an infinite loop!",
                format!("{}", err.root_cause())
            );

            anyhow::Ok(())
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_glob_symlinks_loop() {
        crate::register();
        let scratch = tempfile::tempdir().unwrap();
        {
            use std::os::unix::fs::symlink;

            // Create a simple directory with 1 file and a symlink pointing at at a file in a
            // subdirectory
            let path = scratch.path();
            let sub = &path.join("sub");
            create_dir(sub).unwrap();
            let foo = sub.join("foo.js");
            File::create_new(&foo).unwrap().write_all(b"foo").unwrap();
            // put a link in sub that points back at its parent director
            symlink(sub, sub.join("link")).unwrap();
        }
        let tt = turbo_tasks::TurboTasks::new(TurboTasksBackend::new(
            BackendOptions::default(),
            noop_backing_storage(),
        ));
        let path: RcStr = scratch.path().to_str().unwrap().into();
        tt.run_once(async {
            let fs = Vc::upcast::<Box<dyn FileSystem>>(DiskFileSystem::new(rcstr!("temp"), path));
            let err = fs
                .root()
                .await?
                .read_glob(Glob::new(rcstr!("**")))
                .await
                .expect_err("Should have detected an infinite loop");

            assert_eq!(
                "'sub/link' is a symlink causes that causes an infinite loop!",
                format!("{}", err.root_cause())
            );

            // Same when calling track glob
            let err = fs
                .root()
                .await?
                .track_glob(Glob::new(rcstr!("**")), false)
                .await
                .expect_err("Should have detected an infinite loop");

            assert_eq!(
                "'sub/link' is a symlink causes that causes an infinite loop!",
                format!("{}", err.root_cause())
            );

            anyhow::Ok(())
        })
        .await
        .unwrap();
    }
}
