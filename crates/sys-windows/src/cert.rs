//! The current user's certificate stores: add a certificate, find or delete one
//! by SHA-1 thumbprint, find those whose subject contains a string, and list a
//! store's certificates with the facts a TLS client filters roots by.
//!
//! A CurrentUser system store opened through `CERT_STORE_PROV_SYSTEM_W` is a
//! logical collection of physical stores. For `Root` these are the user's
//! registry store, the machine's and the smart card's ("System Store
//! Locations" in the Win32 docs), so [`View::Logical`] also lists the roots an
//! administrator trusted for every user. It is only ever opened read-only.
//! Every change goes through [`View::UserRegistry`]
//! (`CERT_STORE_PROV_SYSTEM_REGISTRY_W`, the user's `HKEY_CURRENT_USER` store
//! alone): a write through the collection lands in whichever physical store
//! accepts it, which could be the machine's `Root` in an elevated process.
//!
//! Adding to or deleting from the user's `Root` makes crypt32 show a security
//! warning the user must confirm; no flag suppresses it. Other stores never
//! prompt, so tests use scratch stores (the `test-support` feature).

use std::ffi::{CStr, OsStr};
use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::{
    CRYPT_E_NOT_FOUND, ERROR_CANCELLED, ERROR_REQUIRES_INTERACTIVE_WINDOWSTATION, GetLastError,
};
use windows_sys::Win32::Security::Cryptography::{
    CERT_CONTEXT, CERT_FIND_FLAGS, CERT_FIND_SHA1_HASH, CERT_FIND_SUBJECT_STR_W,
    CERT_OPEN_STORE_FLAGS, CERT_SHA1_HASH_PROP_ID, CERT_STORE_ADD_REPLACE_EXISTING,
    CERT_STORE_MAXIMUM_ALLOWED_FLAG, CERT_STORE_OPEN_EXISTING_FLAG,
    CERT_STORE_PROV_SYSTEM_REGISTRY_W, CERT_STORE_PROV_SYSTEM_W, CERT_STORE_READONLY_FLAG,
    CERT_SYSTEM_STORE_CURRENT_USER, CRYPT_INTEGER_BLOB, CTL_USAGE,
    CertAddEncodedCertificateToStore, CertCloseStore, CertDeleteCertificateFromStore,
    CertEnumCertificatesInStore, CertFindCertificateInStore, CertFreeCertificateContext,
    CertGetCertificateContextProperty, CertGetEnhancedKeyUsage, CertOpenStore,
    CertVerifyTimeValidity, HCERTSTORE, PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
};
use windows_sys::Win32::UI::WindowsAndMessaging::WSF_VISIBLE;

use crate::util::{hresult_error, wide, win32_error};

/// SHA-1 hash of a certificate's DER encoding, the "thumbprint" Windows shows.
pub type Thumbprint = [u8; 20];

/// Encodings crypt32 should accept and match; the usual pair for X.509.
const ENCODING: u32 = X509_ASN_ENCODING | PKCS_7_ASN_ENCODING;

/// The server authentication extended key usage.
const SERVER_AUTH: &CStr = c"1.3.6.1.5.5.7.3.1";

/// Failure to change a store.
#[derive(Debug, thiserror::Error)]
pub enum CertError {
    /// The user answered No in the Windows confirmation dialog.
    #[error("the certificate change was declined in the Windows security warning")]
    Cancelled,
    /// The session has no visible window station (SSH, a service), so Windows
    /// could not show the confirmation dialog.
    #[error("Windows needs to show a security warning, but this session has no desktop")]
    NoDesktop,
    /// Any other failure.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Which part of a CurrentUser system store to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Every physical store of the system store, the machine's included.
    /// Read-only.
    Logical,
    /// Only the user's registry store, the one view that can be changed.
    UserRegistry,
}

/// How to open a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read-only; a store that does not exist is `ErrorKind::NotFound`.
    Read,
    /// Read-write where allowed; the store is created when it does not exist.
    Write,
}

/// One certificate of a store, with what a TLS client checks before using it
/// as a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreCertificate {
    /// The DER encoding.
    pub der: Vec<u8>,
    /// Its SHA-1 thumbprint.
    pub thumbprint: Thumbprint,
    /// Usable for server authentication: no usage limits, or limits that
    /// include it (the certificate's extension and the store's properties
    /// together, as `CertGetEnhancedKeyUsage` reports them).
    pub server_auth: bool,
    /// Valid now.
    pub time_valid: bool,
}

/// What [`CertStore::certificates`] found.
#[derive(Debug, Default)]
pub struct Listing {
    /// The certificates whose facts were read.
    pub certificates: Vec<StoreCertificate>,
    /// The certificates whose facts could not be read.
    pub unreadable: Vec<Unreadable>,
}

/// A certificate of a store whose thumbprint or usage could not be read.
#[derive(Debug)]
pub struct Unreadable {
    /// The DER encoding, which a store always has.
    pub der: Vec<u8>,
    /// Why its facts could not be read.
    pub error: io::Error,
}

/// An open certificate store, closed on drop.
#[derive(Debug)]
pub struct CertStore {
    handle: HCERTSTORE,
}

impl CertStore {
    /// Opens the CurrentUser system store `name` (`Root`, `Disallowed`, or a
    /// scratch store). [`View::Logical`] with [`Access::Write`] is refused
    /// with `ErrorKind::InvalidInput`, so no write reaches a machine store.
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
        let flags: CERT_OPEN_STORE_FLAGS = match access {
            Access::Read => CERT_STORE_READONLY_FLAG | CERT_STORE_OPEN_EXISTING_FLAG,
            // A store the user may only read opens read-only instead of
            // failing; the change then fails with access denied.
            Access::Write => CERT_STORE_MAXIMUM_ALLOWED_FLAG,
        };
        let name = wide(OsStr::new(name))?;
        // SAFETY: `name` is NUL-terminated and outlives the call; system store
        // providers take no crypto provider handle.
        let handle = unsafe {
            CertOpenStore(
                provider,
                0,
                0,
                CERT_SYSTEM_STORE_CURRENT_USER | flags,
                name.as_ptr().cast(),
            )
        };
        if handle.is_null() {
            return Err(last_error());
        }
        Ok(Self { handle })
    }

    /// Adds a DER-encoded certificate, replacing an identical one. Adding to
    /// the user's `Root` shows the Windows security warning and waits for the
    /// answer; callers check [`interactive_desktop`] first.
    pub fn add_der(&self, der: &[u8]) -> Result<(), CertError> {
        let len = u32::try_from(der.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "certificate too large"))?;
        // SAFETY: the store is open; `der` holds `len` readable bytes; a null
        // output pointer asks for no context back.
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
        if found.is_null() {
            return Ok(false);
        }
        // SAFETY: `found` came from CertFindCertificateInStore and is freed once.
        unsafe { CertFreeCertificateContext(found) };
        Ok(true)
    }

    /// Deletes the certificate with `thumbprint`; `Ok(false)` when it is
    /// absent. Deleting from the user's `Root` shows the Windows security
    /// warning.
    pub fn delete(&self, thumbprint: &Thumbprint) -> Result<bool, CertError> {
        let found = self.find_sha1(thumbprint)?;
        if found.is_null() {
            return Ok(false);
        }
        // SAFETY: `found` is a context of this store; the call frees it even
        // when it fails, so it is not used afterwards.
        if unsafe { CertDeleteCertificateFromStore(found) } == 0 {
            return Err(change_error());
        }
        Ok(true)
    }

    /// Thumbprints of the certificates whose subject contains `needle`,
    /// compared case-insensitively with every part of the subject name.
    pub fn thumbprints_with_subject(&self, needle: &str) -> io::Result<Vec<Thumbprint>> {
        let needle = wide(OsStr::new(needle))?;
        let mut found = Vec::new();
        self.each(CERT_FIND_SUBJECT_STR_W, needle.as_ptr().cast(), |context| {
            found.push(sha1_property(context)?);
            Ok(())
        })?;
        Ok(found)
    }

    /// Every certificate in the store. One whose facts cannot be read (a
    /// malformed usage extension, say) is listed in
    /// [`Listing::unreadable`] instead of failing the whole listing; only a
    /// failed enumeration is an error.
    pub fn certificates(&self) -> io::Result<Listing> {
        let mut listing = Listing::default();
        let mut previous: *const CERT_CONTEXT = ptr::null();
        loop {
            // SAFETY: the store is open; `previous` is null or the context the
            // last call returned, which this call frees.
            let next = unsafe { CertEnumCertificatesInStore(self.handle, previous) };
            if next.is_null() {
                return end_of_search().map(|()| listing);
            }
            previous = next;
            match describe(next) {
                Ok(certificate) => listing.certificates.push(certificate),
                Err(error) => listing.unreadable.push(Unreadable {
                    der: encoded(next),
                    error,
                }),
            }
        }
    }

    /// Calls `visit` with every context matching a `CertFindCertificateInStore`
    /// query; the context is only valid during the call.
    fn each(
        &self,
        find: CERT_FIND_FLAGS,
        para: *const core::ffi::c_void,
        mut visit: impl FnMut(*const CERT_CONTEXT) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut previous: *const CERT_CONTEXT = ptr::null();
        loop {
            // SAFETY: the store is open; `para` matches `find` and outlives
            // the loop; `previous` is null or the last context, which the call
            // frees.
            let next = unsafe {
                CertFindCertificateInStore(self.handle, ENCODING, 0, find, para, previous)
            };
            if next.is_null() {
                return end_of_search();
            }
            previous = next;
            if let Err(error) = visit(next) {
                // SAFETY: `next` is live and was not handed to another call.
                unsafe { CertFreeCertificateContext(next) };
                return Err(error);
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
        // SAFETY: the store is open; `blob` and its bytes outlive the call; a
        // first search passes no previous context.
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
    /// Closes the handle; contexts never outlive the method that found them.
    fn drop(&mut self) {
        // SAFETY: the handle was opened by `open` and is closed once.
        unsafe { CertCloseStore(self.handle, 0) };
    }
}

/// Copies `context`'s encoding and the facts [`StoreCertificate`] holds.
fn describe(context: *const CERT_CONTEXT) -> io::Result<StoreCertificate> {
    // SAFETY: `context` is a live context, so `pCertInfo` points at its
    // parsed fields; a null time means now.
    let time_valid = unsafe { CertVerifyTimeValidity(ptr::null(), (*context).pCertInfo) } == 0;
    Ok(StoreCertificate {
        der: encoded(context),
        thumbprint: sha1_property(context)?,
        server_auth: server_auth(context)?,
        time_valid,
    })
}

/// A copy of `context`'s DER encoding.
fn encoded(context: *const CERT_CONTEXT) -> Vec<u8> {
    // SAFETY: `context` is a live context, so its encoded bytes are readable
    // for `cbCertEncoded` bytes.
    unsafe {
        let context = &*context;
        std::slice::from_raw_parts(context.pbCertEncoded, context.cbCertEncoded as usize).to_vec()
    }
}

/// Whether `context` may be used for server authentication, read the way
/// rustls-native-certs reads it through schannel.
fn server_auth(context: *const CERT_CONTEXT) -> io::Result<bool> {
    let mut size = 0_u32;
    // SAFETY: `context` is live; a null buffer asks for the size only.
    if unsafe { CertGetEnhancedKeyUsage(context, 0, ptr::null_mut(), &raw mut size) } == 0 {
        return Err(last_error());
    }
    // 8-aligned storage for the CTL_USAGE header and the strings after it.
    let mut buffer = vec![0_u64; (size as usize).div_ceil(8).max(1)];
    let usage = buffer.as_mut_ptr().cast::<CTL_USAGE>();
    // SAFETY: `buffer` holds at least `size` writable, aligned bytes.
    if unsafe { CertGetEnhancedKeyUsage(context, 0, usage, &raw mut size) } == 0 {
        return Err(last_error());
    }
    // SAFETY: the call filled the header at the start of the buffer.
    let usage = unsafe { &*usage };
    if usage.cUsageIdentifier == 0 {
        // No identifiers with "not found" means no limits; otherwise the
        // extension and the properties leave no usage at all.
        // SAFETY: reads the calling thread's last error.
        return Ok(unsafe { GetLastError() } == CRYPT_E_NOT_FOUND.cast_unsigned());
    }
    // SAFETY: the call wrote `cUsageIdentifier` pointers to NUL-terminated
    // strings, all inside `buffer`, which lives until the end of this function.
    let identifiers = unsafe {
        std::slice::from_raw_parts(usage.rgpszUsageIdentifier, usage.cUsageIdentifier as usize)
    };
    Ok(identifiers.iter().any(|&identifier| {
        // SAFETY: as above.
        !identifier.is_null() && unsafe { CStr::from_ptr(identifier.cast()) } == SERVER_AUTH
    }))
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
        return Err(last_error());
    }
    if len as usize != thumbprint.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SHA-1 property has an unexpected length",
        ));
    }
    Ok(thumbprint)
}

/// `Ok` when a null search result means "no (more) matches".
fn end_of_search() -> io::Result<()> {
    // SAFETY: reads the calling thread's last error.
    let code = unsafe { GetLastError() };
    if code == CRYPT_E_NOT_FOUND.cast_unsigned() {
        Ok(())
    } else {
        Err(error_from(code))
    }
}

/// The last error of a crypt32 call, which may be a Win32 code or an HRESULT.
fn last_error() -> io::Error {
    // SAFETY: reads the calling thread's last error.
    error_from(unsafe { GetLastError() })
}

/// `code` as an error: crypt32 sets HRESULTs (high bit set) as well as Win32
/// codes. An HRESULT that wraps a Win32 code (`0x8007xxxx`) becomes that code,
/// so a missing store still reads as `NotFound`.
fn error_from(code: u32) -> io::Error {
    if code & 0x8000_0000 != 0 {
        hresult_error(code.cast_signed())
    } else {
        win32_error(code)
    }
}

/// The Win32 `code` as the HRESULT crypt32 often sets instead (`HRESULT_FROM_WIN32`).
const fn hresult_from_win32(code: u32) -> u32 {
    (code & 0xFFFF) | 0x8007_0000
}

/// Classifies the last error of a failed add or delete. crypt32 also reports
/// a dialog it could not show as `ERROR_CANCELLED`; callers check
/// [`interactive_desktop`] before a change, so here it means No.
fn change_error() -> CertError {
    // SAFETY: reads the calling thread's last error.
    let code = unsafe { GetLastError() };
    let is = |win32: u32| code == win32 || code == hresult_from_win32(win32);
    if is(ERROR_REQUIRES_INTERACTIVE_WINDOWSTATION) {
        CertError::NoDesktop
    } else if is(ERROR_CANCELLED) {
        CertError::Cancelled
    } else {
        CertError::Io(error_from(code))
    }
}

/// Whether this process's window station is visible (`WSF_VISIBLE`), so a
/// dialog it shows reaches the user. False for services and SSH sessions,
/// which run in non-interactive window stations.
pub fn interactive_desktop() -> io::Result<bool> {
    // SAFETY: no arguments; the handle is borrowed and must not be closed.
    let station = unsafe { GetProcessWindowStation() };
    if station.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut flags = USEROBJECTFLAGS {
        fInherit: 0,
        fReserved: 0,
        dwFlags: 0,
    };
    let mut needed = 0_u32;
    // SAFETY: `station` is a valid window station handle; `flags` is writable
    // and as large as the size passed.
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
    Ok(flags.dwFlags & WSF_VISIBLE.cast_unsigned() != 0)
}

#[cfg(feature = "test-support")]
pub use scratch::ScratchStore;

/// Throwaway stores for tests of this crate and its callers.
#[cfg(feature = "test-support")]
mod scratch {
    use std::ffi::OsStr;
    use std::io;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use windows_sys::Win32::Security::Cryptography::{
        CERT_STORE_DELETE_FLAG, CERT_SYSTEM_STORE_CURRENT_USER, CertUnregisterSystemStore,
    };

    use crate::util::wide;

    /// Prefix every scratch store has, so a guard can never delete a real store.
    const PREFIX: &str = "devlanes-test-";

    /// A uniquely named CurrentUser registry store under
    /// `HKCU\Software\Microsoft\SystemCertificates`, created by the first
    /// write and deleted on drop, which also runs when a test panics. It never
    /// prompts: only `Root` does.
    #[derive(Debug)]
    pub struct ScratchStore {
        name: String,
    }

    impl ScratchStore {
        /// A fresh name, unique per process and call.
        pub fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos());
            Self {
                name: format!(
                    "{PREFIX}{}-{}-{nanos}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ),
            }
        }

        /// The store name to pass to `CertStore::open`.
        pub fn name(&self) -> &str {
            &self.name
        }

        /// Deletes the store; `NotFound` when it was never written.
        pub fn delete(&self) -> io::Result<()> {
            if !self.name.starts_with(PREFIX) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "not a scratch store",
                ));
            }
            let name = wide(OsStr::new(&self.name))?;
            // SAFETY: `name` is NUL-terminated and outlives the call.
            let ok = unsafe {
                CertUnregisterSystemStore(
                    name.as_ptr().cast(),
                    CERT_SYSTEM_STORE_CURRENT_USER | CERT_STORE_DELETE_FLAG,
                )
            };
            if ok == 0 {
                return Err(super::last_error());
            }
            Ok(())
        }
    }

    impl Default for ScratchStore {
        /// Same as [`ScratchStore::new`].
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for ScratchStore {
        /// Deletes the store; one that was never written does not exist.
        fn drop(&mut self) {
            match self.delete() {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => eprintln!("leaked certificate store {}: {error}", self.name),
            }
        }
    }
}
