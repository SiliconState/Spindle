//! Workspace download as an uncompressed POSIX ustar archive, written by hand: files,
//! empty directories and symbolic links (stored as they are, never resolved). A path
//! that fits neither ustar's 100-byte name nor a prefix/name split, or a link target
//! over 100 bytes, gets a pax record first (POSIX.1-2001), which every current tar reads.

pub(super) const BLOCK: usize = 512;

fn octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    field[..digits].copy_from_slice(&text.as_bytes()[text.len() - digits..]);
    field[digits] = 0;
}

#[allow(clippy::too_many_arguments)]
fn block(
    name: &[u8],
    prefix: &[u8],
    mode: u64,
    size: u64,
    mtime: u64,
    kind: u8,
    link: &[u8],
) -> [u8; BLOCK] {
    let mut header = [0u8; BLOCK];
    header[..name.len()].copy_from_slice(name);
    octal(&mut header[100..108], mode);
    octal(&mut header[108..116], 0);
    octal(&mut header[116..124], 0);
    octal(&mut header[124..136], size);
    octal(&mut header[136..148], mtime);
    header[148..156].fill(b' ');
    header[156] = kind;
    header[157..157 + link.len()].copy_from_slice(link);
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[345..345 + prefix.len()].copy_from_slice(prefix);
    let sum: u64 = header.iter().map(|b| u64::from(*b)).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    header
}

/// Zero bytes that pad `size` bytes of file data to a whole block.
pub(super) fn padding(size: usize) -> usize {
    (BLOCK - size % BLOCK) % BLOCK
}

/// What one archive entry is.
pub(super) enum Kind<'a> {
    File {
        size: u64,
        executable: bool,
    },
    /// An empty directory.
    Directory,
    /// A symbolic link to this target, stored as it is.
    Link(&'a str),
}

/// "LEN key=VALUE\n", where LEN counts the whole record including its own digits.
fn pax_record(key: &str, value: &str) -> String {
    let body = key.len() + value.len() + 3;
    let mut length = body + 1;
    while length != body + length.to_string().len() {
        length = body + length.to_string().len();
    }
    format!("{length} {key}={value}\n")
}

/// The longest prefix of `bytes` within `limit` that ends on a UTF-8 boundary.
fn cut(bytes: &[u8], limit: usize) -> &[u8] {
    let mut short = &bytes[..bytes.len().min(limit)];
    while std::str::from_utf8(short).is_err() {
        short = &short[..short.len() - 1];
    }
    short
}

/// The header block(s) for one entry. A path that fits neither ustar's 100-byte name
/// nor a prefix/name split, or a link target over 100 bytes, goes in a pax record.
pub(super) fn entry(path: &str, kind: Kind<'_>, mtime: u64) -> Vec<u8> {
    let (mode, size, flag, link) = match kind {
        Kind::File { size, executable } => (if executable { 0o755 } else { 0o644 }, size, b'0', ""),
        Kind::Directory => (0o755, 0, b'5', ""),
        Kind::Link(target) => (0o777, 0, b'2', target),
    };
    let path = match kind {
        Kind::Directory => format!("{path}/"),
        _ => path.to_string(),
    };
    let bytes = path.as_bytes();
    let mut records = String::new();
    let (name, prefix) = if bytes.len() <= 100 {
        (bytes, &b""[..])
    } else if let Some(split) = (0..bytes.len()).rev().find(|&at| {
        bytes[at] == b'/' && at <= 155 && bytes.len() - at - 1 <= 100 && at + 1 < bytes.len()
    }) {
        (&bytes[split + 1..], &bytes[..split])
    } else {
        records.push_str(&pax_record("path", &path));
        (cut(bytes, 100), &b""[..])
    };
    if link.len() > 100 {
        records.push_str(&pax_record("linkpath", link));
    }
    let mut out = vec![];
    if !records.is_empty() {
        out.extend_from_slice(&block(
            b"PaxHeader",
            b"",
            0o644,
            records.len() as u64,
            mtime,
            b'x',
            b"",
        ));
        out.extend_from_slice(records.as_bytes());
        out.resize(out.len() + padding(records.len()), 0);
    }
    out.extend_from_slice(&block(
        name,
        prefix,
        mode,
        size,
        mtime,
        flag,
        cut(link.as_bytes(), 100),
    ));
    out
}

/// The header block(s) for one regular file.
pub(super) fn header(path: &str, size: u64, executable: bool, mtime: u64) -> Vec<u8> {
    entry(path, Kind::File { size, executable }, mtime)
}

/// Two zero blocks end the archive.
pub(super) const END: [u8; 2 * BLOCK] = [0; 2 * BLOCK];

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(files: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut out = vec![];
        for (path, bytes, executable) in files {
            out.extend(header(path, bytes.len() as u64, *executable, 1_700_000_000));
            out.extend_from_slice(bytes);
            out.resize(out.len() + padding(bytes.len()), 0);
        }
        out.extend_from_slice(&END);
        out
    }

    #[test]
    fn ustar_headers_checksum_and_long_paths_round_trip() {
        let long_dir = format!("{}/{}/file.txt", "d".repeat(120), "e".repeat(20));
        let long_name = format!("dir/{}", "n".repeat(150));
        let files: Vec<(&str, &[u8], bool)> = vec![
            ("a.txt", b"hello\n", false),
            ("bin/run.sh", b"#!/bin/sh\necho hi\n", true),
            (&long_dir, b"prefix split", false),
            (&long_name, b"pax path", false),
            ("empty", b"", false),
        ];
        let bytes = archive(&files);
        assert_eq!(bytes.len() % BLOCK, 0);
        let first = &bytes[..BLOCK];
        assert_eq!(&first[257..263], b"ustar\0");
        let stored =
            u64::from_str_radix(std::str::from_utf8(&first[148..154]).unwrap(), 8).unwrap();
        let mut copy = first.to_vec();
        copy[148..156].fill(b' ');
        assert_eq!(stored, copy.iter().map(|b| u64::from(*b)).sum::<u64>());
        assert_eq!(&first[124..135], b"00000000006");
        // The system tar, when present, must list and extract exactly these files.
        let dir = std::env::temp_dir().join(format!("spindle-tar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.tar"), &bytes).unwrap();
        if let Ok(status) = std::process::Command::new("tar")
            .args(["-xf", "x.tar"])
            .current_dir(&dir)
            .status()
        {
            assert!(status.success());
            for (path, data, executable) in &files {
                assert_eq!(&std::fs::read(dir.join(path)).unwrap(), data, "{path}");
                #[cfg(not(unix))]
                let _ = executable;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = std::fs::metadata(dir.join(path))
                        .unwrap()
                        .permissions()
                        .mode();
                    assert_eq!(mode & 0o100 != 0, *executable, "{path}");
                }
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn empty_directories_and_links_extract_as_stored() {
        let long_target = format!("../{}", "t".repeat(150));
        let mut bytes = vec![];
        bytes.extend(entry(
            "a.txt",
            Kind::File {
                size: 2,
                executable: false,
            },
            1_700_000_000,
        ));
        bytes.extend_from_slice(b"hi");
        bytes.resize(bytes.len() + padding(2), 0);
        bytes.extend(entry("empty/inner", Kind::Directory, 1_700_000_000));
        bytes.extend(entry("link", Kind::Link("a.txt"), 1_700_000_000));
        bytes.extend(entry("far", Kind::Link(&long_target), 1_700_000_000));
        bytes.extend_from_slice(&END);
        let dir = std::env::temp_dir().join(format!("spindle-tar-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.tar"), &bytes).unwrap();
        if let Ok(status) = std::process::Command::new("tar")
            .args(["-xf", "x.tar"])
            .current_dir(&dir)
            .status()
        {
            assert!(status.success());
            assert!(dir.join("empty/inner").is_dir());
            assert_eq!(
                std::fs::read_link(dir.join("link")).unwrap().to_str(),
                Some("a.txt")
            );
            assert_eq!(
                std::fs::read_link(dir.join("far")).unwrap().to_str(),
                Some(long_target.as_str())
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
