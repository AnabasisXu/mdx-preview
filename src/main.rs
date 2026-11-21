//! prev — single-file MDict (.mdx) viewer.
//!
//! `prev dictionary.mdx` serves the dictionary's headwords and entries over
//! HTTP so they can be read in a browser (or behind a reverse proxy).
//!
//! Everything is in this one binary: the MDX v2 reader, the embedded web UI,
//! and the HTTP/WebSocket server.

use actix_web::{get, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use actix_ws::Message;
use encoding_rs::Encoding;
use flate2::read::ZlibDecoder;
use futures_util::StreamExt;
use memmap2::Mmap;
use ripemd::{Digest, Ripemd128};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::broadcast;

const INDEX_HTML: &str = include_str!("static/index.html");
const ITEM_HTML: &str = include_str!("static/item.html");
const SITE_CSS: &str = include_str!("static/site.css");
const UTILS_JS: &str = include_str!("static/utils.js");

// Public mount prefix, e.g. "/mdx-preview"; empty means site root.
static BASE: OnceLock<String> = OnceLock::new();
fn base() -> &'static str {
    BASE.get().map(String::as_str).unwrap_or("")
}
fn rebase(text: &str) -> String {
    let b = base();
    if b.is_empty() {
        return text.to_string();
    }
    text.replace("/dist/", &format!("{b}/dist/"))
        .replace("/api/", &format!("{b}/api/"))
}

/// Accept both `/mdx-preview` and `mdx-preview/`; `/` and `""` mean site root.
fn normalize_base(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

// ---------------------------------------------------------------- MDX reader

fn be_u16(d: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([d[i], d[i + 1]])
}
fn be_u32(d: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]])
}
fn be_u64(d: &[u8], i: usize) -> u64 {
    u64::from_be_bytes(d[i..i + 8].try_into().unwrap())
}
fn le_u32(d: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]])
}
fn unzlib(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    ZlibDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

/// MDict `Encrypted=2` obfuscates the key block info with a rolling XOR whose
/// key is derived from the block's adler32 field. Reverse-engineered scheme;
/// see readmdict's `_mdx_decrypt`.
fn mdx_decrypt(block: &[u8]) -> Vec<u8> {
    let mut out = block.to_vec();
    if out.len() <= 8 {
        return out;
    }
    let mut hasher = Ripemd128::new();
    hasher.update(&block[4..8]);
    hasher.update(0x3695u32.to_le_bytes());
    let key = hasher.finalize();
    let mut previous = 0x36u8;
    for (i, &b) in block[8..].iter().enumerate() {
        out[8 + i] = b.rotate_left(4) ^ previous ^ (i as u8) ^ key[i % key.len()];
        previous = b;
    }
    out
}
fn utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Decode record/key bytes with the encoding declared in the container header.
/// `None` means the caller already handles UTF-16.
fn decode(bytes: &[u8], codec: Option<&'static Encoding>) -> String {
    match codec {
        Some(enc) => enc.decode(bytes).0.into_owned(),
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn parse_header(text: &str) -> HashMap<String, String> {
    let b = text.as_bytes();
    let mut m = HashMap::new();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && !(b[i].is_ascii_alphanumeric() || b[i] == b'_') {
            i += 1;
        }
        let ks = i;
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
            i += 1;
        }
        if i + 1 < b.len() && b[i] == b'=' && b[i + 1] == b'"' {
            let key = &text[ks..i];
            i += 2;
            let vs = i;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            if !key.is_empty() {
                m.insert(key.to_string(), text[vs..i].to_string());
            }
            i += 1;
        }
    }
    m
}

/// Read one number whose width depends on the container version.
fn read_num(d: &[u8], p: &mut usize, width: usize) -> u64 {
    let v = if width == 8 {
        let v = be_u64(d, *p);
        *p += 8;
        v
    } else {
        let v = be_u32(d, *p) as u64;
        *p += 4;
        v
    };
    v
}

struct RecordBlock {
    offset: usize,
    comp: usize,
    decomp: usize,
}

/// Memory-mapped .mdx with lazy record decompression. Keeping only the map and
/// the key index makes resident memory track the file size instead of the much
/// larger decompressed text.
struct Mdx {
    map: Mmap,
    blocks: Vec<RecordBlock>,
    starts: Vec<u64>,
    ends: Vec<u64>,
    words: Vec<String>,
    /// Entry ids sorted by headword, for `@@@LINK=` lookups.
    sorted: Vec<u32>,
    encoding: String,
    codec: Option<&'static Encoding>,
}

impl Mdx {
    fn decompress(&self, b: &RecordBlock) -> io::Result<Vec<u8>> {
        let buf = &self.map[b.offset..b.offset + b.comp];
        match le_u32(buf, 0) {
            0 => Ok(buf[8..].to_vec()),
            2 => unzlib(&buf[8..]),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LZO compression is not supported",
            )),
        }
    }

    fn raw(&self, start: usize, end: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(end.saturating_sub(start));
        let mut base = 0usize;
        for b in &self.blocks {
            let bend = base + b.decomp;
            if bend <= start {
                base = bend;
                continue;
            }
            if base >= end {
                break;
            }
            let block = self.decompress(b)?;
            let lo = start.saturating_sub(base);
            let hi = (end - base).min(block.len());
            if lo < hi {
                out.extend_from_slice(&block[lo..hi]);
            }
            base = bend;
        }
        Ok(out)
    }

    fn text_at(&self, idx: usize) -> Option<String> {
        let start = *self.starts.get(idx)? as usize;
        let end = *self.ends.get(idx)? as usize;
        let raw = self.raw(start, end).ok()?;
        let text = decode(&raw, self.codec);
        Some(text.trim_end_matches('\0').to_string())
    }

    /// Entry text with `@@@LINK=` redirects followed (bounded, cycle-safe).
    fn text(&self, id: u32) -> Option<String> {
        let idx = (id as usize).checked_sub(1)?;
        let mut cur = idx;
        for _ in 0..16 {
            let text = self.text_at(cur)?;
            let Some(target) = text.trim().strip_prefix("@@@LINK=") else {
                return Some(text);
            };
            let target = target.trim();
            match self.find(target) {
                Some(next) if next != cur => cur = next,
                _ => return Some(text),
            }
        }
        self.text_at(idx)
    }

    fn find(&self, word: &str) -> Option<usize> {
        self.sorted
            .binary_search_by(|id| self.words[*id as usize].as_str().cmp(word))
            .ok()
            .map(|pos| self.sorted[pos] as usize)
    }

    fn word(&self, id: u32) -> Option<&str> {
        (id as usize)
            .checked_sub(1)
            .and_then(|i| self.words.get(i))
            .map(String::as_str)
    }
}

/// Parse an .mdx container into an index plus its record blocks.
fn load_mdx(path: &str) -> io::Result<Mdx> {
    let file = File::open(path)?;
    let map = unsafe { Mmap::map(&file)? };
    let d: &[u8] = &map;
    if d.len() < 8 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file too small"));
    }
    let hlen = be_u32(&d, 0) as usize;
    let header = utf16le(&d[4..4 + hlen]);
    let h = parse_header(&header);
    let encoding = h.get("Encoding").cloned().unwrap_or_else(|| "UTF-8".into());
    let version: f32 = h
        .get("GeneratedByEngineVersion")
        .and_then(|s| s.parse().ok())
        .unwrap_or(2.0);
    let width = if version >= 2.0 { 8 } else { 4 };
    // 0 = plain, 1 = passcode-protected key numbers, 2 = obfuscated key block
    // info, 3 = both. Only the bit-2 scheme is reversible without a passcode.
    let encrypt = match h.get("Encrypted").map(String::as_str).unwrap_or("No") {
        "Yes" => 1,
        "No" | "" => 0,
        other => other.parse::<u32>().unwrap_or(0),
    };
    if encrypt & 1 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "dictionary is passcode-protected (Encrypted=1/3), which is not supported",
        ));
    }

    let mut p = 4 + hlen;
    if version >= 2.0 {
        p += 4; // adler32 of the header text
    }

    // --- key section
    let num_key_blocks = read_num(&d, &mut p, width);
    let _num_entries = read_num(&d, &mut p, width);
    if version >= 2.0 {
        let _kb_info_decomp = read_num(&d, &mut p, width);
    }
    let kb_info_size = read_num(&d, &mut p, width) as usize;
    let kb_size = read_num(&d, &mut p, width) as usize;
    if version >= 2.0 {
        p += 4; // adler32 of the five numbers
    }
    let kb_info_raw = &d[p..p + kb_info_size];
    p += kb_info_size;
    let kb_info = if version >= 2.0 {
        let block = if encrypt & 2 != 0 {
            mdx_decrypt(kb_info_raw)
        } else {
            kb_info_raw.to_vec()
        };
        unzlib(&block[8..])?
    } else {
        kb_info_raw.to_vec()
    };

    let is_utf16 = encoding.eq_ignore_ascii_case("UTF-16");
    let codec = if is_utf16 {
        None
    } else {
        Encoding::for_label(encoding.as_bytes())
    };

    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < kb_info.len() {
        let _n = read_num(&kb_info, &mut i, width);
        let head_size = if version >= 2.0 {
            let s = be_u16(&kb_info, i) as usize;
            i += 2;
            s
        } else {
            let s = kb_info[i] as usize;
            i += 1;
            s
        };
        i += if is_utf16 {
            (head_size + 1) * 2
        } else {
            head_size + 1
        };
        let tail_size = if version >= 2.0 {
            let s = be_u16(&kb_info, i) as usize;
            i += 2;
            s
        } else {
            let s = kb_info[i] as usize;
            i += 1;
            s
        };
        i += if is_utf16 {
            (tail_size + 1) * 2
        } else {
            tail_size + 1
        };
        let comp = read_num(&kb_info, &mut i, width) as usize;
        let _decomp = read_num(&kb_info, &mut i, width);
        blocks.push((comp, 0));
    }
    let _ = num_key_blocks;

    let kb_comp = &d[p..p + kb_size];
    p += kb_size;

    let mut keys: Vec<(u64, String)> = Vec::new();
    let mut off = 0;
    for (comp, _) in &blocks {
        let kind = le_u32(kb_comp, off);
        let body = &kb_comp[off + 8..off + comp];
        let block = match kind {
            0 => body.to_vec(),
            2 => unzlib(body)?,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "LZO compression is not supported",
                ))
            }
        };
        let mut j = 0;
        while j < block.len() {
            let key_id = read_num(&block, &mut j, width);
            let start = j;
            let word;
            if is_utf16 {
                let mut k = j;
                while k + 1 < block.len() && !(block[k] == 0 && block[k + 1] == 0) {
                    k += 2;
                }
                word = utf16le(&block[start..k]);
                j = k + 2;
            } else {
                let mut k = j;
                while k < block.len() && block[k] != 0 {
                    k += 1;
                }
                word = decode(&block[start..k], codec);
                j = k + 1;
            }
            keys.push((key_id, word));
        }
        off += comp;
    }

    // --- record section
    let num_record_blocks = read_num(&d, &mut p, width);
    let _num_entries2 = read_num(&d, &mut p, width);
    let _rb_info_size = read_num(&d, &mut p, width);
    let _rb_size = read_num(&d, &mut p, width);
    let mut info: Vec<(usize, usize)> = Vec::new();
    for _ in 0..num_record_blocks {
        let comp = read_num(&d, &mut p, width) as usize;
        let decomp = read_num(&d, &mut p, width) as usize;
        info.push((comp, decomp));
    }

    let mut blocks = Vec::with_capacity(info.len());
    let mut offset = p;
    for (comp, decomp) in info {
        blocks.push(RecordBlock {
            offset,
            comp,
            decomp,
        });
        offset += comp;
    }
    let total: u64 = blocks.iter().map(|b| b.decomp as u64).sum();

    let mut starts = Vec::with_capacity(keys.len());
    let mut ends = Vec::with_capacity(keys.len());
    for idx in 0..keys.len() {
        starts.push(keys[idx].0);
        ends.push(if idx + 1 < keys.len() {
            keys[idx + 1].0
        } else {
            total
        });
    }
    let mut words = Vec::with_capacity(keys.len());
    for (_, word) in keys {
        words.push(word);
    }
    let mut sorted: Vec<u32> = (0..words.len() as u32).collect();
    sorted.sort_by(|a, b| words[*a as usize].cmp(&words[*b as usize]));
    Ok(Mdx {
        map,
        blocks,
        starts,
        ends,
        words,
        sorted,
        encoding,
        codec,
    })
}

// ---------------------------------------------------------------- MDD reader

/// One compressed record block inside an .mdd: file offset + sizes.
struct MddBlock {
    offset: u64,
    comp: usize,
    decomp: usize,
}

/// .mdd resource archive: keys are UTF-16 paths ("\dir\file.ext"), records are
/// raw file bytes. Index only; blocks are decompressed on demand.
struct Mdd {
    path: PathBuf,
    blocks: Vec<MddBlock>,
    files: HashMap<String, (usize, usize)>,
}

impl Mdd {
    fn read_block(&self, f: &mut File, b: &MddBlock) -> io::Result<Vec<u8>> {
        f.seek(SeekFrom::Start(b.offset))?;
        let mut buf = vec![0u8; b.comp];
        f.read_exact(&mut buf)?;
        match le_u32(&buf, 0) {
            0 => Ok(buf[8..].to_vec()),
            2 => unzlib(&buf[8..]),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LZO compression is not supported",
            )),
        }
    }

    fn read(&self, start: usize, end: usize) -> io::Result<Vec<u8>> {
        let mut f = File::open(&self.path)?;
        let mut out = Vec::with_capacity(end.saturating_sub(start));
        let mut base = 0usize;
        for b in &self.blocks {
            let bend = base + b.decomp;
            if bend <= start {
                base = bend;
                continue;
            }
            if base >= end {
                break;
            }
            let block = self.read_block(&mut f, b)?;
            let lo = start.saturating_sub(base);
            let hi = (end - base).min(block.len());
            if lo < hi {
                out.extend_from_slice(&block[lo..hi]);
            }
            base = bend;
        }
        Ok(out)
    }
}

fn read_exact_at(f: &mut File, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_fnum(f: &mut File, width: usize) -> io::Result<u64> {
    let raw = read_exact_at(f, width)?;
    Ok(if width == 8 {
        be_u64(&raw, 0)
    } else {
        be_u32(&raw, 0) as u64
    })
}

/// MDD keys use `\dir\file.ext`, URLs use `/dir/file.ext`; compare case-insensitively.
fn norm_res(path: &str) -> String {
    path.trim_start_matches(['/', '\\'])
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn load_mdd(path: &Path) -> io::Result<Mdd> {
    let mut f = File::open(path)?;
    let hlen = be_u32(&read_exact_at(&mut f, 4)?, 0) as usize;
    let header = utf16le(&read_exact_at(&mut f, hlen)?);
    let h = parse_header(&header);
    let version: f32 = h
        .get("GeneratedByEngineVersion")
        .and_then(|s| s.parse().ok())
        .unwrap_or(2.0);
    let width = if version >= 2.0 { 8 } else { 4 };
    let term = if version >= 2.0 { 1 } else { 0 };
    let encrypt = match h.get("Encrypted").map(String::as_str).unwrap_or("No") {
        "Yes" => 1,
        "No" | "" => 0,
        other => other.parse::<u32>().unwrap_or(0),
    };
    if encrypt & 1 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "passcode-protected (Encrypted=1/3)",
        ));
    }

    if version >= 2.0 {
        read_exact_at(&mut f, 4)?; // adler32 of the header text
    }
    let num_key_blocks = read_fnum(&mut f, width)?;
    read_fnum(&mut f, width)?; // number of entries
    if version >= 2.0 {
        read_fnum(&mut f, width)?; // decompressed size of key block info
    }
    let kb_info_size = read_fnum(&mut f, width)? as usize;
    let kb_size = read_fnum(&mut f, width)? as usize;
    if version >= 2.0 {
        read_exact_at(&mut f, 4)?; // adler32 of the five numbers
    }

    let mut kb_info = read_exact_at(&mut f, kb_info_size)?;
    if version >= 2.0 {
        if encrypt & 2 != 0 {
            kb_info = mdx_decrypt(&kb_info);
        }
        kb_info = unzlib(&kb_info[8..])?;
    }

    let mut sizes: Vec<(usize, usize)> = Vec::with_capacity(num_key_blocks as usize);
    let mut i = 0;
    while i < kb_info.len() {
        read_num(&kb_info, &mut i, width); // entries in this key block
        let hs = if version >= 2.0 {
            let s = be_u16(&kb_info, i) as usize;
            i += 2;
            s
        } else {
            let s = kb_info[i] as usize;
            i += 1;
            s
        };
        i += (hs + term) * 2;
        let ts = if version >= 2.0 {
            let s = be_u16(&kb_info, i) as usize;
            i += 2;
            s
        } else {
            let s = kb_info[i] as usize;
            i += 1;
            s
        };
        i += (ts + term) * 2;
        let comp = read_num(&kb_info, &mut i, width) as usize;
        let decomp = read_num(&kb_info, &mut i, width) as usize;
        sizes.push((comp, decomp));
    }

    let kb = read_exact_at(&mut f, kb_size)?;
    let mut keys: Vec<(usize, String)> = Vec::new();
    let mut off = 0;
    for (comp, _) in &sizes {
        let kind = le_u32(&kb, off);
        let body = &kb[off + 8..off + comp];
        let block = match kind {
            0 => body.to_vec(),
            2 => unzlib(body)?,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "LZO compression is not supported",
                ))
            }
        };
        let mut j = 0;
        while j < block.len() {
            let key_id = read_num(&block, &mut j, width) as usize;
            let start = j;
            let mut k = j;
            while k + 1 < block.len() && !(block[k] == 0 && block[k + 1] == 0) {
                k += 2;
            }
            // MDD keys are always UTF-16LE, whatever the header claims.
            keys.push((key_id, norm_res(&utf16le(&block[start..k]))));
            j = k + 2;
        }
        off += comp;
    }

    // --- record section: block directory first, then a lazy offset list.
    let num_record_blocks = read_fnum(&mut f, width)?;
    read_fnum(&mut f, width)?; // number of entries
    read_fnum(&mut f, width)?; // size of record block info
    read_fnum(&mut f, width)?; // size of record blocks
    let mut pairs: Vec<(usize, usize)> = Vec::with_capacity(num_record_blocks as usize);
    for _ in 0..num_record_blocks {
        let comp = read_fnum(&mut f, width)? as usize;
        let decomp = read_fnum(&mut f, width)? as usize;
        pairs.push((comp, decomp));
    }
    let mut offset = f.stream_position()?;
    let mut blocks = Vec::with_capacity(pairs.len());
    for (comp, decomp) in pairs {
        blocks.push(MddBlock {
            offset,
            comp,
            decomp,
        });
        offset += comp as u64;
    }

    keys.sort_by_key(|(start, _)| *start);
    let total: usize = blocks.iter().map(|b| b.decomp).sum();
    let mut files = HashMap::with_capacity(keys.len());
    for idx in 0..keys.len() {
        let (start, ref name) = keys[idx];
        let end = if idx + 1 < keys.len() {
            keys[idx + 1].0
        } else {
            total
        };
        if start < end {
            files.entry(name.clone()).or_insert((start, end));
        }
    }

    Ok(Mdd {
        path: path.to_path_buf(),
        blocks,
        files,
    })
}

/// `dict.mdx` owns `dict.mdd` plus optional `dict.1.mdd`, `dict.2.mdd`, ...
fn mdd_candidates(mdx: &str) -> Vec<PathBuf> {
    let p = Path::new(mdx);
    let dir = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let stem = match p.file_stem() {
        Some(s) => s.to_string_lossy().to_lowercase(),
        None => return Vec::new(),
    };
    let (mut plain, mut numbered) = (None, Vec::new());
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let lower = name.to_lowercase();
            let Some(base) = lower.strip_suffix(".mdd") else {
                continue;
            };
            if base == stem {
                plain = Some(path);
            } else if let Some(rest) = base.strip_prefix(&format!("{stem}.")) {
                if let Ok(n) = rest.parse::<u64>() {
                    numbered.push((n, path));
                }
            }
        }
    }
    numbered.sort_by_key(|(n, _)| *n);
    let mut out = Vec::new();
    if let Some(p) = plain {
        out.push(p);
    }
    out.extend(numbered.into_iter().map(|(_, p)| p));
    out
}

struct Resource {
    mdd: usize,
    start: usize,
    end: usize,
}

fn load_resources(mdx: &str) -> (Vec<Mdd>, HashMap<String, Resource>) {
    let mut mdds = Vec::new();
    let mut map = HashMap::new();
    for path in mdd_candidates(mdx) {
        match load_mdd(&path) {
            Ok(mdd) => {
                let idx = mdds.len();
                for (key, (start, end)) in &mdd.files {
                    map.entry(key.clone()).or_insert(Resource {
                        mdd: idx,
                        start: *start,
                        end: *end,
                    });
                }
                mdds.push(mdd);
            }
            Err(e) => eprintln!("prev: skipping {}: {e}", path.display()),
        }
    }
    (mdds, map)
}

// ---------------------------------------------------------------- web layer

struct Dict {
    /// Headwords live in the mapped file's index; a word's id is its index + 1.
    mdx: Mdx,
    /// Lowercased headwords, computed once so filtering never allocates per word.
    lower: Vec<String>,
    /// Jump bar buckets: (label, first index, last index) in file order.
    initials: Vec<(String, usize, usize)>,
}

impl Dict {
    fn page<I: Iterator<Item = usize>>(
        &self,
        idxs: I,
        q: &str,
        offset: usize,
        limit: usize,
    ) -> (Vec<(String, u32)>, bool) {
        let mut items = Vec::with_capacity(limit);
        let mut skipped = 0;
        let mut has_more = false;
        for i in idxs {
            if !q.is_empty() && !self.lower[i].contains(q) {
                continue;
            }
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if items.len() < limit {
                items.push((self.mdx.words[i].clone(), (i + 1) as u32));
            } else {
                has_more = true;
                break;
            }
        }
        (items, has_more)
    }

    fn suggest_page(
        &self,
        q: &str,
        offset: usize,
        limit: usize,
        desc: bool,
    ) -> (Vec<(String, u32)>, bool) {
        let q = q.to_lowercase();
        if desc {
            self.page((0..self.mdx.words.len()).rev(), &q, offset, limit)
        } else {
            self.page(0..self.mdx.words.len(), &q, offset, limit)
        }
    }

    fn word(&self, id: u32) -> Option<&str> {
        self.mdx.word(id)
    }

    fn text(&self, id: u32) -> Option<String> {
        self.mdx.text(id)
    }
}

/// First-character jump bar. ASCII letters/digits get one bucket each; other
/// scripts (CJK, kana, ...) are sampled down so the strip stays usable.
fn build_initials(words: &[String]) -> Vec<(String, usize, usize)> {
    let mut map: HashMap<String, (usize, usize)> = HashMap::new();
    for (i, word) in words.iter().enumerate() {
        let Some(first) = word.chars().next() else {
            continue;
        };
        let label = if first.is_ascii_alphanumeric() {
            first.to_ascii_lowercase().to_string()
        } else {
            first.to_string()
        };
        map.entry(label)
            .and_modify(|(_, last)| *last = i)
            .or_insert((i, i));
    }
    let (mut ascii, mut other): (Vec<_>, Vec<_>) = map
        .into_iter()
        .map(|(label, (first, last))| (label, first, last))
        .partition(|(label, _, _)| label.chars().all(|c| c.is_ascii_alphanumeric()));
    ascii.sort_by(|a, b| a.0.cmp(&b.0));
    other.sort_by_key(|(_, first, _)| *first);
    const MAX_BUCKETS: usize = 48;
    if !other.is_empty() && ascii.len() + other.len() > MAX_BUCKETS {
        let keep = MAX_BUCKETS.saturating_sub(ascii.len()).max(8);
        let step = ((other.len() as f64) / (keep as f64)).ceil() as usize;
        other = other.into_iter().step_by(step.max(1)).collect();
    }
    ascii.extend(other);
    ascii
}

fn rand_index(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() ^ (d.subsec_nanos() as u64))
        .unwrap_or(0);
    (seed % n as u64) as usize
}

struct AppState {
    dict: Arc<Dict>,
    mdds: Vec<Mdd>,
    res: HashMap<String, Resource>,
    tx: broadcast::Sender<String>,
}

#[get("/")]
async fn index_page() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(rebase(INDEX_HTML))
}

/// The app always lives under `base`; a bare `/` only redirects there.
#[get("/")]
async fn root_redirect() -> impl Responder {
    HttpResponse::Found()
        .insert_header(("Location", format!("{}/", base())))
        .finish()
}

fn routes(cfg: &mut web::ServiceConfig) {
    cfg.service(index_page)
        .service(parser)
        .service(item)
        .service(lookup)
        .service(static_file)
        .service(res_file)
        .service(dist)
        .service(favicon)
        .service(ws);
}

fn root(cfg: &mut web::ServiceConfig) {
    if !base().is_empty() {
        cfg.service(root_redirect);
    }
}

fn render_item(title: &str, body_html: &str) -> String {
    ITEM_HTML
        .replace("{base}", base())
        .replace("{title}", title)
        .replace("{attributes}", body_html)
}

#[get("/api/parser")]
async fn parser(state: web::Data<AppState>) -> impl Responder {
    HttpResponse::Ok().json(json!({
        "encoding": state.dict.mdx.encoding,
        "count": state.dict.mdx.words.len(),
    }))
}

#[get("/api/item/{k_id}")]
async fn item(k_id: web::Path<u32>, state: web::Data<AppState>) -> impl Responder {
    let k_id = k_id.into_inner();
    match state.dict.text(k_id) {
        Some(text) => {
            let title = state.dict.word(k_id).unwrap_or("");
            HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .body(render_item(title, &text))
        }
        None => HttpResponse::NotFound().body("404"),
    }
}
#[get("/api/lookup/{word}")]
async fn lookup(word: web::Path<String>, state: web::Data<AppState>) -> impl Responder {
    let (hits, has_more) = state.dict.suggest_page(&word, 0, 20, false);
    HttpResponse::Ok().json(json!({"items": hits, "has_more": has_more}))
}

#[get("/api/static/{path:.*}")]
async fn static_file(path: web::Path<String>) -> impl Responder {
    let rel = path.into_inner();
    if rel.contains("..") {
        return HttpResponse::BadRequest().body("INVALID_PATH");
    }
    match fs::read(format!("static/{rel}")) {
        Ok(bytes) => HttpResponse::Ok().content_type(mime_of(&rel)).body(bytes),
        Err(_) => HttpResponse::NotFound().body("NOT_FOUND"),
    }
}

fn serve_resource(rel: &str, state: &AppState) -> HttpResponse {
    if rel.is_empty() || rel.contains("..") {
        return HttpResponse::NotFound().body("NOT_FOUND");
    }
    let key = norm_res(rel);
    let Some(res) = state.res.get(&key) else {
        return HttpResponse::NotFound().body("NOT_FOUND");
    };
    match state.mdds[res.mdd].read(res.start, res.end) {
        Ok(bytes) => HttpResponse::Ok()
            .content_type(mime_of(&key))
            .insert_header(("Cache-Control", "public, max-age=3600"))
            .body(bytes),
        Err(e) => HttpResponse::InternalServerError().body(format!("READ_ERROR: {e}")),
    }
}

#[get("/api/res/{path:.*}")]
async fn res_file(path: web::Path<String>, state: web::Data<AppState>) -> impl Responder {
    serve_resource(&path.into_inner(), &state)
}

/// Entries also reference resources with root-absolute paths (`/img.png`);
/// anything no other route claimed is looked up in the .mdd archives.
async fn fallback(req: HttpRequest, state: web::Data<AppState>) -> impl Responder {
    let decoded = percent_encoding::percent_decode_str(req.path()).decode_utf8_lossy();
    let mut rel = decoded.as_ref();
    if rel.ends_with("/favicon.ico") || rel == "/favicon.ico" {
        return HttpResponse::NoContent().finish();
    }
    if !base().is_empty() {
        if let Some(rest) = rel.strip_prefix(base()) {
            rel = rest;
        }
    }
    serve_resource(rel, &state)
}

#[get("/dist/{path:.*}")]
async fn dist(path: web::Path<String>) -> impl Responder {
    match path.into_inner().as_str() {
        "site.css" => HttpResponse::Ok()
            .content_type("text/css; charset=utf-8")
            .body(SITE_CSS),
        "utils.js" => HttpResponse::Ok()
            .content_type("application/javascript; charset=utf-8")
            .body(rebase(UTILS_JS)),
        _ => HttpResponse::NotFound().body("FILE_NOT_FOUND"),
    }
}

#[get("/favicon.ico")]
async fn favicon() -> impl Responder {
    HttpResponse::NoContent().finish()
}

fn mime_of(name: &str) -> &'static str {
    match name.rsplit('.').next() {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("mp4") => "video/mp4",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

#[get("/api/v1/ws/")]
async fn ws(
    req: HttpRequest,
    body: web::Payload,
    state: web::Data<AppState>,
) -> actix_web::Result<HttpResponse> {
    let (response, session, msg_stream) = actix_ws::handle(&req, body)?;
    let mut rx = state.tx.subscribe();
    let state2 = state.clone();

    let mut out = session.clone();
    actix_web::rt::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if out.text(msg).await.is_err() {
                break;
            }
        }
    });

    actix_web::rt::spawn(async move {
        let mut stream = msg_stream;
        while let Some(Ok(msg)) = stream.next().await {
            if let Message::Text(text) = msg {
                if let Ok(req) = serde_json::from_str::<Value>(&text) {
                    let id = req.get("id").cloned().unwrap_or(Value::Null);
                    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    let params = req.get("params").cloned().unwrap_or(json!([]));
                    let reply = match method {
                        "suggest_words" => {
                            let q = params.get(0).and_then(|p| p.as_str()).unwrap_or("");
                            let offset =
                                params.get(1).and_then(|p| p.as_u64()).unwrap_or(0) as usize;
                            let desc = params
                                .get(2)
                                .and_then(|p| p.as_bool())
                                .unwrap_or(false);
                            let (hits, has_more) =
                                state2.dict.suggest_page(q, offset, 20, desc);
                            let items: Vec<[Value; 2]> = hits
                                .into_iter()
                                .map(|(w, id)| [json!(w), json!(id)])
                                .collect();
                            json!({"id": id, "result": {"items": items, "has_more": has_more}, "error": null})
                        }
                        "load_text" => {
                            let k = params.get(0).and_then(|p| p.as_u64()).unwrap_or(0) as u32;
                            let text = state2.dict.text(k).unwrap_or_default();
                            json!({"id": id, "result": text, "error": null})
                        }
                        "random" => {
                            let n = state2.dict.mdx.words.len();
                            let i = rand_index(n);
                            json!({"id": id, "result": {
                                "word": state2.dict.mdx.words[i],
                                "k_id": (i + 1) as u32,
                            }, "error": null})
                        }
                        "initials" => {
                            json!({"id": id, "result": state2.dict.initials, "error": null})
                        }
                        "jump" => {
                            let label = params.get(0).and_then(|p| p.as_str()).unwrap_or("");
                            let desc = params
                                .get(1)
                                .and_then(|p| p.as_bool())
                                .unwrap_or(false);
                            let n = state2.dict.mdx.words.len();
                            let offset = state2
                                .dict
                                .initials
                                .iter()
                                .find(|(l, _, _)| l == label)
                                .map(|(_, first, last)| {
                                    if desc {
                                        n.saturating_sub(1).saturating_sub(*last)
                                    } else {
                                        *first
                                    }
                                })
                                .unwrap_or(0);
                            json!({"id": id, "result": {"offset": offset}, "error": null})
                        }
                        "redirect" => {
                            let k = params.get(0).cloned().unwrap_or(Value::Null);
                            let _ = state2
                                .tx
                                .send(json!({"event": "redirect", "params": [k]}).to_string());
                            json!({"id": id, "result": null, "error": null})
                        }
                        _ => json!({"id": id, "result": null, "error": "unknown method"}),
                    };
                    if session.clone().text(reply.to_string()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    Ok(response)
}

fn usage() -> ! {
    eprintln!(
        "prev — single-file MDict (.mdx) viewer\n\n\
         usage: prev <dictionary.mdx> [--port N] [--base PATH]\n\n\
           --port N      listen port on 127.0.0.1 (default 3030)\n\
           --base PATH   public mount prefix (default /mdx-preview)\n\n\
         env: PREVIEW_BASE overrides --base"
    );
    std::process::exit(2);
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let mut file: Option<String> = None;
    let mut port: u16 = 3030;
    let mut base = std::env::var("PREVIEW_BASE").unwrap_or_else(|_| "/mdx-preview".into());
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" | "-p" => {
                port = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            "--base" | "-b" => base = args.next().unwrap_or_else(|| usage()),
            "--help" | "-h" => usage(),
            _ => file = Some(a),
        }
    }
    let file = file.unwrap_or_else(|| usage());
    let base = normalize_base(&base);

    let mdx = match load_mdx(&file) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("prev: cannot read {file}: {e}");
            std::process::exit(1);
        }
    };
    if mdx.words.is_empty() {
        eprintln!("prev: no entries in {file}");
        std::process::exit(1);
    }
    let count = mdx.words.len();
    let lower: Vec<String> = mdx.words.iter().map(|w| w.to_lowercase()).collect();
    let initials = build_initials(&mdx.words);
    let _ = BASE.set(base.clone());

    let dict = Arc::new(Dict {
        mdx,
        lower,
        initials,
    });
    let (mdds, res) = load_resources(&file);
    let (tx, _rx) = broadcast::channel(32);
    let state = web::Data::new(AppState {
        dict,
        mdds,
        res,
        tx,
    });

    println!("prev: {count} headwords from {file}");
    if !state.res.is_empty() {
        println!(
            "prev: {} resources from {} mdd file(s)",
            state.res.len(),
            state.mdds.len()
        );
    }
    println!("listening on http://127.0.0.1:{port}{base}/");
    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .configure(root)
            .service(web::scope(base.as_str()).configure(routes))
            .default_service(web::to(fallback))
    })
    .bind(("127.0.0.1", port))?
    .run()
    .await
}
