//! Per-OS path encodings for BLOB columns and SQLite file URIs. A database
//! never moves between operating systems, so each OS keeps its native form.

#[cfg(unix)]
pub(crate) use unix::{database_url, path_bytes, path_from_bytes};
#[cfg(windows)]
pub(crate) use windows::{database_url, path_bytes, path_from_bytes};

#[cfg(unix)]
mod unix {
    use crate::Error;
    use std::{
        ffi::OsString,
        os::unix::ffi::{OsStrExt, OsStringExt},
        path::{Path, PathBuf},
    };

    /// Preserves Unix path bytes in BLOB columns.
    pub(crate) fn path_bytes(path: &Path) -> Vec<u8> {
        path.as_os_str().as_bytes().to_vec()
    }

    /// Restores a path supplied by Git or the filesystem.
    pub(crate) fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
        OsString::from_vec(bytes).into()
    }

    /// Encodes an absolute path as a SQLite file URI, keeping non-UTF-8 names
    /// and preventing filename punctuation from becoming URI options.
    pub(crate) fn database_url(path: &Path) -> Result<String, Error> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.contains(&0) {
            return Err(Error::Invalid("database path contains NUL".into()));
        }
        Ok(format!(
            "file:{}",
            percent_encoding::percent_encode(bytes, percent_encoding::NON_ALPHANUMERIC)
        ))
    }
}

#[cfg(windows)]
mod windows {
    use crate::Error;
    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
    use std::{
        ffi::{OsStr, OsString},
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::{Component, Path, PathBuf, Prefix},
    };

    /// RFC 3986 unreserved characters stay literal inside a URI path segment.
    const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');

    /// Stores the path's UTF-16 code units as little-endian bytes, which keeps
    /// unpaired surrogates that a UTF-8 conversion would lose.
    pub(crate) fn path_bytes(path: &Path) -> Vec<u8> {
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    /// Restores a path written by [`path_bytes`]. A trailing odd byte can only
    /// come from a corrupt row; it becomes U+FFFD instead of failing the read.
    pub(crate) fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
        let (pairs, rest) = bytes.as_chunks::<2>();
        let mut wide: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        if !rest.is_empty() {
            wide.push(0xFFFD);
        }
        OsString::from_wide(&wide).into()
    }

    /// Builds `file:///C:/dir/state.db` (or `file:////server/share/...` for
    /// UNC paths) from an absolute path. Each segment is percent-encoded as
    /// UTF-8 so `#`, `?` and `%` stay part of the name. SQLite converts URI
    /// filenames from UTF-8, so paths that are not valid Unicode are refused.
    pub(crate) fn database_url(path: &Path) -> Result<String, Error> {
        let mut components = path.components();
        let mut url = match components.next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                    format!("file:///{}:", char::from(letter))
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    format!("file:////{}/{}", segment(server)?, segment(share)?)
                }
                _ => {
                    return Err(Error::Invalid(format!(
                        "unsupported database path prefix: {}",
                        path.display()
                    )));
                }
            },
            _ => {
                return Err(Error::Invalid(format!(
                    "database path has no drive or share: {}",
                    path.display()
                )));
            }
        };
        for component in components {
            if component == Component::RootDir {
                continue;
            }
            url.push('/');
            url.push_str(&segment(component.as_os_str())?);
        }
        Ok(url)
    }

    /// Percent-encodes one path segment, refusing names SQLite cannot open.
    fn segment(name: &OsStr) -> Result<String, Error> {
        let name = name
            .to_str()
            .ok_or_else(|| Error::Invalid("database path is not valid Unicode".into()))?;
        if name.contains('\0') {
            return Err(Error::Invalid("database path contains NUL".into()));
        }
        Ok(utf8_percent_encode(name, SEGMENT).to_string())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Spaces, URI punctuation and non-ASCII names are escaped per segment;
        /// the drive colon and separators stay literal.
        #[test]
        fn drive_paths_become_percent_encoded_file_uris() {
            let url = database_url(Path::new(r"C:\Users\a b\#x?y%z\é.db")).expect("url");
            assert_eq!(url, "file:///C:/Users/a%20b/%23x%3Fy%25z/%C3%A9.db");
        }

        /// Verbatim and UNC prefixes map to the same forms SQLite accepts.
        #[test]
        fn verbatim_and_unc_prefixes_are_supported() {
            assert_eq!(
                database_url(Path::new(r"\\?\D:\state dir\state.db")).expect("verbatim"),
                "file:///D:/state%20dir/state.db"
            );
            assert_eq!(
                database_url(Path::new(r"\\server\share\dir\state.db")).expect("unc"),
                "file:////server/share/dir/state.db"
            );
        }

        /// Relative paths and paths SQLite cannot represent are refused.
        #[test]
        fn relative_and_non_unicode_paths_are_refused() {
            assert!(matches!(
                database_url(Path::new(r"dir\state.db")),
                Err(Error::Invalid(_))
            ));
            let unpaired = OsString::from_wide(&[u16::from(b'x'), 0xD800]);
            let path = Path::new(r"C:\").join(unpaired);
            assert!(matches!(database_url(&path), Err(Error::Invalid(_))));
        }

        /// Stored bytes round-trip every UTF-16 sequence, including unpaired
        /// surrogates, and a corrupt odd-length value still decodes.
        #[test]
        fn path_bytes_round_trip_utf16() {
            let mut wide: Vec<u16> = r"C:\a b\#?%é".encode_utf16().collect();
            wide.push(0xD800);
            let path = PathBuf::from(OsString::from_wide(&wide));
            let bytes = path_bytes(&path);
            assert_eq!(bytes.len(), wide.len() * 2);
            assert_eq!(path_from_bytes(bytes.clone()), path);

            let mut odd = path_bytes(Path::new("ab"));
            odd.push(b'c');
            assert_eq!(path_from_bytes(odd), PathBuf::from("ab\u{FFFD}"));
        }
    }
}
