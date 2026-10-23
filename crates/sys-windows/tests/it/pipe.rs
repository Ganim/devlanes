use std::io;
use std::time::Duration;

use devlanes_sys_windows::pipe::{PipeFactory, server_owner};
use devlanes_sys_windows::token;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::ClientOptions;

/// A pipe name unique to this test process.
fn name(tag: &str) -> String {
    format!(
        r"\\.\pipe\devlanes-sys-windows-test-{}-{tag}",
        std::process::id()
    )
}

/// A client finds the current user as the pipe's owner and talks to the
/// server; a second first instance of the name is refused, later instances
/// are not.
#[tokio::test]
async fn owner_check_and_first_instance() -> io::Result<()> {
    let name = name("owner");
    let factory = PipeFactory::current_user(&name)?;
    let server = factory.create(true)?;
    assert_eq!(
        factory.create(true).err().map(|error| error.kind()),
        Some(io::ErrorKind::PermissionDenied),
        "the first instance is unique"
    );
    let _spare = factory.create(false)?;
    let mut client = ClientOptions::new().open(&name)?;
    tokio::time::timeout(Duration::from_secs(20), server.connect())
        .await
        .map_err(|_| io::Error::other("connect timed out"))??;
    let mut server = server;
    assert_eq!(server_owner(&client)?, token::current_user()?);
    client.write_all(b"hi").await?;
    let mut buffer = [0; 2];
    tokio::time::timeout(Duration::from_secs(20), server.read_exact(&mut buffer))
        .await
        .map_err(|_| io::Error::other("read timed out"))??;
    assert_eq!(&buffer, b"hi");
    Ok(())
}
