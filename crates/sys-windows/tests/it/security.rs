use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

use devlanes_sys_windows::security::{
    Outcome, Problem, check_private_dir, check_private_file, ensure_private_dir,
};
use devlanes_sys_windows::token::{self, Sid};

/// Runs a System32 tool with `args`, failing the test when it fails.
fn run(tool: &str, args: &[&std::ffi::OsStr]) -> io::Result<()> {
    let program = devlanes_sys_windows::file::system_directory()?.join(tool);
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()?;
    assert!(status.success(), "{tool} {args:?}");
    Ok(())
}

/// Grants Everyone read access to `path` (and what it holds) through `icacls`.
fn grant_everyone(path: &Path, inherit: bool) -> io::Result<()> {
    let grant = if inherit {
        "*S-1-1-0:(OI)(CI)R"
    } else {
        "*S-1-1-0:R"
    };
    run(
        "icacls.exe",
        &[path.as_os_str(), "/grant".as_ref(), grant.as_ref()],
    )
}

/// Turns a problem into an error, for setup steps.
fn private(result: io::Result<Result<Outcome, Problem>>) -> io::Result<Outcome> {
    result?.map_err(|problem| io::Error::other(format!("{problem:?}")))
}

/// A directory created here is private and stays so; a file in its place is
/// reported, not created over.
#[test]
fn created_directories_are_private() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let dir = temp.path().join("private");
    assert_eq!(ensure_private_dir(&dir)?, Ok(Outcome::Created));
    assert_eq!(ensure_private_dir(&dir)?, Ok(Outcome::Present));
    assert_eq!(check_private_dir(&dir)?, Ok(()));
    let file = temp.path().join("file");
    std::fs::write(&file, "x")?;
    assert_eq!(check_private_dir(&file)?, Err(Problem::NotADirectory));
    assert_eq!(ensure_private_dir(&file)?, Err(Problem::NotADirectory));
    let missing = check_private_dir(&temp.path().join("missing"));
    assert_eq!(
        missing.err().map(|error| error.kind()),
        Some(io::ErrorKind::NotFound)
    );
    Ok(())
}

/// An empty directory of this user that others can read is tightened and
/// then passes the check.
#[test]
fn empty_directories_are_tightened() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let open = temp.path().join("open");
    std::fs::create_dir(&open)?;
    grant_everyone(&open, true)?;
    let everyone = Problem::Acl(vec![Sid::everyone()]);
    assert_eq!(check_private_dir(&open)?, Err(everyone));
    assert_eq!(ensure_private_dir(&open)?, Ok(Outcome::Tightened));
    assert_eq!(check_private_dir(&open)?, Ok(()));
    assert_eq!(ensure_private_dir(&open)?, Ok(Outcome::Present));
    Ok(())
}

/// A non-empty directory is never tightened: its entries would keep the old
/// grants.
#[test]
fn non_empty_directories_are_refused() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let open = temp.path().join("open");
    std::fs::create_dir(&open)?;
    std::fs::write(open.join("state.db"), "x")?;
    grant_everyone(&open, true)?;
    let everyone = Problem::Acl(vec![Sid::everyone()]);
    assert_eq!(ensure_private_dir(&open)?, Err(everyone.clone()));
    assert_eq!(check_private_dir(&open)?, Err(everyone), "left unchanged");
    Ok(())
}

/// Junctions are refused, never followed. They need no privilege to create.
#[test]
fn junctions_are_refused() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let target = temp.path().join("target");
    private(ensure_private_dir(&target))?;
    let link = temp.path().join("link");
    run(
        "cmd.exe",
        &[
            "/d".as_ref(),
            "/c".as_ref(),
            "mklink".as_ref(),
            "/J".as_ref(),
            link.as_os_str(),
            target.as_os_str(),
        ],
    )?;
    assert_eq!(check_private_dir(&link)?, Err(Problem::ReparsePoint));
    assert_eq!(ensure_private_dir(&link)?, Err(Problem::ReparsePoint));
    assert_eq!(check_private_file(&link)?, Err(Problem::ReparsePoint));
    Ok(())
}

/// A file in a private directory is private; an explicit grant, a second
/// name or a directory in its place is not.
#[test]
fn private_files() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let dir = temp.path().join("private");
    private(ensure_private_dir(&dir))?;
    let key = dir.join("key.pem");
    std::fs::write(&key, "x")?;
    assert_eq!(check_private_file(&key)?, Ok(()));
    assert_eq!(check_private_file(&dir)?, Err(Problem::NotAFile));
    assert_eq!(
        check_private_file(&dir.join("missing"))
            .err()
            .map(|error| error.kind()),
        Some(io::ErrorKind::NotFound)
    );
    let linked = dir.join("linked.pem");
    std::fs::write(&linked, "x")?;
    std::fs::hard_link(&linked, temp.path().join("elsewhere.pem"))?;
    assert_eq!(check_private_file(&linked)?, Err(Problem::HardLinks(2)));
    grant_everyone(&key, false)?;
    assert_eq!(
        check_private_file(&key)?,
        Err(Problem::Acl(vec![Sid::everyone()]))
    );
    Ok(())
}

/// A directory owned by another account is reported with that owner. The
/// Windows directory belongs to TrustedInstaller.
#[test]
fn foreign_owners_are_reported() -> io::Result<()> {
    let system = devlanes_sys_windows::file::system_directory()?;
    let Some(windows) = system.parent() else {
        return Err(io::Error::other("System32 has no parent"));
    };
    match check_private_dir(windows)? {
        Err(Problem::Owner(owner)) => assert_ne!(owner, token::current_user()?),
        other => panic!("{other:?}"),
    }
    Ok(())
}
