use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject as _;
use sha1::{Digest as _, Sha1};
use sha2::Sha256;

use crate::TrustError;

/// The public CA certificate plus the name stores file it under.
///
/// Only the first certificate of the input PEM is kept and re-encoded, so a
/// bundle or a stray private key can never reach a trust store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaCertificate {
    name: String,
    pem: String,
    der: Vec<u8>,
}

impl CaCertificate {
    /// Parses the first `CERTIFICATE` block of `pem`.
    ///
    /// `name` becomes the NSS nickname and the Linux anchor file name, so it must be
    /// stable across runs for removal to find the certificate again.
    pub fn from_pem(pem: &str, name: impl Into<String>) -> Result<Self, TrustError> {
        let name = name.into();
        if name.trim().is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
            return Err(TrustError::InvalidCertificate(format!(
                "name `{name}` must be 1 to 64 printable characters"
            )));
        }
        let der = CertificateDer::from_pem_slice(pem.as_bytes())
            .map_err(|error| TrustError::InvalidCertificate(error.to_string()))?;
        Ok(Self::from_der_unchecked(name, der.as_ref().to_vec()))
    }

    /// Builds the canonical PEM for already decoded DER.
    fn from_der_unchecked(name: String, der: Vec<u8>) -> Self {
        let encoded = STANDARD.encode(&der);
        let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
        // PEM lines are 64 characters (RFC 7468); base64 output is ASCII so byte chunks are safe.
        for line in encoded.as_bytes().chunks(64) {
            pem.push_str(&String::from_utf8_lossy(line));
            pem.push('\n');
        }
        pem.push_str("-----END CERTIFICATE-----\n");
        Self { name, pem, der }
    }

    /// The store-facing name (NSS nickname).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Canonical single-certificate PEM.
    pub fn pem(&self) -> &str {
        &self.pem
    }

    /// DER bytes of the certificate.
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// Uppercase SHA-1 fingerprint, the form `security delete-certificate -Z` prints and takes.
    pub fn sha1_hex(&self) -> String {
        hex::encode_upper(Sha1::digest(&self.der))
    }

    /// Uppercase SHA-256 fingerprint, for display.
    pub fn sha256_hex(&self) -> String {
        hex::encode_upper(Sha256::digest(&self.der))
    }

    /// File-name-safe slug of the name, used for Linux anchor files.
    pub fn file_stem(&self) -> String {
        let mut stem = String::with_capacity(self.name.len());
        for c in self.name.chars() {
            if c.is_ascii_alphanumeric() {
                stem.push(c.to_ascii_lowercase());
            } else if !stem.ends_with('-') {
                stem.push('-');
            }
        }
        let stem = stem.trim_matches('-');
        if stem.is_empty() {
            "devlanes-ca".into()
        } else {
            stem.into()
        }
    }

    /// Whether any certificate in `pem` is this CA, compared by DER.
    pub fn matches_pem(&self, pem: &[u8]) -> bool {
        CertificateDer::pem_slice_iter(pem)
            .filter_map(Result::ok)
            .any(|cert| cert.as_ref() == self.der.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ca_pem as ca_pem;

    #[test]
    fn keeps_only_the_first_certificate() {
        let pem = ca_pem();
        let bundle = format!("{pem}-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n");
        let ca = CaCertificate::from_pem(&bundle, "devlanes CA").expect("parse");
        assert!(!ca.pem().contains("PRIVATE KEY"));
        assert!(ca.matches_pem(pem.as_bytes()));
        assert_eq!(
            CaCertificate::from_pem(ca.pem(), "devlanes CA").expect("reparse"),
            ca
        );
        assert_eq!(ca.sha1_hex().len(), 40);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(CaCertificate::from_pem("not pem", "x").is_err());
        assert!(CaCertificate::from_pem(&ca_pem(), " ").is_err());
        assert!(CaCertificate::from_pem(&ca_pem(), "a\nb").is_err());
    }

    #[test]
    fn file_stem_is_a_slug() {
        let ca =
            CaCertificate::from_pem(&ca_pem(), "devlanes development CA (me@box)").expect("parse");
        assert_eq!(ca.file_stem(), "devlanes-development-ca-me-box");
        let ca = CaCertificate::from_pem(ca.pem(), "()").expect("parse");
        assert_eq!(ca.file_stem(), "devlanes-ca");
    }
}
