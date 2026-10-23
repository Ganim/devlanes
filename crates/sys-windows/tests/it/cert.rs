//! Certificate stores. Changes only ever reach scratch stores, which never
//! show a dialog; the real `Root` and `Disallowed` are only read.

use std::io;

use devlanes_sys_windows::cert::{
    Access, CertError, CertStore, ScratchStore, Thumbprint, View, interactive_desktop,
};

/// A self-signed CA certificate with common name `name`, DER-encoded.
fn certificate(name: &str) -> Vec<u8> {
    certificate_with(name, |_| {})
}

/// Like [`certificate`], with `adjust` applied to the parameters.
fn certificate_with(name: &str, adjust: impl FnOnce(&mut rcgen::CertificateParams)) -> Vec<u8> {
    let key = rcgen::KeyPair::generate().expect("key");
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, name);
    adjust(&mut params);
    params
        .self_signed(&key)
        .expect("certificate")
        .der()
        .to_vec()
}

/// Whether the scratch store exists.
fn exists(name: &str) -> bool {
    CertStore::open(name, View::UserRegistry, Access::Read).is_ok()
}

/// Add, find by subject and thumbprint, list, delete and delete again.
#[test]
fn scratch_store_round_trip() -> io::Result<()> {
    let scratch = ScratchStore::new();
    let store = CertStore::open(scratch.name(), View::UserRegistry, Access::Write)?;
    let ours = certificate("devlanes local development CA");
    store.add_der(&ours).expect("add");
    store.add_der(&certificate("acme CA")).expect("add other");
    // Adding the same certificate again replaces it.
    store.add_der(&ours).expect("add again");

    let logical = CertStore::open(scratch.name(), View::Logical, Access::Read)?;
    let found = logical.thumbprints_with_subject("DEVLANES local development")?;
    assert_eq!(found.len(), 1, "case-insensitive part of the subject");
    let other = logical.thumbprints_with_subject("acme")?;
    assert_eq!(other.len(), 1);
    assert!(logical.contains(&found[0])?);

    let listed = logical.certificates()?.certificates;
    assert_eq!(listed.len(), 2);
    let entry = listed
        .iter()
        .find(|entry| entry.thumbprint == found[0])
        .expect("listed");
    assert_eq!(entry.der, ours);
    assert!(entry.server_auth && entry.time_valid, "{entry:?}");

    assert!(store.delete(&found[0]).expect("delete"));
    assert!(!store.delete(&found[0]).expect("delete absent"));
    let reopened = CertStore::open(scratch.name(), View::Logical, Access::Read)?;
    assert!(!reopened.contains(&found[0])?);
    assert!(reopened.contains(&other[0])?);
    Ok(())
}

/// Usage limits without server authentication and expired certificates are
/// reported as such.
#[test]
fn listing_reports_usage_and_validity() -> io::Result<()> {
    let scratch = ScratchStore::new();
    let store = CertStore::open(scratch.name(), View::UserRegistry, Access::Write)?;
    let client_only = certificate_with("client only", |params| {
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    });
    let server = certificate_with("server", |params| {
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
        ];
    });
    let expired = certificate_with("expired", |params| {
        params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        params.not_after = rcgen::date_time_ymd(2001, 1, 1);
    });
    for der in [&client_only, &server, &expired] {
        store.add_der(der).expect("add");
    }
    let listed = store.certificates()?.certificates;
    let facts = |der: &Vec<u8>| {
        let entry = listed
            .iter()
            .find(|entry| entry.der == *der)
            .expect("listed");
        (entry.server_auth, entry.time_valid)
    };
    assert_eq!(facts(&client_only), (false, true));
    assert_eq!(facts(&server), (true, true));
    assert_eq!(facts(&expired), (true, false));
    Ok(())
}

/// A certificate whose usage extension does not decode never fails the
/// listing: the rest of the store is listed, and it is listed too, read or
/// unreadable (crypt32 tolerates this one today).
#[test]
fn a_malformed_usage_does_not_fail_the_listing() -> io::Result<()> {
    let scratch = ScratchStore::new();
    let store = CertStore::open(scratch.name(), View::UserRegistry, Access::Write)?;
    let good = certificate("good");
    // Extended key usage (2.5.29.37) whose sequence ends inside its first
    // object identifier.
    let broken = certificate_with("broken usage", |params| {
        params
            .custom_extensions
            .push(rcgen::CustomExtension::from_oid_content(
                &[2, 5, 29, 37],
                vec![0x30, 0x03, 0x06, 0x01],
            ));
    });
    for der in [&good, &broken] {
        store.add_der(der).expect("add");
    }
    let listing = store.certificates()?;
    assert!(
        listing.certificates.iter().any(|entry| entry.der == good),
        "{listing:?}"
    );
    let mut every: Vec<&[u8]> = listing
        .certificates
        .iter()
        .map(|entry| entry.der.as_slice())
        .chain(listing.unreadable.iter().map(|entry| entry.der.as_slice()))
        .collect();
    every.sort_unstable();
    let mut added = [good.as_slice(), broken.as_slice()];
    added.sort_unstable();
    assert_eq!(every, added);
    Ok(())
}

/// The logical view never opens for writing, so nothing reaches a machine store.
#[test]
fn logical_view_is_read_only() {
    let scratch = ScratchStore::new();
    let error = CertStore::open(scratch.name(), View::Logical, Access::Write).expect_err("refused");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
    assert!(!exists(scratch.name()), "a refused open creates nothing");
}

/// Garbage is an I/O error, not a declined dialog.
#[test]
fn invalid_der_is_an_io_error() -> io::Result<()> {
    let scratch = ScratchStore::new();
    let store = CertStore::open(scratch.name(), View::UserRegistry, Access::Write)?;
    assert!(matches!(
        store.add_der(b"not a certificate"),
        Err(CertError::Io(_))
    ));
    Ok(())
}

/// A read-only open never creates a store.
#[test]
fn read_only_open_of_a_missing_store_is_not_found() {
    let scratch = ScratchStore::new();
    for view in [View::Logical, View::UserRegistry] {
        let error =
            CertStore::open(scratch.name(), view, Access::Read).expect_err("store does not exist");
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{view:?}: {error}");
    }
    assert!(!exists(scratch.name()));
}

/// The guard deletes the store when a test panics; a never-written store is
/// `NotFound`.
#[test]
fn scratch_store_is_deleted_after_a_panic() {
    let name = std::panic::catch_unwind(|| {
        let scratch = ScratchStore::new();
        CertStore::open(scratch.name(), View::UserRegistry, Access::Write)
            .expect("create")
            .add_der(&certificate("devlanes panic test CA"))
            .expect("add");
        assert!(exists(scratch.name()));
        std::panic::panic_any(scratch.name().to_owned());
    })
    .expect_err("panicked")
    .downcast::<String>()
    .expect("store name");
    assert!(!exists(&name), "{name} left behind");
    let never = ScratchStore::new();
    let error = never.delete().expect_err("never written");
    assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
}

/// The logical CurrentUser `Root` includes the machine's roots, the user's
/// registry store does not; `Disallowed` reads too. Read-only.
#[test]
fn user_root_includes_machine_roots() -> io::Result<()> {
    let thumbprints = |store: &CertStore| -> io::Result<Vec<Thumbprint>> {
        Ok(store
            .certificates()?
            .certificates
            .into_iter()
            .map(|entry| entry.thumbprint)
            .collect())
    };
    let logical = thumbprints(&CertStore::open("Root", View::Logical, Access::Read)?)?;
    let registry = match CertStore::open("Root", View::UserRegistry, Access::Read) {
        Ok(store) => thumbprints(&store)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    assert!(
        registry
            .iter()
            .all(|thumbprint| logical.contains(thumbprint))
    );
    assert!(
        logical.len() > registry.len(),
        "machine roots are part of the logical store ({} vs {})",
        logical.len(),
        registry.len()
    );
    match CertStore::open("Disallowed", View::Logical, Access::Read) {
        Ok(store) => drop(store.certificates()?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

/// The window station check answers without failing; a desktop test run
/// sees a visible one, a service or SSH session does not.
#[test]
fn window_station_is_readable() -> io::Result<()> {
    interactive_desktop()?;
    Ok(())
}
