//! Renames and removals of files and folders the daemon owns (logs, lane data,
//! prepared files, caches). They block, so callers run them off the async
//! threads.

#[cfg(unix)]
pub(crate) use std::fs::{remove_dir_all, remove_file, rename};

// Windows refuses to rename or delete a file another program (an editor, an
// indexer, antivirus) has open without sharing delete, which is often brief:
// these retry for up to about 0.6 s.
#[cfg(windows)]
pub(crate) use devlanes_sys_windows::file::{
    remove_dir_all_retrying as remove_dir_all, remove_file_retrying as remove_file,
    rename_retrying as rename,
};
