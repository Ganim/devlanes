//! Per-user CA storage and routed-host-only TLS certificates. No trust-store writes.

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
    GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose, NameConstraints, SanType,
};

/// DNS subtrees every newly created devlanes CA may sign for. A configured TLD is added at creation.
pub const PERMITTED_DOMAINS: &[&str] = &["localhost", "test", "internal", "home.arpa"];
/// Common name of new CAs. macOS uses it as the keychain label that `certs untrust` searches.
const CA_COMMON_NAME: &str = "devlanes local development CA";
/// Organization in the subject of every certificate devlanes creates.
const ORGANIZATION: &str = "devlanes";
/// Leaf organizational unit when the routing owner names no project and lane.
const DEFAULT_UNIT: &str = "local development";
/// Shown as a URI subject alternative name on new CAs, which certificate viewers list.
const PROJECT_URL: &str = "https://devlanes.run";
/// RFC 5280 upper bound, in characters, for common name and organizational unit values.
const NAME_BOUND: usize = 64;
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
    #[cfg(unix)]
    #[error("CA key or directory must be a regular non-symlink path with private permissions")]
    InsecurePath,
    /// A CA directory or key file that others could read or swap; `fix` is a
    /// command that makes it private, when one is known. Access lists are hard
    /// to read by hand, so Windows names the path, the problem and the fix.
    #[cfg(windows)]
    #[error("CA path {} is not private: {problem}{}", .path.display(), fix_suffix(.fix))]
    InsecurePath {
        path: std::path::PathBuf,
        problem: PathProblem,
        fix: Option<String>,
    },
    #[error("CA certificate is invalid, expired, or lacks signing authority")]
    InvalidCa,
    #[error("certificate SAN list must contain valid DNS names")]
    InvalidNames,
}

/// `; fix: <command>` for [`Error::InsecurePath`]'s message, or nothing.
#[cfg(windows)]
fn fix_suffix(fix: &Option<String>) -> String {
    fix.as_ref()
        .map(|fix| format!("; fix: {fix}"))
        .unwrap_or_default()
}

/// Why a CA path is not private.
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathProblem {
    /// A symlink, junction or other reparse point, which could point anywhere.
    Link,
    /// Something other than a directory where the CA directory should be.
    NotADirectory,
    /// Something other than a regular file where a key or lock file should be.
    NotAFile,
    /// The file has this many names, so it can be reached or swapped elsewhere.
    HardLinks(u32),
    /// Another account owns it.
    Owner,
    /// Other accounts can open it, as `Name (SID)` or a bare SID; empty when
    /// its access list cannot be judged.
    Accounts(Vec<String>),
}

#[cfg(windows)]
impl std::fmt::Display for PathProblem {
    /// A clause for [`Error::InsecurePath`]'s message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Link => f.write_str("it is a symlink or junction"),
            Self::NotADirectory => f.write_str("it is not a directory"),
            Self::NotAFile => f.write_str("it is not a regular file"),
            Self::HardLinks(links) => write!(f, "it has {links} hard links"),
            Self::Owner => f.write_str("another account owns it"),
            Self::Accounts(accounts) if accounts.is_empty() => {
                f.write_str("its access list grants access that cannot be judged")
            }
            Self::Accounts(accounts) => {
                write!(f, "other accounts can open it: {}", accounts.join(", "))
            }
        }
    }
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
    /// Same as [`Self::load_or_create_with_tld`] without a configured TLD.
    pub fn load_or_create(directory: &Path) -> Result<Self, Error> {
        Self::load_or_create_with_tld(directory, None)
    }

    /// Loads or creates the CA; a new CA is name-constrained to [`PERMITTED_DOMAINS`] plus `tld`.
    ///
    /// This performs blocking I/O; async callers must use `spawn_blocking`. An existing CA
    /// is loaded unchanged, so a TLD configured later needs a recreated CA. A certificate
    /// without its key is refused rather than silently replacing a trusted CA.
    pub fn load_or_create_with_tld(directory: &Path, tld: Option<&str>) -> Result<Self, Error> {
        let permitted = permitted_domains(tld)?;
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
            (true, false) => Err(Error::IncompleteCa),
            // The certificate is renamed into place last, so a key without one comes from an
            // interrupted creation (or a deleted root) and no client can trust it yet.
            (false, _) => {
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
                params.distinguished_name = subject(Some(CA_COMMON_NAME), &machine_label());
                // A CA is not checked against its own name constraints, so the URI stays valid.
                params.subject_alt_names = vec![SanType::URI(PROJECT_URL.try_into()?)];
                // A leaked key can then only impersonate development names, never public sites.
                params.name_constraints = Some(NameConstraints {
                    permitted_subtrees: permitted
                        .into_iter()
                        .map(GeneralSubtree::DnsName)
                        .collect(),
                    excluded_subtrees: Vec::new(),
                });
                let cert = params.self_signed(&key_pair)?;
                write_private(&key, key_pair.serialize_pem().as_bytes(), true)?;
                // The key rename must be durable before the certificate can appear without it.
                sync_directory(directory)?;
                write_private(&certificate, cert.pem().as_bytes(), false)?;
                sync_directory(directory)?;
                Self::load(&certificate, &key)
            }
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

    /// The DNS subtrees the root's name constraints permit, lowercased, or `None`
    /// when it has none (mkcert roots): then any host name can be certified.
    /// Read from the certificate itself, so a CA created before a TLD was
    /// configured reports what it really allows.
    pub fn permitted_dns_names(&self) -> Option<Vec<String>> {
        let (_, parsed) = x509_parser::parse_x509_certificate(&self.certificate).ok()?;
        let constraints = parsed.name_constraints().ok().flatten()?;
        let subtrees = constraints.value.permitted_subtrees.as_ref()?;
        Some(
            subtrees
                .iter()
                .filter_map(|subtree| match &subtree.base {
                    x509_parser::extensions::GeneralName::DNSName(name) => {
                        Some(name.trim_start_matches('.').to_ascii_lowercase())
                    }
                    _ => None,
                })
                .collect(),
        )
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
    ///
    /// The subject names the first SAN and `owner` (a `project@lane` reference), so a
    /// browser's certificate viewer shows what the leaf is for.
    pub fn issue(&self, names: &[String], owner: Option<&str>) -> Result<Arc<CertifiedKey>, Error> {
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
        params.distinguished_name = subject(
            names.first().map(String::as_str),
            owner
                .filter(|owner| !owner.is_empty())
                .unwrap_or(DEFAULT_UNIT),
        );
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
    /// SANs; the first becomes the leaf's common name.
    pub names: Vec<String>,
    /// Project and lane that own the name, such as `acme@0`, shown in the leaf's subject.
    pub owner: Option<String>,
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
    owner: Option<String>,
    key: Arc<CertifiedKey>,
}

#[derive(Debug, Default)]
struct Cache {
    generation: Option<u64>,
    keys: HashMap<String, CachedKey>,
}

impl Cache {
    /// Returns a cached key only while it is young and still carries the authorized SANs
    /// and owner; an owner change alone does not bump the route generation.
    fn fresh(&self, name: &str, identity: &CertificateIdentity) -> Option<Arc<CertifiedKey>> {
        self.keys
            .get(name)
            .filter(|entry| {
                entry.issued.elapsed() < Duration::from_secs(24 * 3600)
                    && entry.names == identity.names
                    && entry.owner == identity.owner
            })
            .map(|entry| entry.key.clone())
    }
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
        {
            let mut cache = self.cache();
            if cache.generation != Some(identity.generation) {
                cache.keys.clear();
                cache.generation = Some(identity.generation);
            }
            if let Some(key) = cache.fresh(&name, &identity) {
                return Some(key);
            }
        }
        // Key generation and signing run unlocked so one miss does not stall every handshake.
        let key = self
            .ca
            .issue(&identity.names, identity.owner.as_deref())
            .ok()?;
        let mut cache = self.cache();
        // Routes changed while minting: serve this handshake but leave the new generation's cache alone.
        if cache.generation != Some(identity.generation) {
            return Some(key);
        }
        if let Some(winner) = cache.fresh(&name, &identity) {
            return Some(winner);
        }
        if cache.keys.len() >= 256 {
            cache.keys.clear();
        }
        cache.keys.insert(
            name,
            CachedKey {
                issued: Instant::now(),
                names: identity.names,
                owner: identity.owner,
                key: key.clone(),
            },
        );
        Some(key)
    }

    /// A panic while holding the lock leaves a usable map; refusing it would disable TLS forever.
    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
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

/// Builds an O, OU, CN subject. A common name over the X.509 bound is left out
/// (the SAN still carries it); a long unit is cut, since it is only informative.
fn subject(common_name: Option<&str>, unit: &str) -> DistinguishedName {
    let mut name = DistinguishedName::new();
    name.push(DnType::OrganizationName, ORGANIZATION);
    name.push(
        DnType::OrganizationalUnitName,
        unit.chars().take(NAME_BOUND).collect::<String>(),
    );
    if let Some(common_name) = common_name.filter(|cn| cn.chars().count() <= NAME_BOUND) {
        name.push(DnType::CommonName, common_name);
    }
    name
}

/// `user@host` of the creating account, so CAs from several machines can be told apart.
#[cfg(unix)]
fn machine_label() -> String {
    let user = ["USER", "LOGNAME"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|user| !user.is_empty())
        .unwrap_or_else(|| rustix::process::getuid().as_raw().to_string());
    let uname = rustix::system::uname();
    format!("{user}@{}", uname.nodename().to_string_lossy())
}

/// `user@host` of the creating account, so CAs from several machines can be
/// told apart. Windows sets `USERNAME` and `COMPUTERNAME` for every logon;
/// `USER` is what Git Bash and other Unix-like shells may set instead.
#[cfg(windows)]
fn machine_label() -> String {
    let var = |keys: &[&str]| {
        keys.iter()
            .filter_map(|key| std::env::var(key).ok())
            .find(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".to_owned())
    };
    format!("{}@{}", var(&["USERNAME", "USER"]), var(&["COMPUTERNAME"]))
}

/// Creates the directory only the owner can enter. Existing directories are
/// checked, never silently chmodded.
#[cfg(unix)]
fn create_private_directory(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::DirBuilderExt;
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => check_private(path, true),
        Err(error) => Err(error.into()),
    }
}

/// Creates the directory with an access list for this user, SYSTEM and
/// Administrators, which the key and certificate inherit. An existing one must
/// already be private, a real directory and this user's; its access list is
/// never changed here.
#[cfg(windows)]
fn create_private_directory(path: &Path) -> Result<(), Error> {
    match devlanes_sys::windows::security::ensure_private_dir(path, false)? {
        Ok(_) => Ok(()),
        Err(problem) => Err(insecure_directory(path, problem)),
    }
}

/// Rejects symlinks and group/world access before secret material is opened.
#[cfg(unix)]
fn check_private(path: &Path, directory: bool) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(Error::InsecurePath);
    }
    Ok(())
}

/// Rejects a directory that is a reparse point (symlinks and junctions, which
/// could point anywhere), is another account's or that others can open; for a
/// file, its directory first (the directory's fix also resets the files inside),
/// then the file itself the same way plus extra hard links, which an explicit
/// grant or a second name in another directory would expose.
#[cfg(windows)]
fn check_private(path: &Path, directory: bool) -> Result<(), Error> {
    use devlanes_sys::windows::security::{FileProblem, check_private_dir, check_private_file};
    let parent = if directory { Some(path) } else { path.parent() };
    // A bare file name sits in the working directory.
    let parent = match parent {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    if let Err(problem) = check_private_dir(parent)? {
        return Err(insecure_directory(parent, problem));
    }
    if directory {
        return Ok(());
    }
    let (problem, fix) = match check_private_file(path)? {
        Ok(()) => return Ok(()),
        Err(FileProblem::NotAFile) => (PathProblem::NotAFile, None),
        Err(FileProblem::ReparsePoint) => (PathProblem::Link, None),
        Err(FileProblem::HardLinks(links)) => (PathProblem::HardLinks(links), None),
        Err(FileProblem::Owner) => (PathProblem::Owner, None),
        // The directory is private by now, so inheriting from it is enough.
        Err(FileProblem::Acl(others)) => (
            accounts(&others),
            Some(devlanes_sys::windows::security::private_file_fix(path)),
        ),
    };
    Err(Error::InsecurePath {
        path: path.to_owned(),
        problem,
        fix,
    })
}

/// [`Error::InsecurePath`] for a CA directory, with the `icacls` commands the
/// daemon's home check also gives when other accounts can open it.
#[cfg(windows)]
fn insecure_directory(path: &Path, problem: devlanes_sys::windows::security::DirProblem) -> Error {
    use devlanes_sys::windows::security::{DirProblem, private_dir_fix};
    let (problem, fix) = match problem {
        DirProblem::NotADirectory => (PathProblem::NotADirectory, None),
        DirProblem::ReparsePoint => (PathProblem::Link, None),
        DirProblem::Owner => (PathProblem::Owner, None),
        DirProblem::Acl(others) => (accounts(&others), private_dir_fix(path, &others).ok()),
    };
    Error::InsecurePath {
        path: path.to_owned(),
        problem,
        fix,
    }
}

/// [`PathProblem::Accounts`] with readable names for well-known groups.
#[cfg(windows)]
fn accounts(sids: &[String]) -> PathProblem {
    PathProblem::Accounts(
        sids.iter()
            .map(|sid| devlanes_sys::windows::security::account_label(sid))
            .collect(),
    )
}

/// Builds the permitted subtrees for a new CA, adding a normalized TLD when it is not covered.
fn permitted_domains(tld: Option<&str>) -> Result<Vec<String>, Error> {
    let mut domains: Vec<String> = PERMITTED_DOMAINS.iter().map(|&d| d.to_owned()).collect();
    if let Some(tld) = tld.map(|tld| tld.trim_matches('.').to_ascii_lowercase())
        && !tld.is_empty()
    {
        if tld.starts_with("*.") || !valid_dns_name(&tld) {
            return Err(Error::InvalidNames);
        }
        if !domains.contains(&tld) {
            domains.push(tld);
        }
    }
    Ok(domains)
}

/// A private tempfile prevents partially written files from becoming part of the live pair.
///
/// `replace` is only for the key, which may be an orphan left by an interrupted creation.
fn write_private(path: &Path, bytes: &[u8], replace: bool) -> Result<(), Error> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("CA path has no parent"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    // On Windows a scanner reading the fresh temp file, or a tool reading the
    // old key, briefly blocks the rename; a failed persist hands the temp back.
    let mut temp = Some(file.into_temp_path());
    devlanes_sys::retrying(|| {
        let current = temp
            .take()
            .ok_or_else(|| io::Error::other("temporary file already persisted"))?;
        let persisted = if replace {
            current.persist(path)
        } else {
            current.persist_noclobber(path)
        };
        persisted.map_err(|error| {
            temp = Some(error.path);
            error.error
        })
    })?;
    Ok(())
}

/// Makes completed renames durable so a power loss cannot reorder the key and certificate.
fn sync_directory(directory: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    fs::File::open(directory)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct One;

    impl CertificatePolicy for One {
        /// Authorizes a single name at a fixed generation.
        fn lookup(&self, name: &str) -> Option<CertificateIdentity> {
            (name == "app.lane.localhost").then(|| CertificateIdentity {
                generation: 1,
                names: vec![name.into()],
                owner: None,
            })
        }
    }

    /// A thread that panicked while holding the cache lock must not disable TLS.
    #[test]
    fn poisoned_cache_still_resolves() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let ca = Arc::new(CertificateAuthority::load_or_create(
            &directory.path().join("ca"),
        )?);
        let resolver = Arc::new(DynamicCertResolver::new(ca, Arc::new(One)));
        let poisoner = resolver.clone();
        let panicked = std::thread::spawn(move || {
            let _guard = poisoner.cache.lock();
            panic!("poison the certificate cache");
        })
        .join();
        assert!(panicked.is_err());
        assert!(resolver.cache.is_poisoned());
        let first = resolver
            .resolve_name("app.lane.localhost")
            .ok_or("no certificate")?;
        let cached = resolver
            .resolve_name("app.lane.localhost")
            .ok_or("no cached certificate")?;
        assert!(Arc::ptr_eq(&first, &cached));
        Ok(())
    }
}
