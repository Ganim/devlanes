use std::fs::File;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::sync::mpsc;
use std::time::Duration;

use devlanes_sys_windows::file::{
    file_identity, is_held, path_identity, remove_dir_all_retrying, remove_file_retrying,
    rename_retrying, retry_while_held, system_directory,
};

/// `FILE_SHARE_READ` only: other opens may read, but not write, rename or delete.
const SHARE_READ: u32 = 1;

/// Two handles to one file, or to two names of it, have one identity;
/// another file has another.
#[test]
fn identity_follows_the_file() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    std::fs::write(&first, "1")?;
    std::fs::write(&second, "2")?;
    let link = temp.path().join("link");
    std::fs::hard_link(&first, &link)?;
    let identity = file_identity(&File::open(&first)?)?;
    assert_eq!(file_identity(&File::open(&first)?)?, identity);
    assert_eq!(file_identity(&File::open(&link)?)?, identity);
    assert_ne!(file_identity(&File::open(&second)?)?, identity);
    Ok(())
}

/// A directory's identity survives a rename and changes when it is removed
/// and created again; a file's matches the one of its open handle.
#[test]
fn path_identity_names_the_entry() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let dir = temp.path().join("dir");
    std::fs::create_dir(&dir)?;
    let before = path_identity(&dir)?;
    let moved = temp.path().join("moved");
    std::fs::rename(&dir, &moved)?;
    assert_eq!(path_identity(&moved)?, before);
    std::fs::remove_dir(&moved)?;
    std::fs::create_dir(&dir)?;
    assert_ne!(path_identity(&dir)?, before);
    let file = temp.path().join("file");
    std::fs::write(&file, "x")?;
    assert_eq!(path_identity(&file)?, file_identity(&File::open(&file)?)?);
    Ok(())
}

/// Opens `path` without delete sharing on a thread and closes it after
/// `held`, so renames and removals meet a sharing violation until then.
fn hold(path: &std::path::Path, held: Duration) -> io::Result<std::thread::JoinHandle<()>> {
    let file = File::options()
        .read(true)
        .share_mode(SHARE_READ)
        .open(path)?;
    Ok(std::thread::spawn(move || {
        std::thread::sleep(held);
        drop(file);
    }))
}

/// A rename or removal waits out a handle that is closed shortly after.
#[test]
fn retries_ride_out_brief_holds() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let from = temp.path().join("from");
    let to = temp.path().join("to");
    std::fs::write(&from, "x")?;
    let holder = hold(&from, Duration::from_millis(100))?;
    rename_retrying(&from, &to)?;
    let _ = holder.join();
    assert!(!from.exists() && to.exists());

    let holder = hold(&to, Duration::from_millis(100))?;
    remove_file_retrying(&to)?;
    let _ = holder.join();
    assert!(!to.exists());

    let tree = temp.path().join("tree");
    std::fs::create_dir_all(tree.join("a/b"))?;
    std::fs::write(tree.join("a/b/c"), "x")?;
    let holder = hold(&tree.join("a/b/c"), Duration::from_millis(100))?;
    remove_dir_all_retrying(&tree)?;
    let _ = holder.join();
    assert!(!tree.exists());
    Ok(())
}

/// A handle held throughout makes the rename fail with the sharing violation
/// once the retries run out.
#[test]
fn retries_give_up() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let from = temp.path().join("from");
    std::fs::write(&from, "x")?;
    let (release, released) = mpsc::channel::<()>();
    let file = File::options()
        .read(true)
        .share_mode(SHARE_READ)
        .open(&from)?;
    let holder = std::thread::spawn(move || {
        let _ = released.recv_timeout(Duration::from_secs(30));
        drop(file);
    });
    let error = rename_retrying(&from, &temp.path().join("to")).err();
    let _ = release.send(());
    let _ = holder.join();
    assert!(error.as_ref().is_some_and(is_held), "{error:?}");
    assert_eq!(error.and_then(|error| error.raw_os_error()), Some(32));
    assert!(!is_held(&io::Error::from(io::ErrorKind::NotFound)));
    Ok(())
}

/// Any operation can ride out a brief hold, and one that keeps failing for
/// another reason fails at once.
#[test]
fn retry_while_held_runs_any_operation() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let from = temp.path().join("from");
    let to = temp.path().join("to");
    std::fs::write(&from, "x")?;
    let holder = hold(&from, Duration::from_millis(100))?;
    let mut attempts = 0;
    retry_while_held(|| {
        attempts += 1;
        std::fs::rename(&from, &to)
    })?;
    let _ = holder.join();
    assert!(to.exists() && attempts > 1, "{attempts}");
    let mut attempts = 0;
    let missing = retry_while_held(|| {
        attempts += 1;
        Err(io::ErrorKind::NotFound.into())
    });
    assert!(missing.is_err());
    assert_eq!(attempts, 1);
    Ok(())
}

/// System32 holds Windows' own tools.
#[test]
fn system_directory_holds_tools() -> io::Result<()> {
    let dir = system_directory()?;
    assert!(dir.is_absolute(), "{}", dir.display());
    assert!(dir.join("cmd.exe").is_file(), "{}", dir.display());
    Ok(())
}
