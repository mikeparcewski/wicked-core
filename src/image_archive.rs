//! Read image exports as data. No extraction, filesystem traversal or image code execution.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const IMAGE_ENTRY_CAP: usize = 1_000_000;
const MANIFEST_SIZE_CAP: u64 = 4 * 1024 * 1024;

fn image_entry(count: &mut usize) -> Result<(), String> {
    *count += 1;
    if *count > IMAGE_ENTRY_CAP {
        Err("seat image archive entry-count cap exceeded".into())
    } else {
        Ok(())
    }
}

/// Archive paths are POSIX paths regardless of the daemon's host. No filesystem extraction.
fn image_path(raw: &str) -> Result<String, String> {
    let mut parts = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err("seat image archive has a parent path component".into()),
            p if p.contains('\\') => {
                return Err("seat image archive has a backslash path component".into())
            }
            p => parts.push(p),
        }
    }
    Ok(parts.join("/"))
}

/// The archive path a link at `path` names: a symlink resolves against the link's own directory
/// (docker's legacy `save` links a repeated layer as `../<id>/layer.tar`), a hard link against the
/// archive root. `..` may not climb above the archive root.
fn resolve_archive_link(path: &str, link: &str, symlink: bool) -> Result<String, String> {
    let mut parts: Vec<&str> = Vec::new();
    if symlink && !link.starts_with('/') {
        parts.extend(path.split('/').filter(|p| !p.is_empty()));
        parts.pop();
    }
    for part in link.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts
                    .pop()
                    .ok_or("seat image archive link climbs above the archive")?;
            }
            p if p.contains('\\') => {
                return Err("seat image archive has a backslash path component".into())
            }
            p => parts.push(p),
        }
    }
    Ok(parts.join("/"))
}

fn remove_image_subtree(paths: &mut BTreeMap<PathBuf, bool>, path: &Path, children_only: bool) {
    paths.retain(|p, _| !(p.starts_with(path) && (!children_only || p != path)));
}

fn tar_string(field: &[u8]) -> Result<String, String> {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    std::str::from_utf8(&field[..end])
        .map(str::to_string)
        .map_err(|_| "seat image archive has a non-Unicode path".into())
}

fn tar_octal(field: &[u8]) -> Result<u64, String> {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    let text = std::str::from_utf8(&field[..end])
        .map_err(|_| "invalid tar octal field")?
        .trim_matches(' ');
    if text.is_empty() {
        return Ok(0);
    }
    if !text.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return Err("invalid tar octal field".into());
    }
    u64::from_str_radix(text, 8).map_err(|_| "invalid tar octal field".into())
}

fn tar_size(field: &[u8]) -> Result<u64, String> {
    if field[0] & 0x80 == 0 {
        return tar_octal(field);
    }
    if field[0] & 0x40 != 0 {
        return Err("negative tar base-256 size".into());
    }
    let mut size = u64::from(field[0] & 0x7f);
    for byte in &field[1..] {
        size = size
            .checked_mul(256)
            .and_then(|n| n.checked_add(u64::from(*byte)))
            .ok_or("tar base-256 size exceeds u64")?;
    }
    Ok(size)
}

fn valid_tar_checksum(header: &[u8]) -> bool {
    let sum: u64 = header
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u64::from(*b)
            }
        })
        .sum();
    tar_octal(&header[148..156]) == Ok(sum)
}

#[derive(Debug)]
struct TarEntry {
    path: String,
    kind: u8,
    link: String,
    offset: u64,
    size: u64,
}

/// A sequential tar reader. Entry data stays in the stream until read or skipped by `next`.
/// Position includes every header, extension and padding byte, including for an outer File.
struct TarReader<R> {
    inner: R,
    position: u64,
    remaining: u64,
    padding: u64,
    ended: bool,
    next_path: Option<String>,
    next_link: Option<String>,
}

impl<R: Read> TarReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            position: 0,
            remaining: 0,
            padding: 0,
            ended: false,
            next_path: None,
            next_link: None,
        }
    }

    fn advance(&mut self, bytes: u64) -> Result<(), String> {
        self.position = self
            .position
            .checked_add(bytes)
            .ok_or("tar stream position exceeds u64")?;
        Ok(())
    }

    fn skip(&mut self, size: u64) -> Result<(), String> {
        let copied = std::io::copy(&mut (&mut self.inner).take(size), &mut std::io::sink())
            .map_err(|e| e.to_string())?;
        self.advance(copied)?;
        if copied != size {
            return Err("unexpected EOF inside tar entry data or padding".into());
        }
        Ok(())
    }

    fn data(&mut self, cap: u64) -> Result<Vec<u8>, String> {
        if self.remaining > cap {
            return Err("seat image tar metadata is too large".into());
        }
        let mut bytes = Vec::new();
        let size = self.remaining;
        (&mut self.inner)
            .take(size)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        self.advance(bytes.len() as u64)?;
        if bytes.len() as u64 != size {
            return Err("unexpected EOF inside tar entry data".into());
        }
        self.remaining = 0;
        Ok(bytes)
    }

    fn pax(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let space = rest
                .iter()
                .position(|b| *b == b' ')
                .ok_or("invalid PAX record")?;
            if space == 0 || !rest[..space].iter().all(u8::is_ascii_digit) {
                return Err("invalid PAX record length".into());
            }
            let len: usize = std::str::from_utf8(&rest[..space])
                .map_err(|_| "invalid PAX record length")?
                .parse()
                .map_err(|_| "invalid PAX record length")?;
            if len <= space + 1 || len > rest.len() || rest[len - 1] != b'\n' {
                return Err("invalid PAX record length or terminator".into());
            }
            let record = &rest[space + 1..len - 1];
            let equals = record
                .iter()
                .position(|b| *b == b'=')
                .ok_or("invalid PAX record")?;
            let key = &record[..equals];
            if key == b"path" || key == b"linkpath" {
                let value = std::str::from_utf8(&record[equals + 1..])
                    .map_err(|_| "seat image archive has a non-Unicode path")?
                    .to_string();
                if key == b"path" {
                    self.next_path = Some(value);
                } else {
                    self.next_link = Some(value);
                }
            }
            rest = &rest[len..];
        }
        Ok(())
    }

    fn next(&mut self, count: &mut usize) -> Result<Option<TarEntry>, String> {
        if self.ended {
            return Ok(None);
        }
        loop {
            self.skip(self.remaining)?;
            self.remaining = 0;
            self.skip(self.padding)?;
            self.padding = 0;
            let mut header = [0; 512];
            self.inner
                .read_exact(&mut header)
                .map_err(|e| format!("could not read tar header: {e}"))?;
            self.advance(512)?;
            if header.iter().all(|b| *b == 0) {
                self.ended = true;
                return Ok(None);
            }
            if !valid_tar_checksum(&header) {
                return Err("bad tar header checksum".into());
            }
            // Count extension headers too, so a metadata-only stream cannot evade the cap.
            image_entry(count)?;
            let size = tar_size(&header[124..136])?;
            self.remaining = size;
            self.padding = (512 - size % 512) % 512;
            let kind = header[156];
            match kind {
                b'L' | b'K' | b'x' => {
                    // Bound untrusted metadata allocations just as for the image manifest.
                    let bytes = self.data(MANIFEST_SIZE_CAP)?;
                    match kind {
                        b'L' => self.next_path = Some(tar_string(&bytes)?),
                        b'K' => self.next_link = Some(tar_string(&bytes)?),
                        _ => self.pax(&bytes)?,
                    }
                    continue;
                }
                b'g' => continue,
                _ => {}
            }
            let mut path = tar_string(&header[..100])?;
            if header[257..263].starts_with(b"ustar") {
                let prefix = tar_string(&header[345..500])?;
                if !prefix.is_empty() {
                    path = format!("{prefix}/{path}");
                }
            }
            let link = if matches!(kind, b'1' | b'2') {
                tar_string(&header[157..257])?
            } else {
                String::new()
            };
            return Ok(Some(TarEntry {
                path: self.next_path.take().unwrap_or(path),
                kind,
                link: self.next_link.take().unwrap_or(link),
                offset: self.position,
                size,
            }));
        }
    }

    fn finish(mut self) -> Result<(), String> {
        // Consume gzip trailers too: a checksum/truncation error must never become facts.
        std::io::copy(&mut self.inner, &mut std::io::sink()).map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn fold_layer<R: Read>(
    reader: R,
    paths: &mut BTreeMap<PathBuf, bool>,
    count: &mut usize,
) -> Result<(), String> {
    let mut tar = TarReader::new(reader);
    let mut entries = Vec::new();
    let mut removals = Vec::new();
    while let Some(entry) = tar.next(count)? {
        let path = image_path(&entry.path)?;
        let (parent, leaf) = path.rsplit_once('/').unwrap_or(("", &path));
        let parent = PathBuf::from(format!("/{parent}"));
        if leaf == ".wh..wh..opq" {
            removals.push((parent, true));
        } else if let Some(target) = leaf.strip_prefix(".wh.") {
            if target.is_empty() {
                return Err("seat image has an invalid whiteout".into());
            }
            removals.push((parent.join(target), false));
        } else {
            let target = if entry.kind == b'1' {
                if entry.link.is_empty() {
                    return Err("seat image hard link has no target".into());
                }
                Some(PathBuf::from(format!("/{}", image_path(&entry.link)?)))
            } else {
                None
            };
            entries.push((PathBuf::from(format!("/{path}")), entry.kind, target));
        }
    }
    tar.finish()?;
    // Whiteouts affect previous layers, independent of their order within this layer.
    for (path, children_only) in removals {
        remove_image_subtree(paths, &path, children_only);
    }
    for (path, kind, target) in entries {
        // A hard link copies the target's inode kind at this point in the layer fold.
        // An unresolved target cannot prove a non-link destination, so refuse it.
        let link = if let Some(target) = target {
            *paths.get(&target).ok_or_else(|| {
                format!(
                    "seat image has an unresolved hard link {} -> {}",
                    path.display(),
                    target.display()
                )
            })?
        } else {
            kind == b'2'
        };
        if kind == b'5' {
            paths.remove(&path);
        } else {
            remove_image_subtree(paths, &path, false);
        }
        paths.insert(path, link);
    }
    Ok(())
}

pub(crate) fn archive_symlinks(archive: &Path, id: &str) -> Result<BTreeSet<PathBuf>, String> {
    let parse = || -> Result<BTreeSet<PathBuf>, String> {
        let file = std::fs::File::open(archive).map_err(|e| format!("unreadable archive: {e}"))?;
        let mut outer = TarReader::new(file);
        let mut blobs = std::collections::HashMap::new();
        let mut aliases = std::collections::HashMap::new();
        let mut manifest = None;
        let mut count = 0;
        while let Some(entry) = outer.next(&mut count)? {
            let path = image_path(&entry.path)?;
            if matches!(entry.kind, b'1' | b'2') {
                let target = resolve_archive_link(&path, &entry.link, entry.kind == b'2')?;
                if aliases.insert(path.clone(), target).is_some() {
                    return Err(format!("duplicate image archive path {path}"));
                }
                continue;
            }
            if !matches!(entry.kind, b'0' | 0) {
                continue;
            }
            if blobs
                .insert(path.clone(), (entry.offset, entry.size))
                .is_some()
            {
                return Err(format!("duplicate image archive path {path}"));
            }
            if path == "manifest.json" {
                if entry.size > MANIFEST_SIZE_CAP {
                    return Err("seat image manifest is too large".into());
                }
                let bytes = outer.data(MANIFEST_SIZE_CAP)?;
                manifest = Some(
                    serde_json::from_slice::<serde_json::Value>(&bytes)
                        .map_err(|e| e.to_string())?,
                );
            }
        }
        // A linked blob (a repeated layer) reads its target's bytes; a few hops at most.
        for (alias, target) in &aliases {
            let mut at = target.clone();
            for _ in 0..8 {
                match aliases.get(&at) {
                    Some(next) => at = next.clone(),
                    None => break,
                }
            }
            let blob = *blobs
                .get(&at)
                .ok_or_else(|| format!("seat image archive link {alias} names no file"))?;
            if blobs.insert(alias.clone(), blob).is_some() {
                return Err(format!("duplicate image archive path {alias}"));
            }
        }
        let manifest = manifest.ok_or("seat image archive has no manifest.json")?;
        let rows = manifest
            .as_array()
            .ok_or("seat image manifest is not an array")?;
        let [row] = rows.as_slice() else {
            return Err("seat image archive must describe exactly one image".into());
        };
        let config = image_path(
            row["Config"]
                .as_str()
                .ok_or("seat image manifest has no config digest")?,
        )?;
        let hex = id.strip_prefix("sha256:").unwrap_or(id);
        let name = config.rsplit('/').next().unwrap_or("");
        if (name != hex && name != format!("{hex}.json")) || !blobs.contains_key(&config) {
            return Err(format!(
                "seat image config digest does not match inspected id {id}"
            ));
        }
        let layers = row["Layers"]
            .as_array()
            .ok_or("seat image manifest has no layers")?;
        let mut paths = BTreeMap::new();
        for layer in layers {
            let name = image_path(layer.as_str().ok_or("invalid seat image layer path")?)?;
            let (offset, size) = blobs
                .get(&name)
                .ok_or_else(|| format!("seat image layer {name} is missing"))?;
            let mut file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
            file.seek(SeekFrom::Start(*offset))
                .map_err(|e| e.to_string())?;
            let mut blob = file.take(*size);
            let mut head = Vec::new();
            (&mut blob)
                .take(512)
                .read_to_end(&mut head)
                .map_err(|e| e.to_string())?;
            let gzip = head.starts_with(&[0x1f, 0x8b]);
            let plain =
                head.len() == 512 && (head.iter().all(|b| *b == 0) || valid_tar_checksum(&head));
            if !gzip && !plain {
                return Err(
                    "seat image layer compression not supported (use plain tar or gzip layers)"
                        .into(),
                );
            }
            let stream = std::io::Cursor::new(head).chain(blob);
            let reader: Box<dyn Read> = if gzip {
                Box::new(flate2::read::MultiGzDecoder::new(stream))
            } else {
                Box::new(stream)
            };
            fold_layer(reader, &mut paths, &mut count)?;
        }
        Ok(paths
            .into_iter()
            .filter_map(|(p, link)| link.then_some(p))
            .collect())
    };
    parse().map_err(|e| {
        format!(
            "could not inspect seat image archive: {e}; \
             rebuild/export the image with plain tar or gzip layers"
        )
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn checksum(header: &mut [u8]) {
        header[148..156].fill(b' ');
        let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    }

    /// Small ustar writer shared by image inspection and arm tests. Never extracts an archive.
    pub(crate) fn image_tar(entries: &[(&str, u8, &[u8], &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        for &(name, kind, data, linkname) in entries {
            let mut header = [0; 512];
            let (prefix, leaf) = if name.len() > 100 {
                name.rsplit_once('/')
                    .expect("long ustar name needs a prefix")
            } else {
                ("", name)
            };
            assert!(prefix.len() <= 155 && leaf.len() <= 100 && linkname.len() <= 100);
            header[..leaf.len()].copy_from_slice(leaf.as_bytes());
            header[100..108].copy_from_slice(b"0000755\0");
            header[108..116].copy_from_slice(b"0000000\0");
            header[116..124].copy_from_slice(b"0000000\0");
            header[136..148].copy_from_slice(b"00000000000\0");
            header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
            header[156] = kind;
            header[157..157 + linkname.len()].copy_from_slice(linkname.as_bytes());
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            header[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
            checksum(&mut header);
            out.extend_from_slice(&header);
            out.extend_from_slice(data);
            out.resize(out.len() + (512 - data.len() % 512) % 512, 0);
        }
        out.resize(out.len() + 1024, 0);
        out
    }

    fn pax_record(key: &str, value: &str) -> Vec<u8> {
        let body = format!(" {key}={value}\n");
        let mut len = body.len() + 1;
        loop {
            let record = format!("{len}{body}");
            if record.len() == len {
                return record.into_bytes();
            }
            len = record.len();
        }
    }

    #[test]
    fn ustar_prefix_and_name_are_joined() {
        let name = format!("{}/leaf", "prefix".repeat(20));
        let bytes = image_tar(&[(&name, b'0', b"data", "")]);
        let mut tar = TarReader::new(bytes.as_slice());
        let entry = tar.next(&mut 0).unwrap().unwrap();
        assert_eq!(entry.path, name);
        assert_eq!((entry.offset, entry.size), (512, 4));
        assert!(tar.next(&mut 0).unwrap().is_none());
    }

    #[test]
    fn gnu_long_name_and_link_apply_to_the_next_entry() {
        let name = "long-name/".repeat(40);
        let target = "target/".repeat(40);
        let long_name = format!("{name}\0");
        let long_link = format!("{target}\0");
        let bytes = image_tar(&[
            ("././@LongLink", b'L', long_name.as_bytes(), ""),
            ("././@LongLink", b'K', long_link.as_bytes(), ""),
            ("short", b'1', b"", "short-target"),
            ("normal", b'0', b"", ""),
        ]);
        let mut tar = TarReader::new(bytes.as_slice());
        let mut count = 0;
        let entry = tar.next(&mut count).unwrap().unwrap();
        assert_eq!(entry.path, name);
        assert_eq!(entry.link, target);
        assert_eq!(entry.offset, 2560);
        assert_eq!(tar.next(&mut count).unwrap().unwrap().path, "normal");
        assert!(tar.next(&mut count).unwrap().is_none());
        assert_eq!(count, 4);
    }

    #[test]
    fn pax_path_and_linkpath_apply_once_and_global_headers_are_skipped() {
        let mut records = pax_record("path", "pax/path");
        records.extend(pax_record("linkpath", "pax/target"));
        let global = pax_record("path", "ignored/global");
        let bytes = image_tar(&[
            ("global", b'g', &global, ""),
            ("extended", b'x', &records, ""),
            ("short", b'1', b"", "short-target"),
            ("normal", b'0', b"", ""),
        ]);
        let mut tar = TarReader::new(bytes.as_slice());
        let entry = tar.next(&mut 0).unwrap().unwrap();
        assert_eq!(entry.path, "pax/path");
        assert_eq!(entry.link, "pax/target");
        assert_eq!(tar.next(&mut 0).unwrap().unwrap().path, "normal");
        assert!(tar.next(&mut 0).unwrap().is_none());
    }

    #[test]
    fn base_256_size_is_read_and_invalid_ranges_are_refused() {
        let mut bytes = image_tar(&[("file", 0, b"abc", "")]);
        bytes[124..136].fill(0);
        bytes[124] = 0x80;
        bytes[135] = 3;
        checksum(&mut bytes[..512]);
        let mut tar = TarReader::new(bytes.as_slice());
        let entry = tar.next(&mut 0).unwrap().unwrap();
        assert_eq!(entry.size, 3);
        assert_eq!(tar.data(3).unwrap(), b"abc");
        assert!(tar.next(&mut 0).unwrap().is_none());
        let mut field = [0; 12];
        field[0] = 0xff;
        assert!(tar_size(&field).unwrap_err().contains("negative"));
        field[0] = 0x80;
        field[3] = 1;
        assert!(tar_size(&field).unwrap_err().contains("exceeds u64"));
        field[3] = 0;
        field[4..].fill(0xff);
        assert_eq!(tar_size(&field).unwrap(), u64::MAX);
    }

    #[test]
    fn bad_checksum_is_refused() {
        let mut bytes = image_tar(&[("file", b'0', b"", "")]);
        bytes[0] ^= 1;
        let error = TarReader::new(bytes.as_slice()).next(&mut 0).unwrap_err();
        assert!(error.contains("checksum"), "{error}");
    }

    #[test]
    fn eof_inside_data_or_header_is_refused() {
        let bytes = image_tar(&[("file", b'0', b"abc", "")]);
        let mut tar = TarReader::new(&bytes[..514]);
        tar.next(&mut 0).unwrap().unwrap();
        assert!(tar.next(&mut 0).unwrap_err().contains("EOF"));
        let mut tar = TarReader::new(&bytes[..514]);
        tar.next(&mut 0).unwrap().unwrap();
        assert!(tar.data(3).unwrap_err().contains("EOF"));
        let mut tar = TarReader::new(&bytes[..100]);
        assert!(tar.next(&mut 0).unwrap_err().contains("header"));
        let mut tar = TarReader::new(&bytes[..1024]);
        tar.next(&mut 0).unwrap().unwrap();
        assert!(tar.next(&mut 0).unwrap_err().contains("header"));
    }

    #[test]
    fn the_first_zero_header_ends_the_archive() {
        let mut tar = TarReader::new(&[0; 512][..]);
        assert!(tar.next(&mut 0).unwrap().is_none());
        assert!(tar.next(&mut 0).unwrap().is_none());
        tar.finish().unwrap();
    }

    #[test]
    fn entry_data_and_padding_are_skipped_before_the_next_header() {
        let bytes = image_tar(&[("first", b'0', b"abc", ""), ("second", b'0', b"defg", "")]);
        let mut tar = TarReader::new(bytes.as_slice());
        assert_eq!(tar.next(&mut 0).unwrap().unwrap().offset, 512);
        let entry = tar.next(&mut 0).unwrap().unwrap();
        assert_eq!(entry.path, "second");
        assert_eq!((entry.offset, entry.size), (1536, 4));
        assert_eq!(tar.data(4).unwrap(), b"defg");
        assert!(tar.next(&mut 0).unwrap().is_none());
    }

    #[test]
    fn malformed_pax_records_are_refused() {
        for data in [b"0 path=x\n".as_slice(), b"40 path=x\n", b"10 path=x!"] {
            let bytes = image_tar(&[("pax", b'x', data, "")]);
            assert!(TarReader::new(bytes.as_slice()).next(&mut 0).is_err());
        }
    }

    #[test]
    fn seat_archive_entry_count_is_capped_including_extensions() {
        let mut count = IMAGE_ENTRY_CAP - 1;
        image_entry(&mut count).unwrap();
        assert!(image_entry(&mut count)
            .unwrap_err()
            .contains("entry-count cap"));
        let bytes = image_tar(&[("long", b'L', b"name\0", ""), ("short", b'0', b"", "")]);
        let mut count = IMAGE_ENTRY_CAP - 1;
        assert!(TarReader::new(bytes.as_slice())
            .next(&mut count)
            .unwrap_err()
            .contains("entry-count cap"));
    }

    #[test]
    fn fold_whiteouts_opaque_directories_and_link_replacements() {
        let base = image_tar(&[
            ("gone", b'2', b"", "elsewhere"),
            ("opaque/old", b'2', b"", "elsewhere"),
            ("replace", b'2', b"", "elsewhere"),
            ("removed/old", b'2', b"", "elsewhere"),
            ("keep/old", b'2', b"", "elsewhere"),
        ]);
        let top = image_tar(&[
            ("opaque/new", b'2', b"", "elsewhere"),
            (".wh.gone", b'0', b"", ""),
            ("opaque/.wh..wh..opq", b'0', b"", ""),
            ("replace", b'5', b"", ""),
            (".wh.removed", b'0', b"", ""),
            ("keep", b'5', b"", ""),
        ]);
        let mut paths = BTreeMap::new();
        let mut count = 0;
        fold_layer(base.as_slice(), &mut paths, &mut count).unwrap();
        fold_layer(top.as_slice(), &mut paths, &mut count).unwrap();
        let links: Vec<_> = paths
            .into_iter()
            .filter(|(_, link)| *link)
            .map(|(p, _)| p)
            .collect();
        assert_eq!(
            links,
            [PathBuf::from("/keep/old"), PathBuf::from("/opaque/new")]
        );
    }

    #[test]
    fn hard_links_copy_the_target_kind_and_unknown_types_are_not_links() {
        let bytes = image_tar(&[
            ("link", b'2', b"", "elsewhere"),
            ("hard", b'1', b"", "link"),
            ("link", b'5', b"", ""),
            ("device", b'3', b"", "link"),
            ("fifo", b'6', b"", "hard"),
        ]);
        let mut paths = BTreeMap::new();
        fold_layer(bytes.as_slice(), &mut paths, &mut 0).unwrap();
        assert_eq!(paths.get(Path::new("/hard")), Some(&true));
        for name in ["/link", "/device", "/fifo"] {
            assert_eq!(paths.get(Path::new(name)), Some(&false));
        }
        let bytes = image_tar(&[("hard", b'1', b"", "missing")]);
        assert!(fold_layer(bytes.as_slice(), &mut paths, &mut 0)
            .unwrap_err()
            .contains("unresolved hard link"));
    }

    #[test]
    fn unsafe_paths_and_hard_link_targets_are_refused() {
        for path in ["../escape", "a/../escape", "a\\escape"] {
            assert!(image_path(path).is_err());
            let bytes = image_tar(&[(path, b'0', b"", "")]);
            assert!(fold_layer(bytes.as_slice(), &mut BTreeMap::new(), &mut 0).is_err());
            let bytes = image_tar(&[("hard", b'1', b"", path)]);
            assert!(fold_layer(bytes.as_slice(), &mut BTreeMap::new(), &mut 0).is_err());
        }
    }
}

#[cfg(test)]
mod archive_tests {
    use super::tests::image_tar;
    use super::*;
    use std::io::Write;

    struct ArchiveFile(PathBuf);

    impl ArchiveFile {
        fn new(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "wicked-image-archive-{}.tar",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }

    impl Drop for ArchiveFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn image(layer: &[u8]) -> Vec<u8> {
        image_tar(&[
            (
                "manifest.json",
                b'0',
                br#"[{"Config":"abc.json","Layers":["layer"]}]"#,
                "",
            ),
            ("abc.json", b'0', b"{}", ""),
            ("layer", b'0', layer, ""),
        ])
    }

    #[test]
    fn outer_file_offsets_locate_layers_and_duplicate_paths_are_refused() {
        let layer = image_tar(&[("link", b'2', b"", "elsewhere")]);
        let archive = ArchiveFile::new(&image(&layer));
        assert_eq!(
            archive_symlinks(&archive.0, "sha256:abc").unwrap(),
            BTreeSet::from([PathBuf::from("/link")])
        );
        let bytes = image_tar(&[("a", b'0', b"first", ""), ("./a", b'0', b"second", "")]);
        let archive = ArchiveFile::new(&bytes);
        assert!(archive_symlinks(&archive.0, "sha256:abc")
            .unwrap_err()
            .contains("duplicate image archive path"));
    }

    /// Docker's legacy `save` writes a repeated layer once and links the others to it
    /// (`<id>/layer.tar -> ../<first>/layer.tar`): the link reads its target's bytes.
    #[test]
    fn a_linked_repeated_layer_reads_its_target_and_cannot_climb_out() {
        let base = image_tar(&[("floor", b'2', b"", "elsewhere")]);
        let manifest = br#"[{"Config":"abc.json","Layers":["one/layer.tar","two/layer.tar"]}]"#;
        let archive = ArchiveFile::new(&image_tar(&[
            ("manifest.json", b'0', manifest, ""),
            ("abc.json", b'0', b"{}", ""),
            ("one/layer.tar", b'0', &base, ""),
            ("two/layer.tar", b'2', b"", "../one/layer.tar"),
        ]));
        assert_eq!(
            archive_symlinks(&archive.0, "sha256:abc").unwrap(),
            BTreeSet::from([PathBuf::from("/floor")])
        );
        assert!(resolve_archive_link("two/layer.tar", "../../x", true).is_err());
        assert_eq!(
            resolve_archive_link("two/layer.tar", "/one/layer.tar", true).unwrap(),
            "one/layer.tar"
        );
        assert_eq!(
            resolve_archive_link("b", "one/layer.tar", false).unwrap(),
            "one/layer.tar"
        );
    }

    #[test]
    fn gzip_trailer_corruption_and_truncation_are_refused() {
        let layer = image_tar(&[("link", b'2', b"", "elsewhere")]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&layer).unwrap();
        let good = gz.finish().unwrap();
        let archive = ArchiveFile::new(&image(&good));
        assert!(archive_symlinks(&archive.0, "sha256:abc").is_ok());
        let mut corrupt = good.clone();
        let crc = corrupt.len() - 8;
        corrupt[crc] ^= 1;
        for layer in [&corrupt[..], &good[..good.len() - 4]] {
            let archive = ArchiveFile::new(&image(layer));
            assert!(archive_symlinks(&archive.0, "sha256:abc").is_err());
        }
    }

    #[test]
    fn oversized_manifest_is_refused_before_reading_its_data() {
        let mut bytes = image_tar(&[("manifest.json", b'0', b"", "")]);
        bytes[124..136].copy_from_slice(b"00020000001\0");
        bytes[148..156].fill(b' ');
        let sum: u32 = bytes[..512].iter().map(|b| u32::from(*b)).sum();
        bytes[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let archive = ArchiveFile::new(&bytes);
        assert!(archive_symlinks(&archive.0, "sha256:abc")
            .unwrap_err()
            .contains("manifest is too large"));
    }
}
