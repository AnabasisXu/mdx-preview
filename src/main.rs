//! prev — single-file MDict (.mdx) viewer.
//!
//! `prev dictionary.mdx` serves the dictionary's headwords and entries over
//! HTTP so they can be read in a browser (or behind a reverse proxy).
//!
//! Everything is in this one binary: the MDX v2 reader, the embedded web UI,
//! and the HTTP/WebSocket server.

use actix_web::http::StatusCode;
use actix_web::{get, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use actix_ws::Message;
use encoding_rs::Encoding;
use flate2::read::ZlibDecoder;
use futures_util::StreamExt;
use memmap2::Mmap;
use ripemd::{Digest, Ripemd128};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
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
fn truncated() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "truncated container")
}
fn need(d: &[u8], at: usize, n: usize) -> io::Result<()> {
    match at.checked_add(n) {
        Some(end) if end <= d.len() => Ok(()),
        _ => Err(truncated()),
    }
}
fn be_u16_at(d: &[u8], i: usize) -> io::Result<usize> {
    need(d, i, 2)?;
    Ok(be_u16(d, i) as usize)
}
fn le_u32_at(d: &[u8], i: usize) -> io::Result<u32> {
    need(d, i, 4)?;
    Ok(le_u32(d, i))
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
fn read_num(d: &[u8], p: &mut usize, width: usize) -> io::Result<u64> {
    let end = p
        .checked_add(width)
        .filter(|e| *e <= d.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated container"))?;
    let v = if width == 8 {
        be_u64(d, *p)
    } else {
        be_u32(d, *p) as u64
    };
    *p = end;
    Ok(v)
}

struct RecordBlock {
    offset: usize,
    comp: usize,
    decomp: usize,
}

/// Decompressed key section: headwords, lookup order, record offsets, and the
/// lowercased forms used for filtering.
struct KeyIndex {
    words: Vec<String>,
    /// Entry ids sorted by headword, for `@@@LINK=` lookups.
    sorted: Vec<u32>,
    starts: Vec<u64>,
    ends: Vec<u64>,
    /// Lowercased headwords, computed once so filtering never allocates per word.
    lower: Vec<String>,
}

impl KeyIndex {
    fn find(&self, word: &str) -> Option<usize> {
        self.sorted
            .binary_search_by(|id| self.words[*id as usize].as_str().cmp(word))
            .ok()
            .map(|pos| self.sorted[pos] as usize)
    }
}

/// Where the key section lives in the mapped file.
struct MdxLayout {
    width: usize,
    version2: bool,
    is_utf16: bool,
    /// `Encrypted=2`: the key block info is obfuscated.
    encrypt2: bool,
    kb_info_span: (usize, usize),
    kb_comp_span: (usize, usize),
}

/// Memory-mapped .mdx. Opening parses the header and the record-block table
/// only; the key section is decompressed on first use so a directory of
/// dictionaries stays cheap until one is actually browsed.
struct Mdx {
    map: Mmap,
    codec: Option<&'static Encoding>,
    num_entries: u64,
    layout: MdxLayout,
    /// Sum of the decompressed record sizes, the last entry's end offset.
    record_total: u64,
    blocks: Vec<RecordBlock>,
    index: OnceLock<Result<KeyIndex, String>>,
}

impl Mdx {
    fn decompress(&self, b: &RecordBlock) -> io::Result<Vec<u8>> {
        let end = b
            .offset
            .checked_add(b.comp)
            .filter(|e| *e <= self.map.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "record block outside file"))?;
        let buf = &self.map[b.offset..end];
        if buf.len() < 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated record block",
            ));
        }
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

    /// Decompress the key section on first use; failures are cached too.
    fn index(&self) -> Result<&KeyIndex, String> {
        self.index
            .get_or_init(|| self.build_index().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| e.clone())
    }

    fn build_index(&self) -> io::Result<KeyIndex> {
        let d: &[u8] = &self.map;
        let width = self.layout.width;
        let (info_off, info_len) = self.layout.kb_info_span;
        let kb_info_raw = &d[info_off..info_off + info_len];
        let kb_info = if self.layout.version2 {
            let block = if self.layout.encrypt2 {
                mdx_decrypt(kb_info_raw)
            } else {
                kb_info_raw.to_vec()
            };
            if block.len() < 8 {
                return Err(truncated());
            }
            unzlib(&block[8..])?
        } else {
            kb_info_raw.to_vec()
        };

        let mut comps: Vec<usize> = Vec::new();
        let mut i = 0;
        while i < kb_info.len() {
            let _n = read_num(&kb_info, &mut i, width)?;
            let head_size = if self.layout.version2 {
                let s = be_u16_at(&kb_info, i)?;
                i += 2;
                s
            } else {
                let s = *kb_info.get(i).ok_or_else(truncated)? as usize;
                i += 1;
                s
            };
            i += if self.layout.is_utf16 {
                (head_size + 1) * 2
            } else {
                head_size + 1
            };
            let tail_size = if self.layout.version2 {
                let s = be_u16_at(&kb_info, i)?;
                i += 2;
                s
            } else {
                let s = *kb_info.get(i).ok_or_else(truncated)? as usize;
                i += 1;
                s
            };
            i += if self.layout.is_utf16 {
                (tail_size + 1) * 2
            } else {
                tail_size + 1
            };
            let comp = read_num(&kb_info, &mut i, width)? as usize;
            let _decomp = read_num(&kb_info, &mut i, width)?;
            comps.push(comp);
        }

        let (kb_off, kb_len) = self.layout.kb_comp_span;
        let kb_comp = &d[kb_off..kb_off + kb_len];

        let mut keys: Vec<(u64, String)> = Vec::new();
        let mut off = 0;
        for comp in comps {
            let kind = le_u32_at(kb_comp, off)?;
            let end = off
                .checked_add(comp)
                .filter(|e| *e <= kb_comp.len())
                .ok_or_else(truncated)?;
            if end < off + 8 {
                return Err(truncated());
            }
            let body = &kb_comp[off + 8..end];
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
                let key_id = read_num(&block, &mut j, width)?;
                let start = j;
                let word;
                if self.layout.is_utf16 {
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
                    word = decode(&block[start..k], self.codec);
                    j = k + 1;
                }
                keys.push((key_id, word));
            }
            off += comp;
        }

        let mut starts = Vec::with_capacity(keys.len());
        let mut ends = Vec::with_capacity(keys.len());
        for idx in 0..keys.len() {
            starts.push(keys[idx].0);
            ends.push(if idx + 1 < keys.len() {
                keys[idx + 1].0
            } else {
                self.record_total
            });
        }
        let words: Vec<String> = keys.into_iter().map(|(_, word)| word).collect();
        let mut sorted: Vec<u32> = (0..words.len() as u32).collect();
        sorted.sort_by(|a, b| words[*a as usize].cmp(&words[*b as usize]));
        let lower: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
        Ok(KeyIndex {
            words,
            sorted,
            starts,
            ends,
            lower,
        })
    }

    fn text_at(&self, idx: usize, index: &KeyIndex) -> Result<Option<String>, String> {
        let (Some(start), Some(end)) = (index.starts.get(idx), index.ends.get(idx)) else {
            return Ok(None);
        };
        let raw = self
            .raw(*start as usize, *end as usize)
            .map_err(|e| format!("entry {idx}: {e}"))?;
        let text = decode(&raw, self.codec);
        Ok(Some(text.trim_end_matches('\0').to_string()))
    }

    /// Entry text with `@@@LINK=` redirects followed (bounded, cycle-safe).
    fn text(&self, id: u32) -> Result<Option<String>, String> {
        let Some(idx) = (id as usize).checked_sub(1) else {
            return Ok(None);
        };
        let index = self.index()?;
        let mut cur = idx;
        for _ in 0..16 {
            let Some(text) = self.text_at(cur, index)? else {
                return Ok(None);
            };
            let Some(target) = text.trim().strip_prefix("@@@LINK=") else {
                return Ok(Some(text));
            };
            let target = target.trim();
            match index.find(target) {
                Some(next) if next != cur => cur = next,
                _ => return Ok(Some(text)),
            }
        }
        self.text_at(idx, index)
    }

    fn word(&self, id: u32) -> Result<Option<&str>, String> {
        let index = self.index()?;
        Ok((id as usize)
            .checked_sub(1)
            .and_then(|i| index.words.get(i))
            .map(String::as_str))
    }
}

/// Open an .mdx container: header and record-block table only. The key section
/// is left compressed until `index()` is called.
fn open(path: &Path) -> io::Result<Mdx> {
    let file = File::open(path)?;
    let map = unsafe { Mmap::map(&file)? };
    let d: &[u8] = &map;
    if d.len() < 8 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file too small"));
    }
    let hlen = be_u32(&d, 0) as usize;
    if hlen < 2 || 4 + hlen > d.len() {
        return Err(truncated());
    }
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
    need(&d, p, 4 * width + 4)?;
    let _num_key_blocks = read_num(&d, &mut p, width)?;
    let num_entries = read_num(&d, &mut p, width)?;
    if version >= 2.0 {
        let _kb_info_decomp = read_num(&d, &mut p, width)?;
    }
    let kb_info_size = read_num(&d, &mut p, width)? as usize;
    let kb_size = read_num(&d, &mut p, width)? as usize;
    if version >= 2.0 {
        p += 4; // adler32 of the five numbers
    }
    let _key_end = p
        .checked_add(kb_info_size)
        .and_then(|v| v.checked_add(kb_size))
        .filter(|e| *e <= d.len())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "key section lies outside the file")
        })?;
    let kb_info_span = (p, kb_info_size);
    p += kb_info_size;
    let kb_comp_span = (p, kb_size);
    p += kb_size;

    let is_utf16 = encoding.eq_ignore_ascii_case("UTF-16");
    let codec = if is_utf16 {
        None
    } else {
        Encoding::for_label(encoding.as_bytes())
    };

    // --- record section
    need(&d, p, 4 * width)?;
    let num_record_blocks = read_num(&d, &mut p, width)?;
    let _num_entries2 = read_num(&d, &mut p, width)?;
    let _rb_info_size = read_num(&d, &mut p, width)?;
    let _rb_size = read_num(&d, &mut p, width)?;
    // The block directory sits between here and the first record block.
    let data_start = num_record_blocks
        .checked_mul(2 * width as u64)
        .and_then(|n| usize::try_from(n).ok())
        .and_then(|n| p.checked_add(n))
        .filter(|e| *e <= d.len())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "record block table lies outside the file",
            )
        })?;
    let mut info: Vec<(usize, usize)> = Vec::new();
    for _ in 0..num_record_blocks {
        let comp = read_num(&d, &mut p, width)? as usize;
        let decomp = read_num(&d, &mut p, width)? as usize;
        info.push((comp, decomp));
    }

    let mut blocks = Vec::with_capacity(info.len());
    let mut offset = data_start;
    for (comp, decomp) in info {
        blocks.push(RecordBlock {
            offset,
            comp,
            decomp,
        });
        offset += comp;
    }
    let total: u64 = blocks.iter().map(|b| b.decomp as u64).sum();

    Ok(Mdx {
        map,
        codec,
        num_entries,
        layout: MdxLayout {
            width,
            version2: version >= 2.0,
            is_utf16,
            encrypt2: encrypt & 2 != 0,
            kb_info_span,
            kb_comp_span,
        },
        record_total: total,
        blocks,
        index: OnceLock::new(),
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
        if buf.len() < 8 {
            return Err(truncated());
        }
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
        if kb_info.len() < 8 {
            return Err(truncated());
        }
        kb_info = unzlib(&kb_info[8..])?;
    }

    let mut sizes: Vec<(usize, usize)> = Vec::with_capacity(num_key_blocks as usize);
    let mut i = 0;
    while i < kb_info.len() {
        read_num(&kb_info, &mut i, width)?; // entries in this key block
        let hs = if version >= 2.0 {
            let s = be_u16_at(&kb_info, i)?;
            i += 2;
            s
        } else {
            let s = *kb_info.get(i).ok_or_else(truncated)? as usize;
            i += 1;
            s
        };
        i += (hs + term) * 2;
        let ts = if version >= 2.0 {
            let s = be_u16_at(&kb_info, i)?;
            i += 2;
            s
        } else {
            let s = *kb_info.get(i).ok_or_else(truncated)? as usize;
            i += 1;
            s
        };
        i += (ts + term) * 2;
        let comp = read_num(&kb_info, &mut i, width)? as usize;
        let decomp = read_num(&kb_info, &mut i, width)? as usize;
        sizes.push((comp, decomp));
    }

    let kb = read_exact_at(&mut f, kb_size)?;
    let mut keys: Vec<(usize, String)> = Vec::new();
    let mut off = 0;
    for (comp, _) in &sizes {
        let kind = le_u32_at(&kb, off)?;
        let end = off
            .checked_add(*comp)
            .filter(|e| *e <= kb.len())
            .ok_or_else(truncated)?;
        if end < off + 8 {
            return Err(truncated());
        }
        let body = &kb[off + 8..end];
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
            let key_id = read_num(&block, &mut j, width)? as usize;
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
fn mdd_candidates(p: &Path) -> Vec<PathBuf> {
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

/// A dictionary's .mdd archives, indexed on first resource request.
struct Resources {
    mdds: Vec<Mdd>,
    map: HashMap<String, Resource>,
}

fn load_resources(mdx: &Path) -> Resources {
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
    Resources { mdds, map }
}

// ---------------------------------------------------------------- web layer

struct Dict {
    id: usize,
    /// File name without extension, shown in the picker and status bar.
    name: String,
    path: PathBuf,
    mdx: Mdx,
    /// .mdd archives, indexed on first resource request.
    resources: OnceLock<Resources>,
}

impl Dict {
    fn new(id: usize, path: PathBuf, mdx: Mdx) -> Dict {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        Dict {
            id,
            name,
            path,
            mdx,
            resources: OnceLock::new(),
        }
    }

    /// Header-declared entry count; once the key index exists, the real count.
    fn count(&self) -> usize {
        match self.mdx.index.get() {
            Some(Ok(index)) => index.words.len(),
            _ => self.mdx.num_entries as usize,
        }
    }

    fn index(&self) -> Result<&KeyIndex, String> {
        self.mdx.index()
    }

    fn page(
        &self,
        index: &KeyIndex,
        idxs: impl Iterator<Item = usize>,
        q: &str,
        offset: usize,
        limit: usize,
    ) -> (Vec<(String, u32)>, bool) {
        let mut items = Vec::with_capacity(limit);
        let mut skipped = 0;
        let mut has_more = false;
        for i in idxs {
            if !q.is_empty() && !index.lower[i].contains(q) {
                continue;
            }
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if items.len() < limit {
                items.push((index.words[i].clone(), (i + 1) as u32));
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
    ) -> Result<(Vec<(String, u32)>, bool), String> {
        let index = self.index()?;
        let q = q.to_lowercase();
        let (items, has_more) = if desc {
            self.page(index, (0..index.words.len()).rev(), &q, offset, limit)
        } else {
            self.page(index, 0..index.words.len(), &q, offset, limit)
        };
        Ok((items, has_more))
    }

    fn word(&self, id: u32) -> Result<Option<&str>, String> {
        self.mdx.word(id)
    }

    fn text(&self, id: u32) -> Result<Option<String>, String> {
        self.mdx.text(id)
    }

    /// Jump bar anchors at exactly `requested` evenly spaced positions: the
    /// first is entry 0, the last is the final entry, the rest are equidistant.
    fn anchors(&self, requested: usize) -> Result<Vec<(String, usize)>, String> {
        let words = &self.index()?.words;
        let n = words.len();
        if n == 0 {
            return Ok(Vec::new());
        }
        let count = requested.clamp(2, 64).min(n);
        let mut out = Vec::with_capacity(count);
        for at in anchor_positions(n, count) {
            let label = words[at]
                .chars()
                .next()
                .map(String::from)
                .unwrap_or_default();
            out.push((label, at));
        }
        Ok(out)
    }

    fn resources(&self) -> &Resources {
        self.resources.get_or_init(|| load_resources(&self.path))
    }
}

/// Jump bar anchors: exactly `count` positions through the headword list, first
/// at index 0 and last at the final entry.
fn anchor_positions(n: usize, count: usize) -> impl Iterator<Item = usize> {
    let count = count.max(1);
    (0..count).map(move |i| {
        if count <= 1 {
            0
        } else {
            i * (n - 1) / (count - 1)
        }
    })
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
    dicts: Vec<Dict>,
    tx: broadcast::Sender<String>,
}

#[get("/")]
async fn index_page() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .insert_header(("Cache-Control", "no-store"))
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

fn render_item(dict: &Dict, title: &str, body_html: &str) -> String {
    let res_base = format!("{}/api/d/{}/res/", base(), dict.id);
    let item_base = format!("{}/api/d/{}/item/", base(), dict.id);
    let body = rewrite_links(dict, &item_base, &res_base, body_html);
    ITEM_HTML
        .replace("{res}", &res_base)
        .replace("{title}", title)
        .replace("{attributes}", &body)
}

/// Rewrite MDict pseudo-schemes in entry HTML: `entry://word` becomes a link to
/// the target entry (when the headword exists), `sound://file` a resource link.
fn rewrite_links(dict: &Dict, item_base: &str, res_base: &str, body: &str) -> String {
    rewrite_links_with(item_base, res_base, body, |word| entry_id(dict, word))
}

/// `resolve` maps a decoded headword to its id; split out so the rewriter can be
/// tested without a container.
fn rewrite_links_with<F>(
    item_base: &str,
    res_base: &str,
    body: &str,
    resolve: F,
) -> String
where
    F: Fn(&str) -> Option<u32>,
{
    let mut out = String::with_capacity(body.len());
    let mut copied = 0usize;
    let mut search = 0usize;
    while let Some(pos) = next_pseudo_link(body, search) {
        let entry = body[pos..].starts_with("entry://");
        let Some(quote_pos) = body[..pos].rfind(['"', '\'']) else {
            break;
        };
        let quote = body.as_bytes()[quote_pos] as char;
        let attr = attr_name_before(body, quote_pos);
        let allowed = if entry {
            attr.eq_ignore_ascii_case("href")
        } else {
            attr.eq_ignore_ascii_case("src") || attr.eq_ignore_ascii_case("href")
        };
        // Only a value that *is* the scheme is rewritten; "…?u=entry://x" is
        // not a link and rewriting it would corrupt the attribute.
        if !allowed || !body[..quote_pos].trim_end().ends_with('=') {
            search = pos + 1;
            continue;
        }
        let Some(end_rel) = body[pos..].find(quote) else {
            break;
        };
        let end = pos + end_rel;
        let value = &body[pos..end];
        let scheme = if entry { "entry://" } else { "sound://" };
        // The value must be exactly the scheme, starting right after the quote;
        // "?u=entry://x" is not a link.
        if pos != quote_pos + 1 || !value.starts_with(scheme) {
            search = end + 1;
            continue;
        }
        let target = if entry {
            let word = &value[scheme.len()..];
            let word = word.split('#').next().unwrap_or(word);
            let word = percent_encoding::percent_decode_str(word).decode_utf8_lossy();
            resolve(word.trim()).map(|id| format!("{item_base}{id}"))
        } else {
            Some(format!("{res_base}{}", encode_res_path(&value[scheme.len()..])))
        };
        if let Some(url) = target {
            out.push_str(&body[copied..quote_pos + 1]);
            out.push_str(&url);
            copied = end;
        }
        search = end + 1;
    }
    out.push_str(&body[copied..]);
    out
}

/// Attribute name immediately before the opening quote of a value.
fn attr_name_before(body: &str, quote_pos: usize) -> &str {
    let mut at = quote_pos;
    let bytes = body.as_bytes();
    while at > 0 && bytes[at - 1].is_ascii_whitespace() {
        at -= 1;
    }
    if at > 0 && bytes[at - 1] == b'=' {
        at -= 1;
    }
    while at > 0 && bytes[at - 1].is_ascii_whitespace() {
        at -= 1;
    }
    let end = at;
    while at > 0
        && (bytes[at - 1].is_ascii_alphanumeric()
            || bytes[at - 1] == b'-'
            || bytes[at - 1] == b'_'
            || bytes[at - 1] == b':')
    {
        at -= 1;
    }
    &body[at..end]
}

fn encode_res_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '%' => out.push_str("%25"),
            _ => out.push(c),
        }
    }
    out
}

fn next_pseudo_link(body: &str, from: usize) -> Option<usize> {
    let hay = &body[from..];
    let a = hay.find("entry://").map(|i| from + i);
    let b = hay.find("sound://").map(|i| from + i);
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

fn entry_id(dict: &Dict, word: &str) -> Option<u32> {
    if word.is_empty() {
        return None;
    }
    let index = dict.index().ok()?;
    index.find(word).map(|idx| idx as u32 + 1)
}

fn unknown_dict() -> HttpResponse {
    HttpResponse::NotFound().body("unknown dictionary")
}

fn index_error(e: String) -> HttpResponse {
    HttpResponse::InternalServerError().body(format!("INDEX_ERROR: {e}"))
}

#[get("/api/parser")]
async fn parser(state: web::Data<AppState>) -> impl Responder {
    let dicts: Vec<Value> = state
        .dicts
        .iter()
        .map(|d| {
            json!({
                "id": d.id,
                "name": d.name,
                "count": d.count(),
                "path": d.path.to_string_lossy(),
            })
        })
        .collect();
    HttpResponse::Ok().json(json!({"dicts": dicts}))
}

#[get("/api/d/{dict}/item/{k_id}")]
async fn item(path: web::Path<(usize, u32)>, state: web::Data<AppState>) -> impl Responder {
    let (d, k_id) = path.into_inner();
    let Some(dict) = state.dicts.get(d) else {
        return unknown_dict();
    };
    match dict.text(k_id) {
        Ok(Some(text)) => {
            let title = dict.word(k_id).ok().flatten().unwrap_or("");
            HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .insert_header(("Cache-Control", "no-store"))
                .body(render_item(dict, title, &text))
        }
        Ok(None) => HttpResponse::NotFound().body("404"),
        Err(e) => index_error(e),
    }
}

#[get("/api/d/{dict}/lookup/{word}")]
async fn lookup(path: web::Path<(usize, String)>, state: web::Data<AppState>) -> impl Responder {
    let (d, word) = path.into_inner();
    let Some(dict) = state.dicts.get(d) else {
        return unknown_dict();
    };
    match dict.suggest_page(&word, 0, 20, false) {
        Ok((hits, has_more)) => HttpResponse::Ok().json(json!({"items": hits, "has_more": has_more})),
        Err(e) => index_error(e),
    }
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

fn serve_resource(dict: &Dict, rel: &str) -> HttpResponse {
    if rel.is_empty() || rel.contains("..") {
        return HttpResponse::NotFound().body("NOT_FOUND");
    }
    // A real file next to the dictionary shadows the .mdd copy, so stylesheets
    // can be edited in place; it is never cached because it changes on save.
    if let Some(resp) = serve_disk_resource(dict, rel) {
        return resp;
    }
    let key = norm_res(rel);
    let resources = dict.resources();
    let Some(res) = resources.map.get(&key) else {
        return HttpResponse::NotFound().body("NOT_FOUND");
    };
    match resources.mdds[res.mdd].read(res.start, res.end) {
        Ok(bytes) => HttpResponse::Ok()
            .content_type(mime_of(&key))
            .insert_header(("Cache-Control", "public, max-age=3600"))
            .body(bytes),
        Err(e) => HttpResponse::InternalServerError().body(format!("READ_ERROR: {e}")),
    }
}

/// Only CSS/JS are taken from disk; anything else keeps coming from the .mdd.
fn serve_disk_resource(dict: &Dict, rel: &str) -> Option<HttpResponse> {
    let ext = Path::new(rel).extension()?.to_str()?.to_ascii_lowercase();
    if ext != "css" && ext != "js" {
        return None;
    }
    let dir = dict.path.parent()?;
    let root = fs::canonicalize(dir).ok()?;
    let file = fs::canonicalize(dir.join(rel)).ok()?;
    if !file.starts_with(&root) {
        return None;
    }
    let bytes = fs::read(&file).ok()?;
    Some(
        HttpResponse::Ok()
            .content_type(mime_of(rel))
            .insert_header(("Cache-Control", "no-store"))
            .body(bytes),
    )
}

#[get("/api/d/{dict}/res/{path:.*}")]
async fn res_file(path: web::Path<(usize, String)>, state: web::Data<AppState>) -> impl Responder {
    let (d, rel) = path.into_inner();
    let Some(dict) = state.dicts.get(d) else {
        return unknown_dict();
    };
    serve_resource(dict, &rel)
}

/// Entries also reference resources with root-absolute paths (`/img.png`),
/// which carry no dictionary. Try the referring entry's dictionary first, then
/// every dictionary in load order.
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
    if rel.is_empty() || rel.contains("..") {
        return HttpResponse::NotFound().body("NOT_FOUND");
    }
    let referer = req
        .headers()
        .get("referer")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if let Some(dict) = dict_from_referer(referer).and_then(|d| state.dicts.get(d)) {
        let resp = serve_resource(dict, rel);
        if resp.status() != StatusCode::NOT_FOUND {
            return resp;
        }
    }
    for dict in &state.dicts {
        let resp = serve_resource(dict, rel);
        if resp.status() != StatusCode::NOT_FOUND {
            return resp;
        }
    }
    HttpResponse::NotFound().body("NOT_FOUND")
}

/// Pull the dictionary id out of `/api/d/<n>/...` in a Referer URL.
fn dict_from_referer(referer: &str) -> Option<usize> {
    let at = referer.find("/api/d/")? + "/api/d/".len();
    let digits: String = referer[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

#[get("/dist/{path:.*}")]
async fn dist(path: web::Path<String>) -> impl Responder {
    match path.into_inner().as_str() {
        "site.css" => HttpResponse::Ok()
            .content_type("text/css; charset=utf-8")
            .insert_header(("Cache-Control", "no-store"))
            .body(SITE_CSS),
        "utils.js" => HttpResponse::Ok()
            .content_type("application/javascript; charset=utf-8")
            .insert_header(("Cache-Control", "no-store"))
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
    // WebSockets bypass CORS, so without this any website could read the whole
    // dictionary through this endpoint.
    if !origin_allowed(&req) {
        return Ok(HttpResponse::Forbidden().body("cross-origin websocket rejected"));
    }
    let (response, session, msg_stream) = actix_ws::handle(&req, body)?;
    let mut rx = state.tx.subscribe();
    let state2 = state.clone();
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();

    // Forward broadcasts until the peer goes away; without the stop signal this
    // task would park on the broadcast forever and hold the connection open, so
    // the server could not shut down gracefully while a tab was still attached.
    let mut out = session.clone();
    actix_web::rt::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                msg = rx.recv() => match msg {
                    Ok(msg) => {
                        if out.text(msg).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
            }
        }
    });

    actix_web::rt::spawn(async move {
        let mut stream = msg_stream;
        while let Some(Ok(msg)) = stream.next().await {
            if let Message::Close(_) = msg {
                break;
            }
            if let Message::Text(text) = msg {
                if let Ok(req) = serde_json::from_str::<Value>(&text) {
                    let id = req.get("id").cloned().unwrap_or(Value::Null);
                    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    let params = req.get("params").cloned().unwrap_or(json!([]));
                    let dict = req
                        .get("dict")
                        .and_then(|v| v.as_u64())
                        .and_then(|v| state2.dicts.get(v as usize));
                    let reply = match dict {
                        Some(dict) => match rpc_reply(&state2, dict, id.clone(), method, &params) {
                            Ok(v) => v,
                            Err(e) => json!({"id": id, "result": null, "error": e}),
                        },
                        None => json!({"id": id, "result": null, "error": "unknown dictionary"}),
                    };
                    if session.clone().text(reply.to_string()).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = stop_tx.send(());
    });
    Ok(response)
}

/// Browsers send an Origin header; accept only the page served by this same
/// host (requests without Origin, e.g. test clients, are allowed).
fn origin_allowed(req: &HttpRequest) -> bool {
    let Some(origin) = req
        .headers()
        .get(actix_web::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return true;
    };
    let host = req
        .headers()
        .get(actix_web::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let authority = origin
        .split("://")
        .nth(1)
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("");
    !host.is_empty() && authority.eq_ignore_ascii_case(host)
}

/// One WebSocket request; errors become the reply's `error` field.
fn rpc_reply(
    state: &AppState,
    dict: &Dict,
    id: Value,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    let ok = |result: Value| json!({"id": id.clone(), "result": result, "error": null});
    match method {
        "suggest_words" => {
            let q = params.get(0).and_then(|p| p.as_str()).unwrap_or("");
            let offset = params.get(1).and_then(|p| p.as_u64()).unwrap_or(0) as usize;
            let desc = params.get(2).and_then(|p| p.as_bool()).unwrap_or(false);
            let (hits, has_more) = dict.suggest_page(q, offset, 20, desc)?;
            let items: Vec<[Value; 2]> = hits
                .into_iter()
                .map(|(w, k)| [json!(w), json!(k)])
                .collect();
            Ok(ok(json!({"items": items, "has_more": has_more})))
        }
        "load_text" => {
            let k = params.get(0).and_then(|p| p.as_u64()).unwrap_or(0) as u32;
            Ok(ok(json!(dict.text(k)?.unwrap_or_default())))
        }
        "random" => {
            let index = dict.index()?;
            let i = rand_index(index.words.len());
            Ok(ok(json!({
                "word": index.words[i],
                "k_id": (i + 1) as u32,
            })))
        }
        "initials" => {
            let count = params.get(0).and_then(|p| p.as_u64()).unwrap_or(64) as usize;
            Ok(ok(json!(dict.anchors(count)?)))
        }
        "jump" => {
            let at = params.get(0).and_then(|p| p.as_u64()).unwrap_or(0) as usize;
            let desc = params.get(1).and_then(|p| p.as_bool()).unwrap_or(false);
            let n = dict.index()?.words.len();
            let at = at.min(n.saturating_sub(1));
            let offset = if desc {
                n.saturating_sub(1).saturating_sub(at)
            } else {
                at
            };
            Ok(ok(json!({"offset": offset})))
        }
        "redirect" => {
            let k = params.get(0).cloned().unwrap_or(Value::Null);
            let _ = state.tx.send(
                json!({"event": "redirect", "dict": dict.id, "params": [k]}).to_string(),
            );
            Ok(ok(Value::Null))
        }
        _ => Ok(json!({"id": id, "result": null, "error": "unknown method"})),
    }
}

fn help() -> ! {
    println!(
        "prev {} — single-file MDict (.mdx) viewer\n\
         \n\
         usage: prev <path>... [--port N] [--base PATH]\n\
         \n\
         \x20 <path>        .mdx file, or a directory searched for *.mdx\n\
         \x20 --port N      listen port on 127.0.0.1 (default 3030)\n\
         \x20 --base PATH   public mount prefix (default /mdx-preview)\n\
         \x20 -h, --help    show this help\n\
         \n\
         env: PREVIEW_BASE overrides --base",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(0);
}

fn usage() -> ! {
    eprintln!("prev: invalid arguments; try --help");
    std::process::exit(2);
}

fn is_mdx(path: &Path) -> bool {
    path.extension()
        .map(|e| e.eq_ignore_ascii_case("mdx"))
        .unwrap_or(false)
}

fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    children.sort();
    for child in children {
        let Ok(meta) = fs::symlink_metadata(&child) else {
            continue;
        };
        // Symlinked directories can loop; direct symlinked files still work.
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            walk_dir(&child, out);
        } else if is_mdx(&child) {
            out.push(child);
        }
    }
}

/// Expand files and directories into one deterministic, deduplicated list.
fn discover(inputs: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for raw in inputs {
        let path = PathBuf::from(raw);
        if !path.exists() {
            eprintln!("prev: no such file or directory: {raw}");
            std::process::exit(1);
        }
        if path.is_dir() {
            walk_dir(&path, &mut out);
        } else if is_mdx(&path) {
            out.push(path);
        } else {
            eprintln!("prev: not an .mdx file: {raw}");
            std::process::exit(1);
        }
    }
    out.sort();
    let mut seen = HashSet::new();
    out.retain(|p| seen.insert(fs::canonicalize(p).unwrap_or_else(|_| p.clone())));
    out
}

/// Watch each dictionary directory (recursively, so nested `css/` and `js/`
/// overrides reload too) and fan a reload event out to every client viewing
/// that dictionary.
fn spawn_watcher(dirs: Vec<(PathBuf, Vec<usize>)>, tx: broadcast::Sender<String>) {
    std::thread::spawn(move || {
        use notify::{RecursiveMode, Watcher};
        let (raw_tx, raw_rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |res| {
            let _ = raw_tx.send(res);
        }) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("prev: file watch disabled: {e}");
                return;
            }
        };
        let mut seen = HashSet::new();
        for (dir, _) in &dirs {
            if seen.insert(dir.clone()) {
                if let Err(e) = watcher.watch(dir, RecursiveMode::Recursive) {
                    eprintln!("prev: cannot watch {}: {e}", dir.display());
                }
            }
        }
        for res in raw_rx {
            let Ok(event) = res else {
                continue;
            };
            let edited = event.paths.iter().any(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("css") || e.eq_ignore_ascii_case("js"))
                    .unwrap_or(false)
            });
            if !edited {
                continue;
            }
            for (dir, ids) in &dirs {
                if event.paths.iter().any(|p| p.starts_with(dir)) {
                    for id in ids {
                        let _ = tx.send(json!({"event": "reload", "dict": id}).to_string());
                    }
                }
            }
        }
    });
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let mut inputs: Vec<String> = Vec::new();
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
            "--help" | "-h" => help(),
            _ => inputs.push(a),
        }
    }
    if inputs.is_empty() {
        usage();
    }
    let base = normalize_base(&base);
    let files = discover(&inputs);
    if files.is_empty() {
        eprintln!("prev: no .mdx files found");
        std::process::exit(1);
    }

    let mut dicts: Vec<Dict> = Vec::new();
    for path in files {
        match open(&path) {
            Ok(mdx) => {
                let id = dicts.len();
                dicts.push(Dict::new(id, path, mdx));
            }
            Err(e) => eprintln!("prev: skipping {}: {e}", path.display()),
        }
    }
    if dicts.is_empty() {
        eprintln!("prev: no readable dictionaries");
        std::process::exit(1);
    }
    if dicts.iter().all(|d| d.count() == 0) {
        eprintln!("prev: no entries in {}", inputs.join(", "));
        std::process::exit(1);
    }
    let _ = BASE.set(base.clone());

    let total: usize = dicts.iter().map(|d| d.count()).sum();
    if dicts.len() == 1 {
        println!(
            "prev: {} headwords from {}",
            dicts[0].count(),
            dicts[0].path.display()
        );
    } else {
        println!("prev: {} dictionaries, {total} entries", dicts.len());
    }

    let (tx, _rx) = broadcast::channel(32);
    let mut watch: Vec<(PathBuf, Vec<usize>)> = Vec::new();
    for dict in &dicts {
        let dir = dict
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        // Event paths arrive absolute (and canonicalized on Windows), so watch
        // and compare the canonical directory.
        let dir = fs::canonicalize(&dir).unwrap_or(dir);
        match watch.iter_mut().find(|(p, _)| *p == dir) {
            Some((_, ids)) => ids.push(dict.id),
            None => watch.push((dir, vec![dict.id])),
        }
    }
    spawn_watcher(watch, tx.clone());
    let state = web::Data::new(AppState { dicts, tx });
    let listening = format!("listening on http://127.0.0.1:{port}{base}/");
    let server = HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .configure(root)
            .service(web::scope(base.as_str()).configure(routes))
            .default_service(web::to(fallback))
    })
    .bind(("127.0.0.1", port));
    let server = match server {
        Ok(server) => server,
        Err(e) => {
            eprintln!("prev: cannot listen on 127.0.0.1:{port}: {e}");
            eprintln!("prev: another instance is probably using that port; try --port N");
            std::process::exit(1);
        }
    };
    println!("{listening}");
    server.run().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rw(body: &str) -> String {
        rewrite_links_with("/i/", "/r/", body, |word| {
            if word == "foo" {
                Some(7)
            } else {
                None
            }
        })
    }

    #[test]
    fn rewrites_entry_links() {
        assert_eq!(
            rw(r#"<a href="entry://foo">x</a>"#),
            r#"<a href="/i/7">x</a>"#
        );
        assert_eq!(
            rw("<a href='entry://foo#part'>x</a>"),
            "<a href='/i/7'>x</a>"
        );
    }

    #[test]
    fn rewrites_sound_links() {
        assert_eq!(
            rw(r#"<audio src="sound://a b.mp3"></audio>"#),
            r#"<audio src="/r/a%20b.mp3"></audio>"#
        );
    }

    #[test]
    fn leaves_unknown_and_non_links_alone() {
        let body = r#"<a href="entry://missing">m</a><span data-x="entry://foo">d</span><p>entry://foo</p>"#;
        assert_eq!(rw(body), body);
    }

    #[test]
    fn leaves_prefixed_values_alone() {
        let body = r#"<a href="?u=entry://foo">x</a>"#;
        assert_eq!(rw(body), body);
    }
}
