//! `ReplaceFileW`: swapping a file's content for another file's while the
//! replaced file keeps its access list, attributes and creation time. A rename
//! over it would lose them (the result would carry the new file's inherited
//! access list instead).

use std::io;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_IGNORE_MERGE_ERRORS, ReplaceFileW};

use super::security::wide;

/// `ERROR_UNABLE_TO_REMOVE_REPLACED`: nothing changed; both files keep their names.
pub const ERROR_UNABLE_TO_REMOVE_REPLACED: i32 = 1175;
/// `ERROR_UNABLE_TO_MOVE_REPLACEMENT`: with a backup name, both files keep their
/// names.
pub const ERROR_UNABLE_TO_MOVE_REPLACEMENT: i32 = 1176;
/// `ERROR_UNABLE_TO_MOVE_REPLACEMENT_2`: the replaced file now has the backup
/// name and the replacement kept its own; rename the backup back.
pub const ERROR_UNABLE_TO_MOVE_REPLACEMENT_2: i32 = 1177;

/// Replaces `replaced` with `replacement` (which must be closed), keeping
/// `replaced`'s access list, attributes and creation time, and moving the old
/// content to `backup`. All three paths must be on one volume.
///
/// The backup name keeps a failure recoverable: without one, a failed final
/// rename (`ERROR_UNABLE_TO_MOVE_REPLACEMENT`) leaves `replaced` gone. See the
/// error constants for what each failure leaves behind.
pub fn replace_file(replaced: &Path, replacement: &Path, backup: &Path) -> io::Result<()> {
    let replaced = wide(replaced.as_os_str());
    let replacement = wide(replacement.as_os_str());
    let backup = wide(backup.as_os_str());
    // SAFETY: the three paths are NUL-terminated and outlive the call; the two
    // reserved pointers are null as documented.
    let ok = unsafe {
        ReplaceFileW(
            replaced.as_ptr(),
            replacement.as_ptr(),
            backup.as_ptr(),
            REPLACEFILE_IGNORE_MERGE_ERRORS,
            ptr::null(),
            ptr::null(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::security::{FileProblem, check_private_file};

    /// The replaced path gets the new content and keeps its own access list (an
    /// extra grant survives); the old content moves to the backup name.
    #[test]
    fn replaces_in_place_and_keeps_the_access_list() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let target = temp.path().join("hosts");
        let new = temp.path().join("hosts.new");
        let backup = temp.path().join("hosts.bak");
        std::fs::write(&target, "old")?;
        std::fs::write(&new, "new")?;
        // Everyone (S-1-1-0), read access, on the replaced file only.
        let status = std::process::Command::new("icacls")
            .arg(&target)
            .args(["/grant", "*S-1-1-0:R"])
            .stdout(std::process::Stdio::null())
            .status()?;
        assert!(status.success(), "icacls");
        assert_eq!(check_private_file(&new)?, Ok(()));
        replace_file(&target, &new, &backup)?;
        assert_eq!(std::fs::read_to_string(&target)?, "new");
        assert_eq!(std::fs::read_to_string(&backup)?, "old");
        assert!(!new.exists());
        assert_eq!(
            check_private_file(&target)?,
            Err(FileProblem::Acl(vec!["S-1-1-0".to_owned()])),
            "the replaced file's grant is kept"
        );
        let missing = temp.path().join("missing");
        assert!(replace_file(&missing, &target, &backup).is_err());
        Ok(())
    }
}
