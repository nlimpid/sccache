// Copyright 2026 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! `GOCACHEPROG` child process.
//!
//! Go 1.24+ spawns this process and speaks JSON on stdin/stdout. Entries are
//! stored on local disk so the protocol can be logged and inspected. This is
//! not wired to sccache's storage backends.

use crate::config::default_disk_cache_dir;
use crate::errors::*;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use base64::read::DecoderReader;
use filetime::{FileTime, set_file_mtime};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ENV_CACHE_DIR: &str = "SCCACHE_GO_CACHEPROG_DIR";

/// `cmd/go/internal/cache` hashes are SHA-256.
const HASH_LEN: usize = 32;
const HEX_LEN: usize = HASH_LEN * 2;

/// Go updates a cache file's mtime at most once per hour.
const MTIME_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Base64 `[]byte` field, matching `encoding/json`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct B64Bytes(Vec<u8>);

impl Serialize for B64Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&B64.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for B64Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.is_empty() {
            return Ok(Self(Vec::new()));
        }
        B64.decode(text.as_bytes())
            .map(Self)
            .map_err(D::Error::custom)
    }
}

impl B64Bytes {
    fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

/// Non-empty decoded bytes, if the field was present.
fn present(bytes: Option<&B64Bytes>) -> Option<&[u8]> {
    bytes.map(B64Bytes::as_slice).filter(|b| !b.is_empty())
}

#[derive(Debug, Deserialize)]
struct Request {
    #[serde(rename = "ID")]
    id: i64,
    #[serde(rename = "Command")]
    command: String,
    #[serde(default, rename = "ActionID")]
    action_id: Option<B64Bytes>,
    #[serde(default, rename = "OutputID")]
    output_id: Option<B64Bytes>,
    // Pre-1.24 name of OutputID. Go 1.25 stopped sending it.
    #[serde(default, rename = "ObjectID")]
    object_id: Option<B64Bytes>,
    #[serde(default, rename = "BodySize")]
    body_size: i64,
}

impl Request {
    /// `OutputID`, or `ObjectID` when that is all Go sent.
    fn effective_output_id(&self) -> Option<&[u8]> {
        present(self.output_id.as_ref()).or(present(self.object_id.as_ref()))
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Response {
    #[serde(rename = "ID")]
    id: i64,
    #[serde(skip_serializing_if = "Option::is_none", rename = "Err")]
    err: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "KnownCommands")]
    known_commands: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "is_false", rename = "Miss")]
    miss: bool,
    #[serde(skip_serializing_if = "Option::is_none", rename = "OutputID")]
    output_id: Option<B64Bytes>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "Size")]
    size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "Time")]
    time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "DiskPath")]
    disk_path: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Response {
    fn capabilities() -> Self {
        Self {
            id: 0,
            err: None,
            known_commands: Some(vec!["get".to_owned(), "put".to_owned(), "close".to_owned()]),
            miss: false,
            output_id: None,
            size: None,
            time: None,
            disk_path: None,
        }
    }

    fn miss(id: i64) -> Self {
        Self {
            id,
            err: None,
            known_commands: None,
            miss: true,
            output_id: None,
            size: None,
            time: None,
            disk_path: None,
        }
    }

    fn hit(id: i64, stored: Stored) -> Self {
        Self {
            id,
            err: None,
            known_commands: None,
            miss: false,
            output_id: Some(B64Bytes(stored.output_id)),
            size: Some(stored.size),
            time: Some(stored.time),
            disk_path: Some(stored.path),
        }
    }

    fn put_ok(id: i64, disk_path: String) -> Self {
        Self {
            id,
            err: None,
            known_commands: None,
            miss: false,
            output_id: None,
            size: None,
            time: None,
            disk_path: Some(disk_path),
        }
    }

    fn close_ok(id: i64) -> Self {
        Self {
            id,
            err: None,
            known_commands: None,
            miss: false,
            output_id: None,
            size: None,
            time: None,
            disk_path: None,
        }
    }

    fn error(id: i64, err: impl Into<String>) -> Self {
        Self {
            id,
            err: Some(err.into()),
            known_commands: None,
            miss: false,
            output_id: None,
            size: None,
            time: None,
            disk_path: None,
        }
    }
}

#[derive(Debug, Default)]
struct SessionStats {
    gets: u64,
    hits: u64,
    misses: u64,
    puts: u64,
    put_bytes: u64,
}

struct Stored {
    output_id: Vec<u8>,
    size: i64,
    time: String,
    path: String,
}

/// A put the caller got wrong, as opposed to a disk failure.
#[derive(Debug)]
struct PutRejected(String);

impl std::fmt::Display for PutRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PutRejected {}

/// On-disk entry, matching `cmd/go/internal/cache.DiskCache`.
///
/// `<hh>/<hex>-a` is one action record. `<hh>/<hex>-d` is the output body,
/// named by OutputID and written once. Go records the DiskPath it was given
/// and later reads that path by OutputID, so a later put of the same action
/// must not change bytes already handed out.
struct LocalStore {
    dir: PathBuf,
}

impl LocalStore {
    fn open(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating go-cacheprog dir {}", dir.display()))?;
        Ok(Self { dir })
    }

    fn file_name(&self, id: &[u8], suffix: &str) -> PathBuf {
        let hex = hex_encode(id);
        // `id` is a SHA-256, so the first byte is always there to shard on.
        self.dir.join(&hex[..2]).join(format!("{hex}-{suffix}"))
    }

    fn get(&self, action_id: &[u8]) -> Option<Stored> {
        let action_path = self.file_name(action_id, "a");
        let entry = read_action(&action_path, action_id)?;
        let data_path = self.file_name(&entry.output_id, "d");
        let meta = fs::metadata(&data_path).ok()?;
        if meta.len() != entry.size as u64 {
            warn!(
                "go-cacheprog: size mismatch for {} (action {} file {})",
                data_path.display(),
                entry.size,
                meta.len()
            );
            return None;
        }
        // Go also marks the data file used. There is no trim yet, so only the
        // action record is touched; a trim has to count `-d` use separately.
        mark_used(&action_path);
        let path = absolute_utf8(&data_path).ok()?;
        Some(Stored {
            output_id: entry.output_id,
            size: entry.size,
            time: entry.time,
            path,
        })
    }

    fn put(&self, action_id: &[u8], output_id: &[u8], body: &[u8]) -> Result<String> {
        let size = i64::try_from(body.len()).context("put body does not fit in i64")?;
        self.put_reader(action_id, output_id, size, body)
    }

    fn put_reader(
        &self,
        action_id: &[u8],
        output_id: &[u8],
        size: i64,
        body: impl Read,
    ) -> Result<String> {
        let data_path = self.file_name(output_id, "d");
        write_output_once(&data_path, output_id, size, body)?;
        write_action(&self.file_name(action_id, "a"), action_id, output_id, size)?;
        absolute_utf8(&data_path)
    }
}

struct ActionEntry {
    output_id: Vec<u8>,
    size: i64,
    time: String,
}

/// `v1 <hex id> <hex out> <size, width 20> <unixnano, width 20>\n`.
///
/// The size and timestamp are space-padded, so the line is not split on
/// spaces: a small size would look like extra fields.
const ENTRY_SIZE: usize = 2 + 1 + HEX_LEN + 1 + HEX_LEN + 1 + 20 + 1 + 20 + 1;

fn read_action(path: &Path, action_id: &[u8]) -> Option<ActionEntry> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!("go-cacheprog: reading {}: {err}", path.display());
            return None;
        }
    };
    parse_action(&bytes, action_id)
}

fn parse_action(bytes: &[u8], action_id: &[u8]) -> Option<ActionEntry> {
    // Go reads entrySize bytes and treats anything else as a miss.
    if bytes.len() != ENTRY_SIZE || bytes[ENTRY_SIZE - 1] != b'\n' {
        return None;
    }
    if &bytes[..3] != b"v1 "
        || bytes[3 + HEX_LEN] != b' '
        || bytes[3 + HEX_LEN + 1 + HEX_LEN] != b' '
    {
        return None;
    }
    let id_at = 3;
    let out_at = id_at + HEX_LEN + 1;
    let size_at = out_at + HEX_LEN + 1;
    let time_at = size_at + 20 + 1;
    let Ok(id) = hex_decode(&bytes[id_at..id_at + HEX_LEN]) else {
        return None;
    };
    if id != action_id {
        return None;
    }
    let Ok(output_id) = hex_decode(&bytes[out_at..out_at + HEX_LEN]) else {
        return None;
    };
    let Ok(size_text) = std::str::from_utf8(&bytes[size_at..size_at + 20]) else {
        return None;
    };
    let Ok(size) = size_text.trim().parse::<i64>() else {
        return None;
    };
    if size < 0 {
        return None;
    }
    let Ok(time_text) = std::str::from_utf8(&bytes[time_at..time_at + 20]) else {
        return None;
    };
    let Ok(nanos) = time_text.trim().parse::<i64>() else {
        return None;
    };
    if nanos < 0 {
        return None;
    }
    Some(ActionEntry {
        output_id,
        size,
        time: rfc3339_nanos(nanos),
    })
}

fn write_action(path: &Path, action_id: &[u8], output_id: &[u8], size: i64) -> Result<()> {
    let parent = path
        .parent()
        .context("go-cacheprog entry path has no parent")?;
    fs::create_dir_all(parent)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let text = format!(
        "v1 {} {} {size:20} {nanos:20}\n",
        hex_encode(action_id),
        hex_encode(output_id)
    );
    // One rename publishes the record. A reader never sees a new body with
    // the previous record, because the body lives under the output id.
    atomic_write(parent, path, text.as_bytes())
}

/// Write the body under its OutputID if that file is not already the right size.
///
/// An existing file of the expected size is left alone: another process may
/// have it open from an earlier get. A size mismatch is a corrupted entry and
/// is replaced. The body is hashed while it is written; a mismatch leaves the
/// previous file in place.
fn write_output_once(path: &Path, output_id: &[u8], size: i64, body: impl Read) -> Result<()> {
    let expected = u64::try_from(size).context("negative output size")?;
    if let Ok(meta) = fs::metadata(path)
        && meta.is_file()
        && meta.len() == expected
    {
        return Ok(());
    }
    let parent = path
        .parent()
        .context("go-cacheprog entry path has no parent")?;
    fs::create_dir_all(parent)?;
    let tmp = write_hashed(parent, body, output_id, expected)?;
    let tmp = match tmp.persist_noclobber(path) {
        Ok(_) => return Ok(()),
        Err(err) => err.file,
    };
    // Another writer may have published this output while we hashed it.
    // Leave its file alone, including handles Go may already have open.
    if let Ok(meta) = fs::metadata(path)
        && meta.is_file()
        && meta.len() == expected
    {
        return Ok(());
    }
    tmp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn write_hashed(
    dir: &Path,
    mut body: impl Read,
    output_id: &[u8],
    expected: u64,
) -> Result<tempfile::NamedTempFile> {
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(dir)
        .with_context(|| format!("creating temp file in {}", dir.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        let n = read_body(&mut body, &mut buf)?;
        if n == 0 {
            break;
        }
        written = written
            .checked_add(n as u64)
            .ok_or_else(|| reject(PutRejected("put body is too large".to_owned())))?;
        if written > expected {
            return Err(reject(PutRejected(format!(
                "put body size mismatch: got more than {expected} bytes"
            ))));
        }
        hasher.update(&buf[..n]);
        tmp.write_all(&buf[..n])?;
    }
    if written != expected {
        return Err(reject(PutRejected(format!(
            "put body size mismatch: got {written} expected {expected}"
        ))));
    }
    let sum = hasher.finalize();
    if sum.as_slice() != output_id {
        return Err(reject(PutRejected(
            "put body hash does not match OutputID".to_owned(),
        )));
    }
    Ok(tmp)
}

/// A short or corrupt base64 stream is a bad frame. A disk error is not.
fn read_body(body: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    match body.read(buf) {
        Ok(n) => Ok(n),
        Err(err)
            if err
                .get_ref()
                .is_some_and(|source| source.downcast_ref::<base64::DecodeError>().is_some()) =>
        {
            Err(reject(PutRejected(format!(
                "put body is not base64: {err}"
            ))))
        }
        Err(err) => Err(err.into()),
    }
}

fn reject(rejected: PutRejected) -> Error {
    Error::new(rejected)
}

fn atomic_write(dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(dir)
        .with_context(|| format!("creating temp file in {}", dir.display()))?;
    tmp.write_all(bytes)?;
    tmp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Best-effort mtime update, skipped when the file was touched within the hour.
fn mark_used(path: &Path) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    let Ok(modified) = meta.modified() else {
        return;
    };
    if modified.elapsed().unwrap_or(MTIME_INTERVAL) < MTIME_INTERVAL {
        return;
    }
    let _ = set_file_mtime(path, FileTime::now());
}

#[cfg(test)]
fn sha256(bytes: &[u8]) -> [u8; HASH_LEN] {
    let digest = Sha256::digest(bytes);
    let mut out = [0; HASH_LEN];
    out.copy_from_slice(&digest);
    out
}

fn rfc3339_nanos(nanos: i64) -> String {
    let whole = nanos.div_euclid(1_000_000_000);
    let frac = nanos.rem_euclid(1_000_000_000) as u32;
    chrono::DateTime::from_timestamp(whole, frac)
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00.000000000Z".to_owned())
}

fn absolute_utf8(path: &Path) -> Result<String> {
    let abs = std::path::absolute(path)
        .with_context(|| format!("absolute path for {}", path.display()))?;
    abs.to_str()
        .map(str::to_owned)
        .context("go-cacheprog path is not UTF-8")
}

/// Directory for the debug store.
///
/// `LruDiskCache` walks every file under the disk cache root and deletes ones
/// it does not own, so the default lives beside that directory rather than
/// inside it.
pub(crate) fn default_go_cacheprog_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(ENV_CACHE_DIR) {
        return PathBuf::from(dir);
    }
    let base = default_disk_cache_dir();
    let mut name = base.file_name().unwrap_or_default().to_os_string();
    name.push("-go-cacheprog");
    base.with_file_name(name)
}

/// Run until `close` or stdin EOF.
pub(crate) fn run(cache_dir: PathBuf) -> Result<()> {
    let cache = LocalStore::open(cache_dir)?;
    info!("go-cacheprog: listening store={}", cache.dir.display());
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve(&cache, stdin.lock(), stdout.lock())?;
    Ok(())
}

fn serve(
    cache: &LocalStore,
    mut input: impl BufRead,
    mut output: impl Write,
) -> Result<SessionStats> {
    write_response(&mut output, &Response::capabilities())?;
    let mut stats = SessionStats::default();
    loop {
        let Some(line) = read_nonblank_line(&mut input)? else {
            log_summary("stdin closed", &stats);
            return Ok(stats);
        };
        trace!("go-cacheprog recv {line}");
        let req: Request = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(err) => bail!("parsing GOCACHEPROG request: {line}: {err}"),
        };
        log_request(&req);

        let command = req.command.clone();
        let resp = match command.as_str() {
            "get" => {
                stats.gets += 1;
                let resp = handle_get(cache, &req);
                if resp.miss {
                    stats.misses += 1;
                } else if resp.err.is_none() {
                    stats.hits += 1;
                }
                resp
            }
            "put" => {
                stats.puts += 1;
                let resp = handle_put(cache, &req, &mut input);
                if resp.err.is_none() {
                    stats.put_bytes += u64::try_from(req.body_size).unwrap_or(0);
                }
                resp
            }
            "close" => {
                let resp = Response::close_ok(req.id);
                log_response(&command, &resp);
                write_response(&mut output, &resp)?;
                log_summary("close", &stats);
                return Ok(stats);
            }
            other => Response::error(req.id, format!("unsupported command {other}")),
        };
        log_response(&command, &resp);
        write_response(&mut output, &resp)?;
    }
}

fn log_request(req: &Request) {
    debug!(
        "go-cacheprog >> id={} cmd={} action={} output={} object={} body_size={}",
        req.id,
        req.command,
        hex_or_dash(present(req.action_id.as_ref())),
        hex_or_dash(present(req.output_id.as_ref())),
        hex_or_dash(present(req.object_id.as_ref())),
        req.body_size
    );
}

fn log_response(cmd: &str, resp: &Response) {
    debug!(
        "go-cacheprog << id={} cmd={} miss={} err={} size={} disk={}",
        resp.id,
        cmd,
        if resp.miss { "true" } else { "false" },
        resp.err.as_deref().unwrap_or("-"),
        resp.size
            .map(|size| size.to_string())
            .unwrap_or_else(|| "-".to_owned()),
        resp.disk_path.as_deref().unwrap_or("-")
    );
}

fn log_summary(reason: &str, stats: &SessionStats) {
    info!(
        "go-cacheprog: {reason} gets={} hits={} misses={} puts={} put_bytes={}",
        stats.gets, stats.hits, stats.misses, stats.puts, stats.put_bytes
    );
}

fn hex_or_dash(bytes: Option<&[u8]>) -> String {
    match bytes {
        Some(bytes) => hex_encode(bytes),
        None => "-".to_owned(),
    }
}

/// A storage failure is a miss. Go forwards a get `Err` as a plain error.
fn handle_get(cache: &LocalStore, req: &Request) -> Response {
    let Some(action_id) = hash_id(present(req.action_id.as_ref())) else {
        return Response::miss(req.id);
    };
    match cache.get(&action_id) {
        Some(stored) => Response::hit(req.id, stored),
        None => Response::miss(req.id),
    }
}

/// A storage failure is an `Err` response. Only a broken frame exits the process.
fn handle_put(cache: &LocalStore, req: &Request, input: &mut impl BufRead) -> Response {
    // The body line follows the header whenever BodySize is positive. It has
    // to be consumed before returning, or the next request is the body.
    let action_id = hash_id(present(req.action_id.as_ref()));
    let output_from_field = present(req.output_id.as_ref());
    let output_from_object = present(req.object_id.as_ref());
    if let (Some(output_id), Some(object_id)) = (output_from_field, output_from_object)
        && output_id != object_id
    {
        warn!(
            "go-cacheprog id={} OutputID and ObjectID differ; using OutputID",
            req.id
        );
    }
    let output_id = hash_id(req.effective_output_id());
    if req.body_size < 0 {
        return Response::error(req.id, "negative BodySize");
    }
    let Some(action_id) = action_id else {
        let _ = discard_body(input, req.body_size);
        return Response::error(req.id, "put missing ActionID");
    };
    let Some(output_id) = output_id else {
        let _ = discard_body(input, req.body_size);
        return Response::error(req.id, "put missing OutputID");
    };

    let stored = if req.body_size == 0 {
        cache.put(&action_id, &output_id, &[])
    } else {
        // Encode writes a newline, then prog.go writes another before the body.
        let Some(encoded) = (match read_json_string(input) {
            Ok(encoded) => encoded,
            Err(err) => return Response::error(req.id, err),
        }) else {
            return Response::error(req.id, "put missing body");
        };
        let decoder = DecoderReader::new(encoded.as_bytes(), &B64);
        cache.put_reader(&action_id, &output_id, req.body_size, decoder)
    };
    match stored {
        Ok(path) => Response::put_ok(req.id, path),
        Err(err) => {
            warn!("go-cacheprog id={} put failed: {err:#}", req.id);
            // The frame is bad when the error is a rejection. A disk error stays
            // generic so a path is not copied into every build's stderr.
            match err.downcast::<PutRejected>() {
                Ok(rejected) => Response::error(req.id, rejected.0),
                Err(_) => Response::error(req.id, "put failed"),
            }
        }
    }
}

/// Drop the body line of a put that will not be stored.
fn discard_body(input: &mut impl BufRead, body_size: i64) -> std::result::Result<(), String> {
    if body_size <= 0 {
        return Ok(());
    }
    read_json_string(input).map(|_| ())
}

/// The put body is a JSON string on its own line. The quotes are stripped so
/// the base64 can be decoded as it is written. The encoded text itself is
/// still buffered: the alphabet needs no JSON escaping, so this is one copy.
fn read_json_string(input: &mut impl BufRead) -> std::result::Result<Option<String>, String> {
    let Some(line) =
        read_nonblank_line(input).map_err(|err| format!("put body read failed: {err:#}"))?
    else {
        return Ok(None);
    };
    let value: String =
        serde_json::from_str(&line).map_err(|_| "put body is not a JSON string".to_owned())?;
    Ok(Some(value))
}

fn hash_id(bytes: Option<&[u8]>) -> Option<[u8; HASH_LEN]> {
    let bytes = bytes?;
    if bytes.len() != HASH_LEN {
        return None;
    }
    let mut id = [0; HASH_LEN];
    id.copy_from_slice(bytes);
    Some(id)
}

fn write_response(out: &mut impl Write, resp: &Response) -> Result<()> {
    serde_json::to_writer(&mut *out, resp)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

/// Skip the blank line Go writes after every JSON request.
fn read_nonblank_line(input: &mut impl BufRead) -> Result<Option<String>> {
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        if !line.trim().is_empty() {
            return Ok(Some(line));
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

fn hex_decode(text: &[u8]) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        bail!("odd hex length");
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.chunks(2) {
        let hi = hex_value(pair[0])?;
        let lo = hex_value(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_value(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => bail!("invalid hex digit"),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::fs::File;
    use std::io::{Cursor, Read};
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn b64(bytes: &[u8]) -> String {
        B64.encode(bytes)
    }

    fn id(byte: u8) -> [u8; HASH_LEN] {
        let mut bytes = [0; HASH_LEN];
        bytes[0] = byte;
        bytes
    }

    fn hashed(body: &[u8]) -> [u8; HASH_LEN] {
        sha256(body)
    }

    fn drive(input: &str) -> (Vec<Response>, tempfile::TempDir, SessionStats) {
        let dir = tempfile::tempdir().unwrap();
        let cache = LocalStore::open(dir.path().join("store")).unwrap();
        let mut output = Vec::new();
        let stats = serve(&cache, Cursor::new(input.as_bytes()), &mut output).unwrap();
        let responses = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).expect(line))
            .collect();
        (responses, dir, stats)
    }

    fn put_frame(id: i64, action: &[u8], field: &str, output: &[u8], body: &[u8]) -> String {
        let header = format!(
            r#"{{"ID":{id},"Command":"put","ActionID":"{}","{field}":"{}","BodySize":{}}}"#,
            b64(action),
            b64(output),
            body.len()
        );
        // Header newline, the extra blank line prog.go writes, then the body.
        let body_line = serde_json::to_string(&b64(body)).unwrap();
        format!("{header}\n\n{body_line}\n")
    }

    fn get_frame(id: i64, action: &[u8]) -> String {
        format!(
            "{{\"ID\":{id},\"Command\":\"get\",\"ActionID\":\"{}\"}}\n\n",
            b64(action)
        )
    }

    fn disk_bytes(resp: &Response) -> Vec<u8> {
        fs::read(resp.disk_path.as_deref().unwrap()).unwrap()
    }

    #[test]
    fn parse_get_request() {
        let req: Request =
            serde_json::from_str(r#"{"ID":1,"Command":"get","ActionID":"AQIDBA=="}"#).unwrap();
        assert_eq!(req.id, 1);
        assert_eq!(req.command, "get");
        assert_eq!(present(req.action_id.as_ref()), Some(&[1, 2, 3, 4][..]));
        assert!(req.effective_output_id().is_none());
        assert_eq!(req.body_size, 0);
    }

    #[test]
    fn parse_put_output_id() {
        let req: Request = serde_json::from_str(
            r#"{"ID":2,"Command":"put","ActionID":"AQID","OutputID":"BAUG","BodySize":3}"#,
        )
        .unwrap();
        assert_eq!(req.body_size, 3);
        assert_eq!(req.effective_output_id(), Some(&[4, 5, 6][..]));
        assert!(req.object_id.is_none());
    }

    #[test]
    fn parse_put_deprecated_object_id() {
        let req: Request = serde_json::from_str(
            r#"{"ID":2,"Command":"put","ActionID":"AQID","ObjectID":"BAUG","BodySize":0}"#,
        )
        .unwrap();
        assert_eq!(present(req.output_id.as_ref()), None);
        assert_eq!(req.effective_output_id(), Some(&[4, 5, 6][..]));
    }

    #[test]
    fn output_id_wins_when_object_id_differs() {
        let action = id(0xab);
        let body = b"data";
        let output = hashed(body);
        let object = id(0x33);
        let header = format!(
            r#"{{"ID":2,"Command":"put","ActionID":"{}","OutputID":"{}","ObjectID":"{}","BodySize":4}}"#,
            b64(&action),
            b64(&output),
            b64(&object)
        );
        let encoded = serde_json::to_string(&b64(body)).unwrap();
        let input = format!("{header}\n\n{encoded}\n{}", get_frame(3, &action));
        let (responses, _dir, stats) = drive(&input);
        assert!(responses[1].err.is_none(), "{:?}", responses[1].err);
        assert!(!responses[2].miss);
        assert_eq!(
            responses[2].output_id.as_ref().map(B64Bytes::as_slice),
            Some(output.as_slice())
        );
        assert_eq!(stats.hits, 1);
        assert_eq!(disk_bytes(&responses[2]), body);
    }

    #[test]
    fn put_body_after_blank_line() {
        let action = id(1);
        let body = b"hello";
        let output = hashed(body);
        let input = format!(
            "{}{}{}{{\"ID\":4,\"Command\":\"close\"}}\n\n",
            get_frame(1, &action),
            put_frame(2, &action, "OutputID", &output, body),
            get_frame(3, &action)
        );
        let (responses, _dir, stats) = drive(&input);
        assert_eq!(responses[0].id, 0);
        assert!(responses[1].miss);
        assert!(responses[2].disk_path.is_some());
        assert!(!responses[3].miss);
        assert_eq!(responses[3].size, Some(5));
        assert_eq!(responses[4].id, 4);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.puts, 1);
        assert_eq!(stats.put_bytes, 5);
        let path = PathBuf::from(responses[3].disk_path.as_deref().unwrap());
        assert!(path.is_absolute());
        assert!(path.file_name().unwrap().to_str().unwrap().ends_with("-d"));
        assert_eq!(disk_bytes(&responses[3]), body);
    }

    #[test]
    fn object_id_put_with_blank_line_round_trips() {
        let action = id(0x10);
        let body = b"xyz";
        let object = hashed(body);
        let input = format!(
            "{}{}",
            put_frame(1, &action, "ObjectID", &object, body),
            get_frame(2, &action)
        );
        let (responses, _dir, _) = drive(&input);
        assert!(responses[1].err.is_none(), "{:?}", responses[1].err);
        assert_eq!(
            responses[2].output_id.as_ref().map(B64Bytes::as_slice),
            Some(object.as_slice())
        );
        assert_eq!(disk_bytes(&responses[2]), body);
    }

    #[test]
    fn zero_size_put_does_not_consume_next_request() {
        let action = id(0xaa);
        let output = hashed(b"");
        let input = format!(
            "{{\"ID\":1,\"Command\":\"put\",\"ActionID\":\"{}\",\"OutputID\":\"{}\",\"BodySize\":0}}\n\n{}",
            b64(&action),
            b64(&output),
            get_frame(2, &action)
        );
        let (responses, _dir, stats) = drive(&input);
        assert_eq!(responses.len(), 3, "{responses:?}");
        assert!(responses[1].err.is_none(), "{:?}", responses[1].err);
        assert_eq!(responses[2].id, 2);
        assert!(!responses[2].miss);
        assert_eq!(responses[2].size, Some(0));
        assert_eq!(stats.hits, 1);
        assert_eq!(disk_bytes(&responses[2]), b"");
    }

    #[test]
    fn local_get_put_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LocalStore::open(dir.path().to_path_buf()).unwrap();
        let action = id(0xab);
        let body = b"hello-go-cache";
        let output = hashed(body);
        assert!(cache.get(&action).is_none());

        let path = PathBuf::from(cache.put(&action, &output, body).unwrap());
        assert!(path.is_absolute());
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("{}-d", hex_encode(&output))
        );
        assert_eq!(
            path.parent().unwrap().file_name().unwrap(),
            &hex_encode(&output)[..2]
        );

        let stored = cache.get(&action).unwrap();
        assert_eq!(stored.output_id, output);
        assert_eq!(stored.size, body.len() as i64);
        assert_eq!(fs::read(&stored.path).unwrap(), body);
        chrono::DateTime::parse_from_rfc3339(&stored.time).unwrap();
        assert!(cache.get(&id(0xff)).is_none());

        let action_file = cache.file_name(&action, "a");
        let record = fs::read_to_string(&action_file).unwrap();
        assert!(record.starts_with("v1 "));
        assert!(record.ends_with('\n'));
        assert_eq!(record.len(), ENTRY_SIZE);
    }

    #[test]
    fn reput_keeps_the_previous_output_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LocalStore::open(dir.path().to_path_buf()).unwrap();
        let action = id(0x01);
        let first = b"first-output";
        let second = b"second-output-which-is-longer";
        let first_id = hashed(first);
        let second_id = hashed(second);

        let first_path = cache.put(&action, &first_id, first).unwrap();
        let first_bytes = fs::read(&first_path).unwrap();
        let second_path = cache.put(&action, &second_id, second).unwrap();

        assert_ne!(first_path, second_path);
        assert_eq!(fs::read(&first_path).unwrap(), first_bytes);
        assert_eq!(fs::read(&second_path).unwrap(), second);
        let stored = cache.get(&action).unwrap();
        assert_eq!(stored.output_id, second_id);
        assert_eq!(stored.path, second_path);
    }

    #[test]
    fn corrupt_action_record_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LocalStore::open(dir.path().to_path_buf()).unwrap();
        let action = id(0x02);
        let body = b"kept";
        let output = hashed(body);
        let data_path = cache.put(&action, &output, body).unwrap();
        fs::write(cache.file_name(&action, "a"), b"not a record\n").unwrap();

        let mut output_buf = Vec::new();
        serve(
            &cache,
            Cursor::new(get_frame(9, &action).as_bytes()),
            &mut output_buf,
        )
        .unwrap();
        let responses: Vec<Response> = String::from_utf8(output_buf)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(responses[1].miss);
        assert!(responses[1].err.is_none());
        // The output file is not rewritten just because the action record is bad.
        assert_eq!(fs::read(data_path).unwrap(), body);
    }

    #[test]
    fn short_id_is_rejected() {
        let action = [1, 2, 3, 4];
        let input = format!(
            "{{\"ID\":1,\"Command\":\"put\",\"ActionID\":\"{}\",\"OutputID\":\"{}\",\"BodySize\":0}}\n\n",
            b64(&action),
            b64(&id(1))
        );
        let (responses, _dir, stats) = drive(&input);
        assert!(
            responses[1].err.as_ref().unwrap().contains("ActionID"),
            "{:?}",
            responses[1].err
        );
        assert_eq!(stats.puts, 1);
        assert_eq!(stats.put_bytes, 0);
    }

    #[test]
    fn rejected_put_still_reads_its_body() {
        let short = [1, 2, 3, 4];
        let next = id(2);
        let body = b"hello";
        let header = format!(
            r#"{{"ID":1,"Command":"put","ActionID":"{}","OutputID":"{}","BodySize":{}}}"#,
            b64(&short),
            b64(&id(1)),
            body.len()
        );
        let encoded = serde_json::to_string(&b64(body)).unwrap();
        let input = format!(
            "{header}\n\n{encoded}\n{}",
            put_frame(2, &next, "OutputID", &hashed(body), body)
        );
        let (responses, _dir, stats) = drive(&input);
        assert_eq!(responses.len(), 3, "{responses:?}");
        assert!(
            responses[1].err.as_ref().unwrap().contains("ActionID"),
            "{:?}",
            responses[1].err
        );
        assert!(responses[2].err.is_none(), "{:?}", responses[2].err);
        assert_eq!(stats.puts, 2);
        assert_eq!(disk_bytes(&responses[2]), body);
    }

    #[test]
    fn body_size_mismatch_is_an_error() {
        let action = id(1);
        let output = hashed(b"hello");
        let header = format!(
            r#"{{"ID":7,"Command":"put","ActionID":"{}","OutputID":"{}","BodySize":3}}"#,
            b64(&action),
            b64(&output)
        );
        let body = serde_json::to_string(&b64(b"hello")).unwrap();
        let input = format!("{header}\n\n{body}\n{}", get_frame(8, &action));
        let (responses, _dir, stats) = drive(&input);
        assert!(responses[1].err.as_ref().unwrap().contains("mismatch"));
        assert!(responses[2].miss);
        assert_eq!(stats.puts, 1);
        assert_eq!(stats.put_bytes, 0);
        assert_eq!(stats.hits, 0);
    }

    #[test]
    fn body_that_is_not_base64_is_an_error() {
        let action = id(5);
        let header = format!(
            r#"{{"ID":1,"Command":"put","ActionID":"{}","OutputID":"{}","BodySize":4}}"#,
            b64(&action),
            b64(&id(1))
        );
        let input = format!("{header}\n\n\"!!!!\"\n{}", get_frame(2, &action));
        let (responses, _dir, stats) = drive(&input);
        assert!(
            responses[1].err.as_ref().unwrap().contains("base64"),
            "{:?}",
            responses[1].err
        );
        assert!(responses[2].miss);
        assert_eq!(stats.hits, 0);
    }

    #[test]
    fn body_hash_mismatch_is_an_error() {
        let action = id(3);
        let input = put_frame(1, &action, "OutputID", &id(9), b"hello");
        let (responses, _dir, _) = drive(&input);
        assert!(
            responses[1].err.as_ref().unwrap().contains("OutputID"),
            "{:?}",
            responses[1].err
        );
    }

    #[test]
    fn fresh_hit_does_not_touch_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LocalStore::open(dir.path().to_path_buf()).unwrap();
        let action = id(4);
        let body = b"fresh";
        cache.put(&action, &hashed(body), body).unwrap();
        let action_path = cache.file_name(&action, "a");
        let before = fs::metadata(&action_path).unwrap().modified().unwrap();
        thread::sleep(Duration::from_millis(20));
        cache.get(&action).unwrap();
        let after = fs::metadata(&action_path).unwrap().modified().unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn two_processes_share_one_output_file() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"from-both-processes";
        let output = hashed(body);
        let store = dir.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for action_byte in [7u8, 8] {
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let cache = LocalStore::open(store).unwrap();
                barrier.wait();
                cache.put(&id(action_byte), &output, body).unwrap()
            }));
        }
        let first = handles.pop().unwrap().join().unwrap();
        let second = handles.pop().unwrap().join().unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read(&first).unwrap(), body);
        let cache = LocalStore::open(store).unwrap();
        for action_byte in [7u8, 8] {
            let stored = cache.get(&id(action_byte)).unwrap();
            assert_eq!(stored.path, first);
            assert_eq!(stored.output_id, output);
        }
    }

    #[test]
    fn two_stores_do_not_clobber_an_open_output() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"shared-output";
        let output = hashed(body);
        let action = id(5);
        let first = LocalStore::open(dir.path().to_path_buf()).unwrap();
        let path = first.put(&action, &output, body).unwrap();
        let opened = File::open(&path).unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let dir_path = dir.path().to_path_buf();
        let wait = Arc::clone(&barrier);
        let writer = thread::spawn(move || {
            let second = LocalStore::open(dir_path).unwrap();
            wait.wait();
            second.put(&id(6), &output, body).unwrap()
        });
        barrier.wait();
        let again = writer.join().unwrap();
        assert_eq!(again, path);
        let mut read_back = Vec::new();
        opened.take(64).read_to_end(&mut read_back).unwrap();
        assert_eq!(read_back, body);
    }

    #[test]
    fn cache_dir_from_env_and_default_location() {
        let dir = tempfile::tempdir().unwrap();
        temp_env::with_var(ENV_CACHE_DIR, Some(dir.path()), || {
            assert_eq!(default_go_cacheprog_dir(), dir.path());
        });
        temp_env::with_var_unset(ENV_CACHE_DIR, || {
            let chosen = default_go_cacheprog_dir();
            let base = default_disk_cache_dir();
            assert_eq!(chosen.parent(), base.parent());
            assert_ne!(chosen, base);
            assert!(
                chosen
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with("-go-cacheprog")
            );
        });
    }

    #[test]
    fn capabilities_omit_miss_and_list_commands() {
        let (responses, _dir, _) = drive("");
        assert_eq!(responses.len(), 1);
        let caps = &responses[0];
        assert_eq!(caps.id, 0);
        assert!(caps.err.is_none());
        assert!(!caps.miss);
        let cmds: Vec<&str> = caps
            .known_commands
            .as_ref()
            .unwrap()
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(cmds, ["get", "put", "close"]);
        let raw = serde_json::to_string(&Response::capabilities()).unwrap();
        assert!(!raw.contains("Miss"));
        assert!(!raw.contains("ObjectID"));
    }
}
