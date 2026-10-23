//! The roots of `bundle.pem` on Windows.

use std::collections::HashSet;
use std::io;

use base64::Engine as _;
use devlanes_sys_windows::cert::{Access, CertStore, Listing, StoreCertificate, Thumbprint, View};
use sha1::{Digest as _, Sha1};

use crate::{DISALLOWED, ROOT};

/// The roots for `bundle.pem` and what kept a store from contributing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BundleRoots {
    /// PEM certificates, each once.
    pub pem: String,
    /// One line per store that could not be read, for the daemon's log.
    pub problems: Vec<String>,
}

/// Mozilla's public roots plus every root in the user's logical `Root` store
/// (machine roots included, so enterprise and custom roots come along) that
/// is valid now and usable for server authentication, minus anything in
/// `Disallowed` and minus `own` (the devlanes CA, which the caller appends).
/// Blocking.
///
/// Windows fetches most public roots on first use, so the store alone lacks
/// many that Python's certifi has, and programs pointed at the bundle would
/// lose them. A root the store lists but refuses (purposes disabled, expired)
/// is left out even when Mozilla's set has it.
pub fn bundle_roots(own: &[u8]) -> BundleRoots {
    let mut problems = Vec::new();
    let store = match read(ROOT) {
        Ok(listing) => {
            if let Some(first) = listing.unreadable.first() {
                problems.push(format!(
                    r"left {} certificates of Cert:\CurrentUser\{ROOT} out of bundle.pem because their usage could not be read: {}",
                    listing.unreadable.len(),
                    first.error
                ));
            }
            store_roots(listing)
        }
        Err(error) => {
            problems.push(format!(
                r"cannot read Cert:\CurrentUser\{ROOT}, so bundle.pem has Mozilla's roots only: {error}"
            ));
            Vec::new()
        }
    };
    let disallowed = match read(DISALLOWED) {
        Ok(listing) => thumbprints(&listing),
        Err(error) => {
            problems.push(format!(
                r"cannot read Cert:\CurrentUser\{DISALLOWED}, so distrusted roots are not left out of bundle.pem: {error}"
            ));
            HashSet::new()
        }
    };
    let builtin = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|certificate| certificate.as_ref().to_vec());
    BundleRoots {
        pem: union_roots(builtin, &store, &disallowed, own),
        problems,
    }
}

/// Every certificate of the logical CurrentUser store `name`; a store that
/// does not exist is empty.
fn read(name: &str) -> io::Result<Listing> {
    match CertStore::open(name, View::Logical, Access::Read) {
        Ok(store) => store.certificates(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Listing::default()),
        Err(error) => Err(error),
    }
}

/// The `Root` listing as store entries. Certificates whose usage could not be
/// read count as refused, so they stay out of the bundle even when Mozilla's
/// set has them, as Windows would not use them either.
fn store_roots(listing: Listing) -> Vec<StoreCertificate> {
    let refused = listing
        .unreadable
        .into_iter()
        .map(|entry| StoreCertificate {
            thumbprint: Sha1::digest(&entry.der).into(),
            der: entry.der,
            server_auth: false,
            time_valid: false,
        });
    listing.certificates.into_iter().chain(refused).collect()
}

/// Thumbprints of every certificate of `listing`. For the ones Windows could
/// not read the thumbprint comes from the encoding, so no distrusted root
/// slips into the bundle.
fn thumbprints(listing: &Listing) -> HashSet<Thumbprint> {
    listing
        .certificates
        .iter()
        .map(|entry| entry.thumbprint)
        .chain(
            listing
                .unreadable
                .iter()
                .map(|entry| Sha1::digest(&entry.der).into()),
        )
        .collect()
}

/// `builtin` and the usable `store` roots as PEM, each once, without `own`,
/// without store roots that are not usable and without `disallowed` ones.
fn union_roots(
    builtin: impl IntoIterator<Item = Vec<u8>>,
    store: &[StoreCertificate],
    disallowed: &HashSet<Thumbprint>,
    own: &[u8],
) -> String {
    let usable = |entry: &StoreCertificate| entry.server_auth && entry.time_valid;
    let refused: HashSet<&[u8]> = store
        .iter()
        .filter(|entry| !usable(entry))
        .map(|entry| entry.der.as_slice())
        .collect();
    let mut seen: HashSet<Vec<u8>> = HashSet::from([own.to_vec()]);
    builtin
        .into_iter()
        .chain(
            store
                .iter()
                .filter(|entry| usable(entry))
                .map(|entry| entry.der.clone()),
        )
        .filter(|der| !refused.contains(der.as_slice()))
        .filter(|der| !disallowed.contains(&<Thumbprint>::from(Sha1::digest(der))))
        .filter(|der| seen.insert(der.clone()))
        .map(|der| pem_certificate(&der))
        .collect()
}

/// `der` as a PEM certificate block with 64-character lines.
fn pem_certificate(der: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in encoded.as_bytes().chunks(64) {
        pem.push_str(&String::from_utf8_lossy(line));
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    pem
}

#[cfg(test)]
mod tests {
    use devlanes_sys_windows::cert::Unreadable;

    use super::*;

    /// A store entry for `der`.
    fn entry(der: &[u8], server_auth: bool, time_valid: bool) -> StoreCertificate {
        StoreCertificate {
            der: der.to_vec(),
            thumbprint: Sha1::digest(der).into(),
            server_auth,
            time_valid,
        }
    }

    /// The bundle is Mozilla's roots and the usable store roots, minus store
    /// roots Windows refuses (also when Mozilla has them), distrusted ones and
    /// the devlanes CA, each once.
    #[test]
    fn union_of_mozilla_and_the_store() {
        let cert = |n: u8| vec![0x30, 0x03, 0x02, 0x01, n];
        let (mozilla, both, disabled, enterprise, expired, distrusted, own) = (
            cert(1),
            cert(2),
            cert(3),
            cert(4),
            cert(5),
            cert(6),
            cert(7),
        );
        let disallowed = HashSet::from([<Thumbprint>::from(Sha1::digest(&distrusted))]);
        let pem = union_roots(
            [
                mozilla.clone(),
                both.clone(),
                disabled.clone(),
                distrusted.clone(),
                own.clone(),
            ],
            &[
                entry(&both, true, true),
                entry(&disabled, false, true),
                entry(&enterprise, true, true),
                entry(&expired, true, false),
                entry(&distrusted, true, true),
                entry(&own, true, true),
                entry(&enterprise, true, true),
            ],
            &disallowed,
            &own,
        );
        let expected: String = [mozilla, both, enterprise]
            .iter()
            .map(|der| pem_certificate(der))
            .collect();
        assert_eq!(pem, expected);
    }

    /// A certificate Windows could not read stays out of the bundle when it
    /// is a root, even one Mozilla has, and is still distrusted when it is
    /// in `Disallowed`; the readable roots stay in.
    #[test]
    fn unreadable_certificates_are_left_out() {
        let cert = |n: u8| vec![0x30, 0x03, 0x02, 0x01, n];
        let (kept, unjudged, distrusted) = (cert(1), cert(2), cert(3));
        let unreadable = |der: &[u8]| Unreadable {
            der: der.to_vec(),
            error: io::Error::other("bad usage"),
        };
        let root = Listing {
            certificates: vec![entry(&kept, true, true), entry(&distrusted, true, true)],
            unreadable: vec![unreadable(&unjudged)],
        };
        let disallowed = Listing {
            certificates: Vec::new(),
            unreadable: vec![unreadable(&distrusted)],
        };
        let pem = union_roots(
            [unjudged.clone()],
            &store_roots(root),
            &thumbprints(&disallowed),
            &[],
        );
        assert_eq!(pem, pem_certificate(&kept));
    }

    /// PEM blocks wrap at 64 characters and round-trip through the parser
    /// the trust crate uses.
    #[test]
    fn pem_blocks_parse() {
        let key = rcgen::KeyPair::generate().expect("key");
        let params = rcgen::CertificateParams::new(vec!["a.localhost".into()]).expect("params");
        let der = params.self_signed(&key).expect("cert").der().to_vec();
        let pem = pem_certificate(&der);
        assert!(pem.lines().all(|line| line.len() <= 64), "{pem}");
        let parsed = devlanes_trust::CaCertificate::from_pem(&pem, "root").expect("parse");
        assert_eq!(parsed.der(), der.as_slice());
    }

    /// The real stores give a bundle with Mozilla's roots (ISRG Root X1 among
    /// them, which no Disallowed store holds) and without the given CA.
    /// Read-only.
    #[test]
    fn real_bundle_has_mozilla_roots() {
        let isrg = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .find(|cert| {
                let name = b"ISRG Root X1";
                cert.windows(name.len()).any(|bytes| bytes == name)
            })
            .expect("ISRG Root X1 in Mozilla's set");
        let roots = bundle_roots(&[]);
        assert!(roots.problems.is_empty(), "{:?}", roots.problems);
        assert!(roots.pem.contains(&pem_certificate(isrg)));
        let without = bundle_roots(isrg);
        assert!(!without.pem.contains(&pem_certificate(isrg)));
    }
}
