//! Applies a multi-file `WorkspaceEdit` to disk only -- never to an
//! already-open buffer, that split is `ide-ui`'s job (`docs/features/
//! code-actions.md` §2.2/§3.4). All-or-nothing across every file in one
//! `WorkspaceEdit`: each file is read fresh immediately before it is
//! written, and if any file's read, bounds check, or write fails, every
//! file this call already wrote is restored to its pre-call content before
//! the error is returned.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::text::Transaction;

/// One file's worth of a multi-file edit, already expressed as an
/// `ide-core`-native `Transaction` (byte offsets, not LSP `Position`s --
/// `ide-ui` is responsible for that conversion; `ide-core` has no
/// dependency on `ide-lsp` and must stay that way).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub path: PathBuf,
    pub transaction: Transaction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEdit {
    pub edits: Vec<FileEdit>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceEditError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("transaction for {path} does not fit the file's current content")]
    OffsetOutOfRange { path: PathBuf },
    #[error(
        "could not write {path}: {source} (rollback of already-written files: {rollback_errors:?})"
    )]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
        /// Every already-written file this call attempted to restore after
        /// `path` failed, and the error each restore itself hit -- empty
        /// means rollback fully succeeded. A rollback failure (e.g. the
        /// disk went read-only mid-operation) is reported here rather than
        /// swallowed, so a caller never treats a botched restore as a
        /// clean, misleadingly-simple "failed to apply."
        rollback_errors: Vec<(PathBuf, io::Error)>,
    },
}

/// Applies `edit` to files on disk only -- **not** to any already-open
/// buffer; that's `ide-ui`'s job, applied separately via `Buffer::apply`.
/// All-or-nothing across every file in `edit.edits`: reads each file fresh
/// immediately before writing it, and if any file's read or write fails,
/// restores every file this call had already successfully written back to
/// its pre-call content before returning the error.
///
/// Reads and writes the same file through **one already-open handle**
/// rather than two independent path-resolving calls
/// (`docs/security-findings/rust-core-dev-workspace-edit-2026-08-20.md`,
/// finding 1): `fs::read_to_string`/`fs::write` each re-resolve `path`
/// from scratch, so if `path` is a symlink whose target changes between
/// the two calls, the read and the write can silently land in two
/// different files -- live-verified to actually happen under a racing
/// symlink swap (~2.9% of calls in that harness). Opening once with
/// `OpenOptions::read(true).write(true)` binds both operations to the
/// same inode, closing that race.
pub fn apply_workspace_edit_to_disk(edit: &WorkspaceEdit) -> Result<(), WorkspaceEditError> {
    let mut written: Vec<(PathBuf, String)> = Vec::with_capacity(edit.edits.len());

    for file_edit in &edit.edits {
        let path = file_edit.path.clone();

        let open_and_read = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .and_then(|mut file| {
                let mut original = String::new();
                file.read_to_string(&mut original)?;
                Ok((file, original))
            });
        let (mut file, original) = match open_and_read {
            Ok(pair) => pair,
            Err(source) => {
                rollback(&written);
                return Err(WorkspaceEditError::Read { path, source });
            }
        };

        let new_content = match apply_transaction(&original, &file_edit.transaction) {
            Some(text) => text,
            None => {
                rollback(&written);
                return Err(WorkspaceEditError::OffsetOutOfRange { path });
            }
        };

        if let Err(source) = write_in_place(&mut file, &new_content) {
            let rollback_errors = rollback(&written);
            return Err(WorkspaceEditError::Write {
                path,
                source,
                rollback_errors,
            });
        }

        written.push((path, original));
    }

    Ok(())
}

/// Overwrites `file`'s full contents in place through its existing handle
/// (see `apply_workspace_edit_to_disk`'s doc comment for why this matters
/// over a path-based `fs::write`), truncating any bytes left over from a
/// longer previous length.
fn write_in_place(file: &mut fs::File, content: &str) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(content.as_bytes())?;
    file.set_len(content.len() as u64)?;
    file.flush()
}

/// Restores every `(path, original content)` pair to disk, best-effort --
/// a file that fails to restore is named in the returned vec rather than
/// silently assumed fixed.
fn rollback(written: &[(PathBuf, String)]) -> Vec<(PathBuf, io::Error)> {
    written
        .iter()
        .filter_map(|(path, original)| {
            fs::write(path, original)
                .err()
                .map(|source| (path.clone(), source))
        })
        .collect()
}

/// Deliberately *stricter* than `TextBuffer::apply`'s own clamping: a live
/// typing widget's occasionally-slightly-off offset is fine to clamp for
/// UX, but silently clamping a server-computed disk edit would corrupt a
/// file's content with no error surfaced anywhere -- worse than refusing
/// the write. `None` means some change's range doesn't fit `content`.
///
/// `pub` since `docs/features/refactor-this.md` §2.1: the Refactor
/// Preview dialog needs the exact same "apply a `Transaction` to a
/// `String`, `None` if it doesn't fit" operation to compute a file's
/// post-edit text for diffing, before anything is actually written.
pub fn apply_transaction(content: &str, transaction: &Transaction) -> Option<String> {
    for change in transaction.changes() {
        if change.range.end > content.len()
            || !content.is_char_boundary(change.range.start)
            || !content.is_char_boundary(change.range.end)
        {
            return None;
        }
    }

    let mut result = content.to_string();
    for change in transaction.changes().iter().rev() {
        result.replace_range(change.range.clone(), &change.insert);
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Change;
    use std::fs;

    fn write_temp(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ide-core-workspace-edit-tests-{}-{}",
            std::process::id(),
            name
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    fn tx(changes: &[(std::ops::Range<usize>, &str)]) -> Transaction {
        Transaction::new(
            changes
                .iter()
                .map(|(r, s)| Change::new(r.clone(), *s))
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn applies_a_single_file_edit() {
        let path = write_temp("single.txt", "hello world");
        let edit = WorkspaceEdit {
            edits: vec![FileEdit {
                path: path.clone(),
                transaction: tx(&[(0..5, "goodbye")]),
            }],
        };
        apply_workspace_edit_to_disk(&edit).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "goodbye world");
    }

    #[test]
    fn applies_edits_across_multiple_files() {
        let a = write_temp("multi-a.txt", "aaa");
        let b = write_temp("multi-b.txt", "bbb");
        let edit = WorkspaceEdit {
            edits: vec![
                FileEdit {
                    path: a.clone(),
                    transaction: tx(&[(0..3, "AAA")]),
                },
                FileEdit {
                    path: b.clone(),
                    transaction: tx(&[(0..3, "BBB")]),
                },
            ],
        };
        apply_workspace_edit_to_disk(&edit).unwrap();
        assert_eq!(fs::read_to_string(&a).unwrap(), "AAA");
        assert_eq!(fs::read_to_string(&b).unwrap(), "BBB");
    }

    #[test]
    fn empty_edit_list_is_a_no_op() {
        apply_workspace_edit_to_disk(&WorkspaceEdit { edits: vec![] }).unwrap();
    }

    #[test]
    fn a_transaction_with_no_changes_leaves_the_file_untouched() {
        let path = write_temp("empty-tx.txt", "unchanged");
        let edit = WorkspaceEdit {
            edits: vec![FileEdit {
                path: path.clone(),
                transaction: Transaction::default(),
            }],
        };
        apply_workspace_edit_to_disk(&edit).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "unchanged");
    }

    #[test]
    fn read_failure_on_a_nonexistent_file_returns_read_error() {
        let missing = std::env::temp_dir().join(format!(
            "ide-core-workspace-edit-missing-{}.txt",
            std::process::id()
        ));
        let _ = fs::remove_file(&missing);
        let edit = WorkspaceEdit {
            edits: vec![FileEdit {
                path: missing.clone(),
                transaction: tx(&[(0..0, "x")]),
            }],
        };
        let err = apply_workspace_edit_to_disk(&edit).unwrap_err();
        match err {
            WorkspaceEditError::Read { path, .. } => assert_eq!(path, missing),
            other => panic!("expected Read, got {other:?}"),
        }
    }

    #[test]
    fn read_failure_on_a_later_file_rolls_back_earlier_writes() {
        let good = write_temp("rollback-read-good.txt", "original");
        let missing = std::env::temp_dir().join(format!(
            "ide-core-workspace-edit-missing2-{}.txt",
            std::process::id()
        ));
        let _ = fs::remove_file(&missing);
        let edit = WorkspaceEdit {
            edits: vec![
                FileEdit {
                    path: good.clone(),
                    transaction: tx(&[(0..8, "mutated!")]),
                },
                FileEdit {
                    path: missing,
                    transaction: tx(&[(0..0, "x")]),
                },
            ],
        };
        let err = apply_workspace_edit_to_disk(&edit).unwrap_err();
        assert!(matches!(err, WorkspaceEditError::Read { .. }));
        assert_eq!(fs::read_to_string(&good).unwrap(), "original");
    }

    #[test]
    fn offset_out_of_range_is_rejected_without_writing() {
        let path = write_temp("oob.txt", "short");
        let edit = WorkspaceEdit {
            edits: vec![FileEdit {
                path: path.clone(),
                transaction: tx(&[(0..1000, "x")]),
            }],
        };
        let err = apply_workspace_edit_to_disk(&edit).unwrap_err();
        match err {
            WorkspaceEditError::OffsetOutOfRange { path: p } => assert_eq!(p, path),
            other => panic!("expected OffsetOutOfRange, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "short");
    }

    #[test]
    fn offset_out_of_range_on_a_later_file_rolls_back_earlier_writes() {
        let good = write_temp("rollback-oob-good.txt", "original");
        let bad = write_temp("rollback-oob-bad.txt", "short");
        let edit = WorkspaceEdit {
            edits: vec![
                FileEdit {
                    path: good.clone(),
                    transaction: tx(&[(0..8, "mutated!")]),
                },
                FileEdit {
                    path: bad,
                    transaction: tx(&[(0..1000, "x")]),
                },
            ],
        };
        let err = apply_workspace_edit_to_disk(&edit).unwrap_err();
        assert!(matches!(err, WorkspaceEditError::OffsetOutOfRange { .. }));
        assert_eq!(fs::read_to_string(&good).unwrap(), "original");
    }

    #[test]
    fn a_change_range_landing_mid_char_boundary_is_rejected() {
        // "héllo": 'é' is a 2-byte char at offset 1..3. Offset 2 splits it.
        let path = write_temp("boundary.txt", "héllo");
        let edit = WorkspaceEdit {
            edits: vec![FileEdit {
                path: path.clone(),
                transaction: tx(&[(2..4, "x")]),
            }],
        };
        let err = apply_workspace_edit_to_disk(&edit).unwrap_err();
        assert!(matches!(err, WorkspaceEditError::OffsetOutOfRange { .. }));
        assert_eq!(fs::read_to_string(&path).unwrap(), "héllo");
    }

    #[test]
    #[cfg(unix)]
    fn a_file_that_cannot_be_opened_for_writing_rolls_back_earlier_writes() {
        // Read and write now share one OpenOptions::read(true).write(true)
        // handle (the TOCTOU fix -- see apply_workspace_edit_to_disk's doc
        // comment), so a permission problem that used to only surface at
        // the write() stage now surfaces at open() time instead, and is
        // classified as `Read` rather than `Write` -- the open call is the
        // single point that needs both permissions, and it fails before
        // any read happens. This still exercises the same "rollback every
        // already-written file" invariant.
        let good_a = write_temp("rollback-open-a.txt", "aaa");
        let good_b = write_temp("rollback-open-b.txt", "bbb");
        let readonly = write_temp("rollback-open-readonly.txt", "readonly");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&readonly, fs::Permissions::from_mode(0o444)).unwrap();

        let edit = WorkspaceEdit {
            edits: vec![
                FileEdit {
                    path: good_a.clone(),
                    transaction: tx(&[(0..3, "AAA")]),
                },
                FileEdit {
                    path: good_b.clone(),
                    transaction: tx(&[(0..3, "BBB")]),
                },
                FileEdit {
                    path: readonly.clone(),
                    transaction: tx(&[(0..0, "x")]),
                },
            ],
        };
        let result = apply_workspace_edit_to_disk(&edit);

        fs::set_permissions(&readonly, fs::Permissions::from_mode(0o644)).unwrap();

        match result.unwrap_err() {
            WorkspaceEditError::Read { path, .. } => assert_eq!(path, readonly),
            other => panic!("expected Read, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&good_a).unwrap(), "aaa");
        assert_eq!(fs::read_to_string(&good_b).unwrap(), "bbb");
    }

    #[test]
    fn write_in_place_on_a_handle_without_write_access_errors_without_panicking() {
        let path = write_temp("write-in-place-readonly-handle.txt", "original");
        let mut read_only_handle = fs::OpenOptions::new().read(true).open(&path).unwrap();
        write_in_place(&mut read_only_handle, "new content").unwrap_err();
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn rollback_reports_a_restore_that_itself_fails() {
        let dir = std::env::temp_dir().join(format!(
            "ide-core-workspace-edit-tests-rollback-fail-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let gone = dir.join("will-be-removed.txt");
        fs::write(&gone, "original").unwrap();
        fs::remove_dir_all(&dir).unwrap();

        let errors = rollback(&[(gone.clone(), "original".to_string())]);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, gone);
    }

    #[test]
    #[cfg(unix)]
    fn a_symlinked_path_is_edited_through_its_target_not_recreated_as_a_plain_file() {
        // Regression guard for the TOCTOU fix: the old fs::read_to_string
        // + fs::write pair re-resolved the path twice, so a symlink swap
        // between the two calls could redirect the write to a different
        // file than the one that was read (see
        // docs/security-findings/rust-core-dev-workspace-edit-2026-08-20.md,
        // finding 1). With a *stable* symlink target this must still work
        // exactly as if `path` were the target itself, and the symlink
        // must remain a symlink afterward (proving the write went through
        // the existing handle rather than fs::write's create-if-missing
        // path recreating a plain file over the link).
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!(
            "ide-core-workspace-edit-tests-symlink-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.txt");
        let link = dir.join("link.txt");
        fs::write(&target, "hello world").unwrap();
        let _ = fs::remove_file(&link);
        symlink(&target, &link).unwrap();

        let edit = WorkspaceEdit {
            edits: vec![FileEdit {
                path: link.clone(),
                transaction: tx(&[(0..5, "goodbye")]),
            }],
        };
        apply_workspace_edit_to_disk(&edit).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "goodbye world");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn error_display_messages_are_informative() {
        let read_err = WorkspaceEditError::Read {
            path: PathBuf::from("/tmp/foo.rs"),
            source: io::Error::new(io::ErrorKind::NotFound, "no such file"),
        };
        assert!(read_err.to_string().contains("/tmp/foo.rs"));

        let oob_err = WorkspaceEditError::OffsetOutOfRange {
            path: PathBuf::from("/tmp/bar.rs"),
        };
        assert!(oob_err.to_string().contains("/tmp/bar.rs"));
        assert!(oob_err.to_string().contains("does not fit"));

        let write_err = WorkspaceEditError::Write {
            path: PathBuf::from("/tmp/baz.rs"),
            source: io::Error::other("disk full"),
            rollback_errors: vec![(
                PathBuf::from("/tmp/qux.rs"),
                io::Error::other("also failed"),
            )],
        };
        let msg = write_err.to_string();
        assert!(msg.contains("/tmp/baz.rs"));
        assert!(msg.contains("qux.rs"));
    }
}
