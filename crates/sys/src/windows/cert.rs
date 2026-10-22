//! The current user's certificate stores, for `trust-windows`: add a certificate,
//! find or delete one by SHA-1 thumbprint, list those whose subject contains a
//! string.
//!
//! A CurrentUser system store opened through `CERT_STORE_PROV_SYSTEM_W` is a
//! logical collection of physical stores. For `Root` these are `.Default` (the
//! user's registry store), `.LocalMachine` and `.SmartCard` ("System Store
//! Locations" in the Win32 docs), so [`View::Logical`] also lists the roots an
//! administrator trusted for every user. That is the right answer to "does this
//! user trust the CA", and the logical view is read-only here. Every change goes
//! through [`View::UserRegistry`] (`CERT_STORE_PROV_SYSTEM_REGISTRY_W`, only the
//! user's `HKEY_CURRENT_USER` store, no collection): a write through the
//! collection lands in whichever physical store accepts it, which could be the
//! machine's `Root` in an elevated process.
//!
//! Adding to or deleting from the user's `Root` makes crypt32 show a security
//! warning that the user must confirm; no flag suppresses it. Other stores never
//! prompt. The tests therefore use [`TestStore`] scratch stores and run in the
//! normal test suite: they create `devlanes-test-*` keys under
//! `HKCU\Software\Microsoft\SystemCertificates`, need no elevation, show no dialog,
//! and the guard deletes the store when the test ends or panics. Only the ignored
//! test in `devlanes-trust-windows` touches the real `Root`.

use std::ffi::OsStr;
use std::io;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{
    CRYPT_E_NOT_FOUND, ERROR_CANCELLED, ERROR_REQUIRES_INTERACTIVE_WINDOWSTATION, GetLastError,
};
use windows_sys::Win32::Security::Cryptography::{
    CERT_CONTEXT, CERT_FIND_ANY, CERT_FIND_FLAGS, CERT_FIND_SHA1_HASH, CERT_FIND_SUBJECT_STR_W,
    CERT_OPEN_STORE_FLAGS, CERT_SHA1_HASH_PROP_ID, CERT_STORE_ADD_REPLACE_EXISTING,
    CERT_STORE_DELETE_FLAG, CERT_STORE_MAXIMUM_ALLOWED_FLAG, CERT_STORE_OPEN_EXISTING_FLAG,
    CERT_STORE_PROV_SYSTEM_REGISTRY_W, CERT_STORE_PROV_SYSTEM_W, CERT_STORE_READONLY_FLAG,
    CERT_SYSTEM_STORE_CURRENT_USER, CRYPT_INTEGER_BLOB, CertAddEncodedCertificateToStore,
    CertCloseStore, CertDeleteCertificateFromStore, CertFindCertificateInStore,
    CertFreeCertificateContext, CertGetCertificateContextProperty, CertOpenStore,
    CertUnregisterSystemStore, HCERTSTORE, PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
};
use windows_sys::Win32::UI::WindowsAndMessaging::WSF_VISIBLE;

use super::security::wide;

/// SHA-1 hash of a certificate's DER encoding, the "thumbprint" Windows shows.
pub type Thumbprint = [u8; 20];

/// Encodings crypt32 should accept and match; the usual pair for X.509 certificates.
const ENCODING: u32 = X509_ASN_ENCODING | PKCS_7_ASN_ENCODING;

/// Prefix [`unregister_test_store`] insists on, so a test can never delete a real store.
pub const TEST_STORE_PREFIX: &str = "devlanes-test-";

/// Failure to change a store.
#[derive(Debug, thiserror::Error)]
pub enum CertError {
    /// The user answered No in Windows' confirmation dialog.
    #[error("the certificate change was declined in the Windows security warning")]
    Cancelled,
    /// The process has no visible window station (SSH, a service, a container), so
    /// Windows could not show the confirmation dialog.
    #[error("Windows needs to show a security warning, but this session has no desktop")]
    NoDesktop,
    /// Any other failure.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Which part of a CurrentUser system store to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// The logical store: every physical store of the system store, including the
    /// machine-wide ones. Read-only.
    Logical,
    /// Only the user's registry store (`.Default`), the one view that can be changed.
    UserRegistry,
}

/// How to open a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read-only; a store that does not exist is `ErrorKind::NotFound`.
    Read,
    /// Read-write where allowed (`CERT_STORE_MAXIMUM_ALLOWED_FLAG`); the store is
    /// created when it does not exist.
    Write,
}

/// An open certificate store, closed on drop.
#[derive(Debug)]
pub struct CertStore {
    handle: HCERTSTORE,
}

impl CertStore {
    /// Opens the CurrentUser system store `name` (`Root`, or a scratch name in tests).
    /// [`View::Logical`] with [`Access::Write`] is `ErrorKind::InvalidInput`, so no
    /// write can reach a machine-wide physical store.
    pub fn open(name: &str, view: View, access: Access) -> io::Result<Self> {
        let provider = match (view, access) {
            (View::Logical, Access::Read) => CERT_STORE_PROV_SYSTEM_W,
            (View::Logical, Access::Write) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the logical certificate store view is read-only",
                ));
            }
            (View::UserRegistry, _) => CERT_STORE_PROV_SYSTEM_REGISTRY_W,
        };
        let access_flags: CERT_OPEN_STORE_FLAGS = match access {
            Access::Read => CERT_STORE_READONLY_FLAG | CERT_STORE_OPEN_EXISTING_FLAG,
            // A store the user may only read opens read-only instead of failing;
            // the change then fails with access denied.
            Access::Write => CERT_STORE_MAXIMUM_ALLOWED_FLAG,
        };
        let name = wide(OsStr::new(name));
        // SAFETY: `name` is NUL-terminated and outlives the call; system store
        // providers take no crypto provider handle.
        let handle = unsafe {
            CertOpenStore(
                provider,
                0,
                0,
                CERT_SYSTEM_STORE_CURRENT_USER | access_flags,
                name.as_ptr().cast(),
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    /// Adds a DER-encoded certificate, replacing an identical one. Adding to the
    /// user's `Root` shows Windows' security warning and waits for the answer.
    pub fn add_der(&self, der: &[u8]) -> Result<(), CertError> {
        let len = u32::try_from(der.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "certificate too large"))?;
        // SAFETY: the store handle is open; `der` holds `len` readable bytes; a null
        // output pointer means no context is returned.
        let ok = unsafe {
            CertAddEncodedCertificateToStore(
                self.handle,
                ENCODING,
                der.as_ptr(),
                len,
                CERT_STORE_ADD_REPLACE_EXISTING,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(change_error());
        }
        Ok(())
    }

    /// Whether a certificate with `thumbprint` is in the store.
    pub fn contains(&self, thumbprint: &Thumbprint) -> io::Result<bool> {
        let found = self.find_sha1(thumbprint)?;
        let present = !found.is_null();
        if present {
            // SAFETY: `found` came from CertFindCertificateInStore and is freed once.
            unsafe { CertFreeCertificateContext(found) };
        }
        Ok(present)
    }

    /// Deletes the certificate with `thumbprint`; `Ok(false)` when it is absent.
    /// Deleting from the user's `Root` shows Windows' security warning.
    pub fn delete(&self, thumbprint: &Thumbprint) -> Result<bool, CertError> {
        let found = self.find_sha1(thumbprint)?;
        if found.is_null() {
            return Ok(false);
        }
        // SAFETY: `found` is a context of this store; the call frees it even when
        // it fails, so it is not used afterwards.
        let ok = unsafe { CertDeleteCertificateFromStore(found) };
        if ok == 0 {
            return Err(change_error());
        }
        Ok(true)
    }

    /// Thumbprints of the certificates whose subject contains `needle`, compared
    /// case-insensitively against every part of the subject name
    /// (`CERT_FIND_SUBJECT_STR_W`).
    pub fn thumbprints_with_subject(&self, needle: &str) -> io::Result<Vec<Thumbprint>> {
        let needle = wide(OsStr::new(needle));
        self.thumbprints(CERT_FIND_SUBJECT_STR_W, needle.as_ptr().cast())
    }

    /// Thumbprints of every certificate in the store.
    pub fn all_thumbprints(&self) -> io::Result<Vec<Thumbprint>> {
        self.thumbprints(CERT_FIND_ANY, ptr::null())
    }

    /// Thumbprints of every certificate matching a `CertFindCertificateInStore` query.
    fn thumbprints(
        &self,
        find: CERT_FIND_FLAGS,
        para: *const core::ffi::c_void,
    ) -> io::Result<Vec<Thumbprint>> {
        let mut found = Vec::new();
        let mut previous: *const CERT_CONTEXT = ptr::null();
        loop {
            // SAFETY: the store is open; `para` matches `find` and outlives the
            // loop; `previous` is null or the last context this loop got, which the
            // call frees.
            let next = unsafe {
                CertFindCertificateInStore(self.handle, ENCODING, 0, find, para, previous)
            };
            if next.is_null() {
                return end_of_search().map(|()| found);
            }
            previous = next;
            match sha1_property(next) {
                Ok(thumbprint) => found.push(thumbprint),
                Err(error) => {
                    // SAFETY: `next` is the live context from the call above.
                    unsafe { CertFreeCertificateContext(next) };
                    return Err(error);
                }
            }
        }
    }

    /// The context with `thumbprint`, or null when absent; the caller frees it.
    fn find_sha1(&self, thumbprint: &Thumbprint) -> io::Result<*const CERT_CONTEXT> {
        let blob = CRYPT_INTEGER_BLOB {
            cbData: thumbprint.len() as u32,
            // crypt32 only reads the hash through this pointer.
            pbData: thumbprint.as_ptr().cast_mut(),
        };
        // SAFETY: the store is open; `blob` and the bytes it points to outlive the
        // call; this is a first search, so no previous context is passed.
        let found = unsafe {
            CertFindCertificateInStore(
                self.handle,
                ENCODING,
                0,
                CERT_FIND_SHA1_HASH,
                (&raw const blob).cast(),
                ptr::null(),
            )
        };
        if found.is_null() {
            end_of_search()?;
        }
        Ok(found)
    }
}

impl Drop for CertStore {
    /// Closes the handle; contexts are never kept past the method that found them.
    fn drop(&mut self) {
        // SAFETY: the handle was opened by `open` and is closed once.
        unsafe { CertCloseStore(self.handle, 0) };
    }
}

/// `Ok` when a null search result means "no (more) matches".
fn end_of_search() -> io::Result<()> {
    // SAFETY: reads the calling thread's last-error value.
    let code = unsafe { GetLastError() };
    if code as i32 == CRYPT_E_NOT_FOUND {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

/// The cached SHA-1 hash property of a certificate context.
fn sha1_property(context: *const CERT_CONTEXT) -> io::Result<Thumbprint> {
    let mut thumbprint: Thumbprint = [0; 20];
    let mut len = thumbprint.len() as u32;
    // SAFETY: `context` is live; the buffer holds `len` writable bytes.
    let ok = unsafe {
        CertGetCertificateContextProperty(
            context,
            CERT_SHA1_HASH_PROP_ID,
            thumbprint.as_mut_ptr().cast(),
            &raw mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != thumbprint.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SHA-1 property has an unexpected length",
        ));
    }
    Ok(thumbprint)
}

/// The Win32 code `code` as an `HRESULT` (`HRESULT_FROM_WIN32`), the form crypt32
/// often sets as last error.
const fn hresult_from_win32(code: u32) -> i32 {
    ((code & 0xFFFF) | 0x8007_0000) as i32
}

/// Classifies the last error of a failed add or delete.
///
/// crypt32 reports both a No in the dialog and a dialog it could not show as
/// `ERROR_CANCELLED` (`0x800704C7`; the latter is seen in Windows containers,
/// microsoft/Windows-Containers#506), so the window station tells them apart.
fn change_error() -> CertError {
    let error = io::Error::last_os_error();
    let cancelled = [ERROR_CANCELLED as i32, hresult_from_win32(ERROR_CANCELLED)];
    let no_desktop = [
        ERROR_REQUIRES_INTERACTIVE_WINDOWSTATION as i32,
        hresult_from_win32(ERROR_REQUIRES_INTERACTIVE_WINDOWSTATION),
    ];
    match error.raw_os_error() {
        Some(code) if no_desktop.contains(&code) => CertError::NoDesktop,
        // An unreadable window station is treated as interactive: a real No
        // should not be reported as a missing desktop.
        Some(code) if cancelled.contains(&code) => match interactive_desktop() {
            Ok(false) => CertError::NoDesktop,
            Ok(true) | Err(_) => CertError::Cancelled,
        },
        _ => CertError::Io(error),
    }
}

/// Whether this process's window station has a visible display (`WSF_VISIBLE`),
/// so a dialog it shows can reach the user. False for services, SSH sessions and
/// containers, which run in non-interactive window stations.
pub fn interactive_desktop() -> io::Result<bool> {
    // SAFETY: no arguments; the returned handle is borrowed and must not be closed.
    let station = unsafe { GetProcessWindowStation() };
    if station.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut flags = USEROBJECTFLAGS {
        fInherit: 0,
        fReserved: 0,
        dwFlags: 0,
    };
    let mut needed = 0u32;
    // SAFETY: `station` is a valid window station handle; `flags` is writable and
    // as large as the size passed.
    let ok = unsafe {
        GetUserObjectInformationW(
            station,
            UOI_FLAGS,
            (&raw mut flags).cast(),
            size_of::<USEROBJECTFLAGS>() as u32,
            &raw mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags.dwFlags & WSF_VISIBLE as u32 != 0)
}

/// Unregisters and deletes the CurrentUser system store `name`, which must start
/// with [`TEST_STORE_PREFIX`]: only test stores are ever removed.
pub fn unregister_test_store(name: &str) -> io::Result<()> {
    if !name.starts_with(TEST_STORE_PREFIX) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to delete certificate store `{name}`"),
        ));
    }
    let wide_name = wide(OsStr::new(name));
    // SAFETY: `wide_name` is NUL-terminated and outlives the call.
    let ok = unsafe {
        CertUnregisterSystemStore(
            wide_name.as_ptr().cast(),
            CERT_SYSTEM_STORE_CURRENT_USER | CERT_STORE_DELETE_FLAG,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A uniquely named scratch CurrentUser store for tests, created by the first
/// write and deleted on drop, which also runs when the test panics. It never
/// prompts: only `Root` does.
#[derive(Debug)]
pub struct TestStore {
    name: String,
}

impl TestStore {
    /// A fresh name under [`TEST_STORE_PREFIX`], unique per process and call.
    pub fn unique() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        Self {
            name: format!(
                "{TEST_STORE_PREFIX}{}-{}-{nanos}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ),
        }
    }

    /// The store name to pass to [`CertStore::open`].
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for TestStore {
    /// Deletes the store; one that was never written does not exist.
    fn drop(&mut self) {
        match unregister_test_store(&self.name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => eprintln!("leaked certificate store {}: {error}", self.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether the scratch store's registry key exists.
    fn exists(name: &str) -> bool {
        CertStore::open(name, View::UserRegistry, Access::Read).is_ok()
    }

    /// A self-signed CA certificate with common name `name`, DER-encoded.
    fn certificate(name: &str) -> Vec<u8> {
        let key = rcgen::KeyPair::generate().expect("key");
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        params
            .self_signed(&key)
            .expect("certificate")
            .der()
            .to_vec()
    }

    /// Add, find by subject and thumbprint, delete, and delete again in a scratch store.
    #[test]
    fn scratch_store_round_trip() {
        let scratch = TestStore::unique();
        let store =
            CertStore::open(scratch.name(), View::UserRegistry, Access::Write).expect("open");
        store
            .add_der(&certificate("devlanes local development CA"))
            .expect("add");
        store
            .add_der(&certificate("someone else's CA"))
            .expect("add other");

        let logical = CertStore::open(scratch.name(), View::Logical, Access::Read).expect("read");
        let ours = logical
            .thumbprints_with_subject("DEVLANES local development")
            .expect("by subject");
        assert_eq!(ours.len(), 1, "case-insensitive substring of the subject");
        assert_eq!(logical.all_thumbprints().expect("all").len(), 2);
        assert!(logical.contains(&ours[0]).expect("contains"));

        assert!(store.delete(&ours[0]).expect("delete"));
        assert!(!store.delete(&ours[0]).expect("delete absent"));
        let reopened =
            CertStore::open(scratch.name(), View::Logical, Access::Read).expect("reopen");
        assert!(!reopened.contains(&ours[0]).expect("gone"));
        assert_eq!(reopened.all_thumbprints().expect("all").len(), 1);
    }

    /// The logical view never opens for writing, so nothing can reach a machine store.
    #[test]
    fn logical_view_is_read_only() {
        let scratch = TestStore::unique();
        let error =
            CertStore::open(scratch.name(), View::Logical, Access::Write).expect_err("refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
        assert!(!exists(scratch.name()), "a refused open creates nothing");
    }

    /// Garbage is an I/O error, not a cancelled dialog.
    #[test]
    fn invalid_der_is_an_io_error() {
        let scratch = TestStore::unique();
        let store =
            CertStore::open(scratch.name(), View::UserRegistry, Access::Write).expect("open");
        assert!(matches!(
            store.add_der(b"not a certificate"),
            Err(CertError::Io(_))
        ));
    }

    /// A read-only open never creates a store.
    #[test]
    fn read_only_open_of_a_missing_store_is_not_found() {
        let scratch = TestStore::unique();
        for view in [View::Logical, View::UserRegistry] {
            let error = CertStore::open(scratch.name(), view, Access::Read)
                .expect_err("store does not exist");
            assert_eq!(error.kind(), io::ErrorKind::NotFound, "{view:?}: {error}");
        }
        assert!(!exists(scratch.name()));
    }

    /// The guard deletes the store when a test panics, and a never-written store
    /// is no error.
    #[test]
    fn test_store_is_deleted_after_a_panic() {
        let name = std::panic::catch_unwind(|| {
            let scratch = TestStore::unique();
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
        let never = TestStore::unique();
        let error = unregister_test_store(never.name()).expect_err("never written");
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
    }

    /// The logical CurrentUser `Root` includes the machine's roots, the user's
    /// registry store does not. Read-only: the real `Root` is never changed.
    #[test]
    fn user_root_aggregates_machine_roots() {
        let logical = CertStore::open("Root", View::Logical, Access::Read)
            .expect("logical Root")
            .all_thumbprints()
            .expect("list logical");
        let registry = match CertStore::open("Root", View::UserRegistry, Access::Read) {
            Ok(store) => store.all_thumbprints().expect("list registry"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("user registry Root: {error}"),
        };
        assert!(
            registry
                .iter()
                .all(|thumbprint| logical.contains(thumbprint)),
            "the user's own roots are part of the logical store"
        );
        assert!(
            logical.len() > registry.len(),
            "machine roots show up in the logical store ({} vs {})",
            logical.len(),
            registry.len()
        );
    }

    /// Only test stores can be deleted.
    #[test]
    fn refuses_to_unregister_real_stores() {
        for name in ["Root", "My", "CA", "root"] {
            let error = unregister_test_store(name).expect_err("refused");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn window_station_flags_are_readable() {
        interactive_desktop().expect("window station flags");
    }

    #[test]
    fn hresult_wraps_win32_codes() {
        assert_eq!(
            hresult_from_win32(ERROR_CANCELLED) as u32,
            0x8007_04C7,
            "the code certutil prints for a declined root"
        );
    }
}
