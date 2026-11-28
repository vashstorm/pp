//! Path ↔ `file:` URI conversion: lexical absolutization, byte-wise percent encoding and strict decoding.

use std::path::{Component, Path, PathBuf};

/// Path → `file:` URI, percent-encoded byte by byte.
///
/// Absolutization is purely lexical: `..` pops the previous segment instead of being dropped — `/a/../b` must be `/b`.
/// No canonicalize: a new file may not exist yet, and semantic navigation also allows paths outside the workspace; symlinks
/// keep the literal path supplied by the caller.
pub fn path_to_uri(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::RootDir => normalized.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
        }
    }
    let mut encoded = String::from("file://");
    for component in normalized.components() {
        match component {
            Component::RootDir => encoded.push('/'),
            Component::Normal(part) => {
                encoded.push_str(&encode_segment(&part.to_string_lossy()));
                encoded.push('/');
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {}
        }
    }
    if encoded.len() > "file:///".len() && encoded.ends_with('/') {
        encoded.pop();
    }
    encoded
}

pub(super) fn encode_segment(segment: &str) -> String {
    const SAFE: &[u8] = b"-._~!$&'()*+,;=:@";
    let mut encoded = String::new();
    for byte in segment.as_bytes() {
        if byte.is_ascii_alphanumeric() || SAFE.contains(byte) {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// `file:` URI → path; a non-file scheme, a remote host and invalid encoding all return `None`.
///
/// An unescaped `?`/`#` is a URI query/fragment and not part of the file name, so it must be rejected; whereas
/// `?`/`#` decoded from `%3F`/`%23` are legitimate file name characters. A decoded NUL is always rejected.
pub(crate) fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    if rest.contains('?') || rest.contains('#') {
        return None;
    }
    let rest = match rest.strip_prefix("localhost/") {
        Some(rest) => format!("/{rest}"),
        None => rest.to_owned(),
    };
    let rest = if rest.starts_with('/') {
        rest
    } else {
        return None;
    };
    let mut bytes = Vec::new();
    let raw = rest.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' {
            let hex = raw.get(index + 1..index + 3)?;
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            bytes.push(value);
            index += 3;
        } else {
            bytes.push(raw[index]);
            index += 1;
        }
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.contains('\0') {
        return None;
    }
    Some(PathBuf::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsp_client_uri_paths_roundtrip_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        // `..` must lexically pop the previous segment, not be dropped into `/a/b`.
        let dotted = dir.path().join("a").join("..").join("b");
        assert_eq!(
            path_to_uri(&dotted),
            format!("file://{}", dir.path().join("b").display()),
            "`..` must pop the previous segment"
        );
        // A new file need not exist, and no canonicalize is done.
        let missing = dir.path().join("new file?.rs");
        let uri = path_to_uri(&missing);
        assert!(uri.ends_with("new%20file%3F.rs"), "{uri}");
        assert_eq!(uri_to_path(&uri).as_deref(), Some(missing.as_path()));

        // Percent-encoded `?`/`#` are legitimate file name characters.
        assert_eq!(
            uri_to_path("file:///tmp/a%3Fb%23c"),
            Some(PathBuf::from("/tmp/a?b#c"))
        );
        // Unescaped query/fragment, NUL and malformed percent escapes are all rejected.
        assert!(uri_to_path("file:///tmp/a?x=1").is_none());
        assert!(uri_to_path("file:///tmp/a#frag").is_none());
        assert!(uri_to_path("file:///tmp/a%00b").is_none());
        assert!(uri_to_path("file:///tmp/a%2").is_none());
        assert!(uri_to_path("http:///tmp/a").is_none());
        assert!(uri_to_path("file://remote/tmp/a").is_none());
    }
}
