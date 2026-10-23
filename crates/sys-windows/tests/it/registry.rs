use std::io;

use devlanes_sys_windows::registry::{
    Hive, RegValue, delete_tree, delete_value, read_binary, read_string, read_value, subkeys,
    write_binary, write_dword, write_multi_string, write_string,
};

/// A scratch key under `HKCU\Software`, removed when dropped.
struct Scratch(String);

impl Scratch {
    fn new(tag: &str) -> Self {
        Self(format!(
            r"Software\devlanes-sys-windows-test-{}-{tag}",
            std::process::id()
        ))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = delete_tree(&self.0);
    }
}

/// Strings round-trip, including non-ASCII and empty text; deleting reports
/// whether there was a value; missing keys and values read as `None`.
#[test]
fn strings_round_trip() -> io::Result<()> {
    let scratch = Scratch::new("strings");
    assert_eq!(read_string(&scratch.0, "devlanes")?, None);
    assert!(!delete_value(&scratch.0, "devlanes")?);
    let value = "\"C:\\Program Files\\d\u{e9}vlanes\\devlanes.exe\" daemon run";
    write_string(&scratch.0, "devlanes", value)?;
    assert_eq!(read_string(&scratch.0, "devlanes")?.as_deref(), Some(value));
    write_string(&scratch.0, "devlanes", "")?;
    assert_eq!(read_string(&scratch.0, "devlanes")?.as_deref(), Some(""));
    assert!(delete_value(&scratch.0, "devlanes")?);
    assert_eq!(read_string(&scratch.0, "devlanes")?, None);
    delete_tree(&scratch.0)?;
    delete_tree(&scratch.0)?;
    assert!(delete_tree(r"\").is_err());
    Ok(())
}

/// Binary values round-trip; a value of the other type is an error rather
/// than a misread.
#[test]
fn binary_values_round_trip() -> io::Result<()> {
    let scratch = Scratch::new("binary");
    assert_eq!(read_binary(&scratch.0, "devlanes")?, None);
    let value = [3, 0, 0, 0, 0x2c, 0xf1, 0x65, 0x7f, 0x99, 0xed, 0xd9, 0x01];
    write_binary(&scratch.0, "devlanes", &value)?;
    assert_eq!(
        read_binary(&scratch.0, "devlanes")?.as_deref(),
        Some(&value[..])
    );
    assert!(read_string(&scratch.0, "devlanes").is_err());
    write_string(&scratch.0, "text", "x")?;
    assert!(read_binary(&scratch.0, "text").is_err());
    Ok(())
}

/// Typed reads see every type the writers produce, and subkeys are listed;
/// missing keys and values read as `None`.
#[test]
fn typed_values_and_subkeys() -> io::Result<()> {
    let scratch = Scratch::new("typed");
    let rules = format!(r"{}\rules", scratch.0);
    assert_eq!(subkeys(Hive::CurrentUser, &rules)?, None);
    assert_eq!(read_value(Hive::CurrentUser, &rules, "Name")?, None);
    let rule = format!(r"{rules}\devlanes-test");
    write_multi_string(&rule, "Name", &[".test", ".dev.internal"])?;
    write_dword(&rule, "ConfigOptions", 8)?;
    write_string(&rule, "IPSECCARestriction", "")?;
    write_string(&format!(r"{rules}\other"), "Comment", "x")?;
    assert_eq!(
        read_value(Hive::CurrentUser, &rule, "Name")?,
        Some(RegValue::MultiString(vec![
            ".test".into(),
            ".dev.internal".into()
        ]))
    );
    assert_eq!(
        read_value(Hive::CurrentUser, &rule, "ConfigOptions")?,
        Some(RegValue::Dword(8))
    );
    assert_eq!(
        read_value(Hive::CurrentUser, &rule, "IPSECCARestriction")?,
        Some(RegValue::String(String::new()))
    );
    assert_eq!(read_value(Hive::CurrentUser, &rule, "Missing")?, None);
    let mut names = subkeys(Hive::CurrentUser, &rules)?.expect("rules key");
    names.sort();
    assert_eq!(names, ["devlanes-test", "other"]);
    assert_eq!(
        subkeys(Hive::CurrentUser, &rule)?,
        Some(Vec::new()),
        "a key without subkeys"
    );
    assert!(write_multi_string(&rule, "Bad", &[".test", ""]).is_err());
    Ok(())
}

/// The machine hive is readable without elevation: the DNS client's
/// parameters key exists on every Windows install.
#[test]
fn machine_keys_are_readable() -> io::Result<()> {
    let parameters = r"SYSTEM\CurrentControlSet\Services\Dnscache\Parameters";
    assert!(subkeys(Hive::LocalMachine, parameters)?.is_some());
    assert_eq!(
        subkeys(
            Hive::LocalMachine,
            r"SOFTWARE\devlanes-sys-windows-test-missing"
        )?,
        None
    );
    Ok(())
}
