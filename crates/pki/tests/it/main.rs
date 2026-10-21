use std::{
    fs,
    sync::{Arc, RwLock},
};

use devlanes_pki::{
    CertificateAuthority, CertificateIdentity, CertificatePolicy, DynamicCertResolver,
};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Persistent roots survive restarts without changing their trust identity.
#[test]
fn persists_ca_and_protects_key() -> Result {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("ca");
    let first = CertificateAuthority::load_or_create(&path)?;
    let second = CertificateAuthority::load_or_create(&path)?;
    assert_eq!(first.certificate_der(), second.certificate_der());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path.join("ca-key.pem"))?.permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::metadata(path)?.permissions().mode() & 0o777, 0o700);
    }
    assert!(
        first
            .combined_bundle("existing roots\n")
            .starts_with("existing roots\n-----BEGIN CERTIFICATE-----")
    );
    Ok(())
}

/// Leaves carry exact, wildcard and apex SANs with a bounded validity interval.
#[test]
fn leaf_sans_and_validity() -> Result {
    let directory = tempfile::tempdir()?;
    let ca = CertificateAuthority::load_or_create(&directory.path().join("ca"))?;
    let names = vec![
        "*.lane.localhost".into(),
        "lane.localhost".into(),
        "deep.app.lane.localhost".into(),
    ];
    let key = ca.issue(&names)?;
    let (_, leaf) = x509_parser::parse_x509_certificate(&key.cert[0])?;
    let san = leaf.subject_alternative_name()?.ok_or("missing SAN")?;
    let actual: Vec<_> = san
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            x509_parser::extensions::GeneralName::DNSName(name) => Some(*name),
            _ => None,
        })
        .collect();
    assert_eq!(actual, names);
    assert!(
        leaf.validity().not_after.timestamp() - leaf.validity().not_before.timestamp()
            <= 30 * 86400 + 300
    );
    assert!(ca.issue(&["*.localhost".into()]).is_err());
    assert!(ca.issue(&["bad/name".into()]).is_err());
    Ok(())
}

/// A mismatched or interrupted pair must never silently replace a trusted CA.
#[test]
fn refuses_incomplete_and_mismatched_ca() -> Result {
    let directory = tempfile::tempdir()?;
    let a = directory.path().join("a");
    let b = directory.path().join("b");
    CertificateAuthority::load_or_create(&a)?;
    CertificateAuthority::load_or_create(&b)?;
    fs::copy(b.join("ca.pem"), a.join("ca.pem"))?;
    assert!(CertificateAuthority::load_or_create(&a).is_err());
    fs::remove_file(a.join("ca.pem"))?;
    assert!(matches!(
        CertificateAuthority::load_or_create(&a),
        Err(devlanes_pki::Error::IncompleteCa)
    ));
    Ok(())
}

/// CA import follows mkcert's filenames without invoking a trust installer.
#[test]
fn imports_mkcert_layout() -> Result {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("ca");
    let original = CertificateAuthority::load_or_create(&path)?;
    fs::rename(path.join("ca.pem"), path.join("rootCA.pem"))?;
    fs::rename(path.join("ca-key.pem"), path.join("rootCA-key.pem"))?;
    let imported = CertificateAuthority::from_mkcert(&path)?;
    assert_eq!(original.certificate_der(), imported.certificate_der());
    Ok(())
}

#[derive(Debug)]
struct Policy(RwLock<Option<u64>>);

impl CertificatePolicy for Policy {
    /// Tests mutate authorization independently of the resolver cache.
    fn lookup(&self, name: &str) -> Option<CertificateIdentity> {
        if name != "app.lane.localhost" {
            return None;
        }
        Some(CertificateIdentity {
            generation: (*self.0.read().ok()?)?,
            names: vec![name.into()],
        })
    }
}

/// Cached keys cannot authorize removed routes, and generation changes rotate leaves.
#[test]
fn resolver_cache_invalidation_and_unknown_host() -> Result {
    let directory = tempfile::tempdir()?;
    let ca = Arc::new(CertificateAuthority::load_or_create(
        &directory.path().join("ca"),
    )?);
    let policy = Arc::new(Policy(RwLock::new(Some(1))));
    let resolver = Arc::new(DynamicCertResolver::new(ca, policy.clone()));
    assert!(resolver.resolve_name("unknown.localhost").is_none());
    let first = resolver
        .resolve_name("app.lane.localhost")
        .ok_or("no certificate")?;
    let repeated = resolver
        .resolve_name("app.lane.localhost")
        .ok_or("no cached certificate")?;
    assert!(Arc::ptr_eq(&first, &repeated));
    *policy.0.write().map_err(|_| "poisoned policy")? = Some(2);
    let rotated = resolver
        .resolve_name("app.lane.localhost")
        .ok_or("no rotated certificate")?;
    assert!(!Arc::ptr_eq(&first, &rotated));
    *policy.0.write().map_err(|_| "poisoned policy")? = None;
    assert!(resolver.resolve_name("app.lane.localhost").is_none());
    let config = resolver.server_config()?;
    assert_eq!(
        config.alpn_protocols,
        [b"h2".to_vec(), b"http/1.1".to_vec()]
    );
    Ok(())
}

/// Permission and symlink checks happen before reading private key material.
#[cfg(unix)]
#[test]
fn refuses_exposed_or_symlink_keys() -> Result {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("ca");
    CertificateAuthority::load_or_create(&path)?;
    let key = path.join("ca-key.pem");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644))?;
    assert!(matches!(
        CertificateAuthority::load_or_create(&path),
        Err(devlanes_pki::Error::InsecurePath)
    ));
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600))?;
    fs::rename(&key, path.join("real-key"))?;
    symlink(path.join("real-key"), &key)?;
    assert!(matches!(
        CertificateAuthority::load_or_create(&path),
        Err(devlanes_pki::Error::InsecurePath)
    ));
    Ok(())
}
