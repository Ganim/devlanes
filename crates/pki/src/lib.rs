//! Per-user CA storage and routed-host-only TLS certificates. No trust-store writes.

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("CA filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("certificate generation failed: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("TLS configuration failed: {0}")]
    Tls(#[from] rustls::Error),
    #[error("CA directory contains an incomplete certificate/key pair")]
    IncompleteCa,
    #[error("CA key or directory must be a regular non-symlink path with private permissions")]
    InsecurePath,
    #[error("CA certificate is invalid, expired, or lacks signing authority")]
    InvalidCa,
    #[error("certificate SAN list must contain valid DNS names")]
    InvalidNames,
}

/// Owns an ECDSA signing key without exposing it through Debug or accessors.
pub struct CertificateAuthority {
    issuer: Issuer<'static, KeyPair>,
    certificate: CertificateDer<'static>,
    pem: String,
    not_after: time::OffsetDateTime,
}

impl std::fmt::Debug for CertificateAuthority {
    /// Avoids rendering private signing material in diagnostics.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertificateAuthority")
            .finish_non_exhaustive()
    }
}

impl CertificateAuthority {
    /// Loads or creates `ca.pem` and `ca-key.pem` under a private user directory.
    ///
    /// This performs blocking I/O; async callers must use `spawn_blocking`.
    /// Incomplete pairs are refused rather than silently replacing a trusted CA.
    pub fn load_or_create(directory: &Path) -> Result<Self, Error> {
        create_private_directory(directory)?;
        let lock_path = directory.join(".lock");
        match fs::symlink_metadata(&lock_path) {
            Ok(_) => check_private(&lock_path, false)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(lock_path)?;
        fs2::FileExt::lock_exclusive(&lock)?;
        let certificate = directory.join("ca.pem");
        let key = directory.join("ca-key.pem");
        match (certificate.try_exists()?, key.try_exists()?) {
            (true, true) => Self::load(&certificate, &key),
            (false, false) => {
                let key_pair = KeyPair::generate()?;
                let mut params = CertificateParams::new(Vec::<String>::new())?;
                let now = time::OffsetDateTime::now_utc();
                params.not_before = now - time::Duration::days(1);
                params.not_after = now + time::Duration::days(3650);
                params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
                params.key_usages = vec![
                    KeyUsagePurpose::KeyCertSign,
                    KeyUsagePurpose::CrlSign,
                    KeyUsagePurpose::DigitalSignature,
                ];
                params
                    .distinguished_name
                    .push(DnType::CommonName, "devlanes local development CA");
                let cert = params.self_signed(&key_pair)?;
                write_new(&key, key_pair.serialize_pem().as_bytes())?;
                write_new(&certificate, cert.pem().as_bytes())?;
                Self::load(&certificate, &key)
            }
            _ => Err(Error::IncompleteCa),
        }
    }

    /// Reuses a caller-selected mkcert CAROOT without invoking mkcert or changing trust.
    pub fn from_mkcert(caroot: &Path) -> Result<Self, Error> {
        Self::load(&caroot.join("rootCA.pem"), &caroot.join("rootCA-key.pem"))
    }

    /// Loads and validates an existing CA and protected key on a blocking thread.
    pub fn load(certificate_path: &Path, key_path: &Path) -> Result<Self, Error> {
        check_private(key_path, false)?;
        let pem = fs::read_to_string(certificate_path)?;
        let key = KeyPair::from_pem(&fs::read_to_string(key_path)?)?;
        let certificate = rustls_pemfile::certs(&mut pem.as_bytes())
            .next()
            .transpose()?
            .ok_or(Error::InvalidCa)?;
        let (_, parsed) =
            x509_parser::parse_x509_certificate(&certificate).map_err(|_| Error::InvalidCa)?;
        if !parsed.is_ca()
            || !parsed.validity().is_valid()
            || parsed
                .key_usage()
                .map_err(|_| Error::InvalidCa)?
                .is_some_and(|usage| !usage.value.key_cert_sign())
        {
            return Err(Error::InvalidCa);
        }
        let not_after =
            time::OffsetDateTime::from_unix_timestamp(parsed.validity().not_after.timestamp())
                .map_err(|_| Error::InvalidCa)?;
        // rustls verifies that the stored certificate and private key match.
        CertifiedKey::from_der(
            vec![certificate.clone()],
            PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            &rustls::crypto::ring::default_provider(),
        )?;
        let issuer = Issuer::from_ca_cert_pem(&pem, key)?;
        Ok(Self {
            issuer,
            certificate,
            pem,
            not_after,
        })
    }

    /// Supplies the public root for client trust configuration.
    pub fn certificate_der(&self) -> CertificateDer<'static> {
        self.certificate.clone()
    }

    /// Supplies the public root for CLI-side trust installers and runtime bundles.
    pub fn certificate_pem(&self) -> &str {
        &self.pem
    }

    /// Appends the local root to an existing PEM bundle; callers choose its user-owned path.
    pub fn combined_bundle(&self, existing_roots: &str) -> String {
        format!("{}\n{}", existing_roots.trim_end(), self.pem)
    }

    /// Mints a short-lived leaf. Route authorization belongs to `CertificatePolicy`.
    pub fn issue(&self, names: &[String]) -> Result<Arc<CertifiedKey>, Error> {
        if names.is_empty() || names.iter().any(|name| !valid_dns_name(name)) {
            return Err(Error::InvalidNames);
        }
        let now = time::OffsetDateTime::now_utc();
        if self.not_after <= now {
            return Err(Error::InvalidCa);
        }
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(names.to_vec())?;
        params.not_before = now - time::Duration::minutes(5);
        params.not_after = (now + time::Duration::days(30)).min(self.not_after);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = params.signed_by(&key, &self.issuer)?;
        let certified = CertifiedKey::from_der(
            vec![certificate.der().clone(), self.certificate.clone()],
            PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            &rustls::crypto::ring::default_provider(),
        )?;
        Ok(Arc::new(certified))
    }
}

/// A policy result must come from one immutable routing snapshot.
#[derive(Clone, Debug)]
pub struct CertificateIdentity {
    pub generation: u64,
    pub names: Vec<String>,
}

/// Implemented by the routing owner; unknown names must return `None`.
pub trait CertificatePolicy: Send + Sync + std::fmt::Debug {
    /// Called synchronously during a TLS handshake, without filesystem I/O.
    fn lookup(&self, server_name: &str) -> Option<CertificateIdentity>;
}

#[derive(Debug)]
struct CachedKey {
    issued: Instant,
    names: Vec<String>,
    key: Arc<CertifiedKey>,
}

#[derive(Debug, Default)]
struct Cache {
    generation: Option<u64>,
    keys: HashMap<String, CachedKey>,
}

/// Synchronous on-demand issuance with bounded memory and route-generation invalidation.
#[derive(Debug)]
pub struct DynamicCertResolver {
    ca: Arc<CertificateAuthority>,
    policy: Arc<dyn CertificatePolicy>,
    cache: Mutex<Cache>,
}

impl DynamicCertResolver {
    /// Uses a policy that is rechecked even on certificate-cache hits.
    pub fn new(ca: Arc<CertificateAuthority>, policy: Arc<dyn CertificatePolicy>) -> Self {
        Self {
            ca,
            policy,
            cache: Mutex::new(Cache::default()),
        }
    }

    /// Resolves a routed DNS name; unknown hosts and issuance failures refuse TLS.
    pub fn resolve_name(&self, name: &str) -> Option<Arc<CertifiedKey>> {
        if self.ca.not_after <= time::OffsetDateTime::now_utc() {
            return None;
        }
        let name = name.to_ascii_lowercase();
        let identity = self.policy.lookup(&name)?;
        let mut cache = self.cache.lock().ok()?;
        if cache.generation != Some(identity.generation) {
            cache.keys.clear();
            cache.generation = Some(identity.generation);
        }
        if let Some(entry) = cache.keys.get(&name)
            && entry.issued.elapsed() < Duration::from_secs(24 * 3600)
            && entry.names == identity.names
        {
            return Some(entry.key.clone());
        }
        let key = self.ca.issue(&identity.names).ok()?;
        if cache.keys.len() >= 256 {
            cache.keys.clear();
        }
        cache.keys.insert(
            name,
            CachedKey {
                issued: Instant::now(),
                names: identity.names,
                key: key.clone(),
            },
        );
        Some(key)
    }

    /// Uses an explicit ring provider, avoiding process-global provider races.
    pub fn server_config(self: Arc<Self>) -> Result<Arc<ServerConfig>, Error> {
        let mut config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_no_client_auth()
                .with_cert_resolver(self);
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }
}

impl ResolvesServerCert for DynamicCertResolver {
    /// Missing SNI is refused because it cannot authorize a routed hostname.
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.resolve_name(client_hello.server_name()?)
    }
}

/// Keeps SANs syntactically DNS-only and refuses ineffective public-suffix wildcards.
fn valid_dns_name(name: &str) -> bool {
    let host = name.strip_prefix("*.").unwrap_or(name);
    (!name.starts_with("*.") || host.contains('.'))
        && !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// Existing directories are checked, never silently chmodded.
fn create_private_directory(path: &Path) -> Result<(), Error> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => check_private(path, true),
        Err(error) => Err(error.into()),
    }
}

/// Rejects symlinks and group/world access before secret material is opened.
fn check_private(path: &Path, directory: bool) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(Error::InsecurePath);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::InsecurePath);
        }
    }
    Ok(())
}

/// A private tempfile prevents partially written keys from becoming the live pair.
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("CA path has no parent"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    Ok(())
}
