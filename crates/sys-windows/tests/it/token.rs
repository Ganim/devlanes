use devlanes_sys_windows::token::{self, InvalidSid, MEDIUM_INTEGRITY, Sid};

/// SIDs parse from their canonical text and print it back; the well-known
/// constructors match their text form.
#[test]
fn sids_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let me = token::current_user()?;
    let text = me.to_string();
    assert!(
        text.starts_with("S-1-5-") || text.starts_with("S-1-12-"),
        "{text}"
    );
    assert_eq!(Sid::parse(&text)?, me);
    assert_eq!(text.parse::<Sid>()?, me);
    assert_eq!(Sid::parse("S-1-5-18")?, Sid::local_system());
    assert_eq!(Sid::parse("S-1-5-32-544")?, Sid::administrators());
    assert_eq!(Sid::parse("S-1-1-0")?, Sid::everyone());
    assert_eq!(Sid::parse("S-1-3-0")?, Sid::creator_owner());
    assert_eq!(Sid::local_system().to_string(), "S-1-5-18");
    assert_eq!(Sid::administrators().to_string(), "S-1-5-32-544");
    assert_ne!(Sid::local_system(), Sid::administrators());
    let wide = "S-1-0x000100000000-7";
    assert_eq!(Sid::parse(wide)?.to_string(), wide);
    let max = "S-1-5-21-4294967295-1-2-3-4-5-6-7-8-9-10-11-12-13";
    assert_eq!(Sid::parse(max)?.to_string(), max);
    Ok(())
}

/// Anything that is not a canonical SID is refused, including SDDL aliases
/// and text that could carry SDDL syntax.
#[test]
fn sids_parse_strictly() {
    for bad in [
        "",
        "SY",
        "BA",
        "S-1-5-21)(A;;GA;;;WD",
        "S-1-5-18 ",
        " S-1-5-18",
        "s-1-5-18",
        "S-2-5-18",
        "S-1-",
        "S-1-5",
        "S-1-5-18-",
        "S-1--18",
        "S-1-05-18",
        "S-1-5-018",
        "S-1-5-+18",
        "S-1-5-4294967296",
        "S-1-0x0000000000FF-1",
        "S-1-0x000100000000a-1",
        "S-1-0x00010000000a-1",
        "S-1-5-1-2-3-4-5-6-7-8-9-10-11-12-13-14-15-16",
    ] {
        assert_eq!(Sid::parse(bad), Err(InvalidSid), "{bad:?}");
    }
}

/// This process's token facts are consistent with each other.
#[test]
fn current_token_facts() -> std::io::Result<()> {
    let facts = token::current_facts()?;
    assert_eq!(facts.elevated, token::elevated()?);
    assert!(facts.integrity >= MEDIUM_INTEGRITY, "{facts:?}");
    assert!(!facts.app_container, "{facts:?}");
    Ok(())
}
