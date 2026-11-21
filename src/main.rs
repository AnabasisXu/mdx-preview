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
use ripemd::{Digest, Ripemd128};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
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

/// Parse an .mdx container into (headword, entry HTML) pairs, in file order.
fn load_mdx(path: &str) -> io::Result<(Vec<(String, String)>, String)> {
    let d = fs::read(path)?;
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

    let mut stream: Vec<u8> = Vec::new();
    for (comp, _decomp) in &info {
        let kind = le_u32(&d, p);
        let body = &d[p + 8..p + comp];
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
        stream.extend_from_slice(&block);
        p += comp;
    }

    let mut out = Vec::with_capacity(keys.len());
    for idx in 0..keys.len() {
        let start = keys[idx].0 as usize;
        let end = if idx + 1 < keys.len() {
            keys[idx + 1].0 as usize
        } else {
            stream.len()
        };
        let text = decode(
            &stream[start.min(stream.len())..end.min(stream.len())],
            codec,
        )
        .trim_end_matches('\0')
        .to_string();
        out.push((keys[idx].1.clone(), text));
    }
    resolve_links(&mut out);
    Ok((out, encoding))
}

/// MDict redirect entries carry no content of their own: their body is
/// `@@@LINK=other headword`. Show the target entry instead of the raw marker.
fn resolve_links(entries: &mut [(String, String)]) {
    let mut index: HashMap<&str, usize> = HashMap::with_capacity(entries.len());
    for (i, (word, _)) in entries.iter().enumerate() {
        index.entry(word.as_str()).or_insert(i);
    }
    let link: Vec<Option<usize>> = entries
        .iter()
        .map(|(_, text)| {
            text.trim()
                .strip_prefix("@@@LINK=")
                .map(str::trim)
                .and_then(|name| index.get(name).copied())
        })
        .collect();
    let resolved: Vec<String> = (0..entries.len())
        .map(|i| {
            let (mut cur, mut hops) = (i, 0);
            while let Some(j) = link[cur] {
                if j == i || hops == 16 {
                    return entries[i].1.clone(); // cycle: keep the marker
                }
                cur = j;
                hops += 1;
            }
            entries[cur].1.clone()
        })
        .collect();
    for (i, text) in resolved.into_iter().enumerate() {
        if link[i].is_some() {
            entries[i].1 = text;
        }
    }
}

// ---------------------------------------------------------------- web layer

struct Dict {
    words: Vec<(String, u32)>,
    texts: HashMap<u32, String>,
    encoding: String,
}

impl Dict {
    fn suggest_page(&self, q: &str, offset: usize, limit: usize) -> (Vec<(String, u32)>, bool) {
        let q = q.to_lowercase();
        let mut items = Vec::with_capacity(limit);
        let mut skipped = 0;
        let mut has_more = false;
        for (word, id) in &self.words {
            if !q.is_empty() && !word.to_lowercase().contains(&q) {
                continue;
            }
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if items.len() < limit {
                items.push((word.clone(), *id));
            } else {
                has_more = true;
                break;
            }
        }
        (items, has_more)
    }
}

struct AppState {
    dict: Arc<Dict>,
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
        .replace("{title}", title)
        .replace("{attributes}", body_html)
}

#[get("/api/parser")]
async fn parser(state: web::Data<AppState>) -> impl Responder {
    HttpResponse::Ok().json(json!({
        "encoding": state.dict.encoding,
        "count": state.dict.words.len(),
    }))
}

#[get("/api/item/{k_id}")]
async fn item(k_id: web::Path<u32>, state: web::Data<AppState>) -> impl Responder {
    let k_id = k_id.into_inner();
    match state.dict.texts.get(&k_id) {
        Some(text) => {
            let title = state
                .dict
                .words
                .iter()
                .find(|(_, id)| *id == k_id)
                .map(|(w, _)| w.as_str())
                .unwrap_or("");
            HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .body(render_item(title, text))
        }
        None => HttpResponse::NotFound().body("404"),
    }
}
#[get("/api/lookup/{word}")]
async fn lookup(word: web::Path<String>, state: web::Data<AppState>) -> impl Responder {
    let (hits, has_more) = state.dict.suggest_page(&word, 0, 20);
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
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("json") => "application/json",
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
                            let (hits, has_more) = state2.dict.suggest_page(q, offset, 20);
                            let items: Vec<[Value; 2]> = hits
                                .into_iter()
                                .map(|(w, id)| [json!(w), json!(id)])
                                .collect();
                            json!({"id": id, "result": {"items": items, "has_more": has_more}, "error": null})
                        }
                        "load_text" => {
                            let k = params.get(0).and_then(|p| p.as_u64()).unwrap_or(0) as u32;
                            let text = state2.dict.texts.get(&k).cloned().unwrap_or_default();
                            json!({"id": id, "result": text, "error": null})
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

    let (entries, encoding) = match load_mdx(&file) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("prev: cannot read {file}: {e}");
            std::process::exit(1);
        }
    };
    if entries.is_empty() {
        eprintln!("prev: no entries in {file}");
        std::process::exit(1);
    }
    let count = entries.len();
    let mut words = Vec::with_capacity(count);
    let mut texts = HashMap::with_capacity(count);
    for (i, (word, text)) in entries.into_iter().enumerate() {
        let id = (i + 1) as u32;
        words.push((word, id));
        texts.insert(id, text);
    }
    let _ = BASE.set(base.clone());

    let dict = Arc::new(Dict {
        words,
        texts,
        encoding,
    });
    let (tx, _rx) = broadcast::channel(32);
    let state = web::Data::new(AppState { dict, tx });

    println!("prev: {count} headwords from {file}");
    println!("listening on http://127.0.0.1:{port}{base}/");
    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .configure(root)
            .service(web::scope(base.as_str()).configure(routes))
    })
    .bind(("127.0.0.1", port))?
    .run()
    .await
}
