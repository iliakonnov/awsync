use crate::serde_b64;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

fn u64_to_ascii(num: u64) -> [u8; 12] {
    // To fit into 12 bytes we need at least 41 different chars
    // For 11 bytes we need 57, but that is too much.
    let digits = b'0'..b'9'; // 10
    let upper = b'A'..b'Z'; // 25
                            // 6 more chars:
    let additional = [b'-', b'+', b'!', b'=', b'_', b'#'];

    let alphabet = additional
        .iter()
        .copied()
        .chain(digits)
        .chain(upper)
        .rev()
        .collect::<Vec<u8>>();
    assert!(alphabet.len() >= 41);
    let mut result = [alphabet[0]; 12];
    let mut idx = 0;
    let mut num = num as usize;
    while num != 0 {
        let rem = num % alphabet.len();
        let div = num / alphabet.len();
        debug_assert!(idx < 12);
        result[idx] = alphabet[rem];
        num = div;
        idx += 1;
    }
    result
}

const EXTRA_SPACE: usize = 128;

/// Represents path represented using bytes with following requirements:
/// - `/` symbol is used as separator. Windows nor ntfs-3g does not allow this character, so it is safe to use
/// - `.` symbol is encoded as b"." when possible. It is a plain ASCII symbol, so we should not have any problems.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Eq)]
pub struct EncodedPath<K: PathKind>(
    #[serde(with = "serde_b64")] Vec<u8>,
    #[serde(skip, default)] std::marker::PhantomData<K>,
);

pub trait PathKind: std::fmt::Debug + Clone + PartialEq + Serialize + Eq + Sized {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Eq)]
pub enum Local {}

impl PathKind for Local {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Eq)]
pub enum External {}

impl PathKind for External {}

// We do not support exotic platforms.
static_assertions::const_assert!(std::path::MAIN_SEPARATOR.is_ascii());

impl EncodedPath<Local> {
    pub fn from_path(path: PathBuf) -> Self {
        let os = path.into_os_string();
        let vec = os_str_bytes::OsStringBytes::into_raw_vec(os);
        EncodedPath::from_vec(vec).cast_to()
    }

    pub fn to_path(&self) -> Result<PathBuf, os_str_bytes::EncodingError> {
        let mut vec = self.0.clone();
        for i in &mut vec {
            if *i == b'/' {
                *i = std::path::MAIN_SEPARATOR as u8;
            }
        }
        os_str_bytes::OsStringBytes::from_raw_vec(vec)
    }
}

impl EncodedPath<External> {
    pub fn from_vec(mut vec: Vec<u8>) -> Self {
        let separator = std::path::MAIN_SEPARATOR as u8;
        for i in &mut vec {
            if *i == separator {
                *i = b'/';
            }
        }
        EncodedPath(vec, Default::default())
    }
}

impl<K: PathKind> EncodedPath<K> {
    pub(self) fn cast_to<T: PathKind>(self) -> EncodedPath<T> {
        EncodedPath(self.0, Default::default())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn split_parent(&self) -> (&[u8], &[u8]) {
        let slash = self
            .0
            .iter()
            .enumerate()
            .rev()
            .find(|(_, x)| **x == b'/')
            .map(|(idx, _)| idx)
            .unwrap_or_default();
        let slice = &self.0[..];
        slice.split_at(slash)
    }

    pub fn crop_name_to<L: Into<usize>>(&self, max_length: L) -> Cow<[u8]> {
        let max_length = max_length.into();
        if self.0.len() <= max_length {
            return Cow::Borrowed(&self.0);
        }

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.0.hash(&mut hasher);
        let hash = hasher.finish();

        let hash = u64_to_ascii(hash);
        let ext_start = self.0.len() - 10;
        let dot = (&self.0[ext_start..])
            .iter()
            .rposition(|&x| x == b'.')
            .unwrap_or(self.0.len());
        let (name, extension) = self.0.split_at(dot);
        let space_available = max_length - extension.len() - hash.len();
        let name = &name[..space_available];
        let res = name
            .iter()
            .chain(hash.iter())
            .chain(extension.iter())
            .copied()
            .collect::<Vec<u8>>();
        log!(warn: "Cropped name `{}` to `{}`", before=self.escaped(), after=res.escaped());
        Cow::Owned(res)
    }
}

pub trait EscapedString {
    fn escaped(&self) -> Cow<str>;
}

impl<T: PathKind> EscapedString for EncodedPath<T> {
    fn escaped(&self) -> Cow<str> {
        self.0.escaped()
    }
}

impl EscapedString for [u8] {
    fn escaped(&self) -> Cow<str> {
        let mut remaining = self;
        let mut result = String::new();
        loop {
            match std::str::from_utf8(remaining) {
                Ok(x) => {
                    return if result.is_empty() {
                        Cow::Borrowed(x)
                    } else {
                        result.push_str(x);
                        Cow::Owned(result)
                    }
                }
                Err(err) => {
                    let (valid, bad) = remaining.split_at(err.valid_up_to());
                    let (bad, rest) = bad.split_at(err.error_len().unwrap_or(0));
                    let valid = unsafe { std::str::from_utf8_unchecked(valid) };
                    remaining = rest;
                    result.push_str(valid);
                    let escaped: String = bad.iter().map(|x| format!("\\x{:02X}", x)).collect();
                    result.push_str(&escaped);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::path::EscapedString;

    #[test]
    fn test_escape_good() {
        let ascii = b"Hello world!";
        let escaped = ascii.escaped();
        assert_eq!(escaped, "Hello world!");
    }

    #[test]
    fn test_escape_zero() {
        let ascii = b"Hello \0 world!";
        let escaped = ascii.escaped();
        assert_eq!(escaped, "Hello \0 world!");
    }

    #[test]
    fn test_escape_wrong_unicode() {
        let ascii = b"Hello \xC3\x28 world!";
        let escaped = ascii.escaped();
        assert_eq!(escaped, "Hello \\xC3( world!");
    }

    #[test]
    fn test_escape_wrong_unicode_another() {
        let ascii = b"Hello \xF4\xBF\xBF\xBF world!";
        let escaped = ascii.escaped();
        assert_eq!(escaped, "Hello \\xF4\\xBF\\xBF\\xBF world!");
    }
}