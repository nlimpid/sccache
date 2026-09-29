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

//! Go `GOCACHEPROG` protocol handler (debug / local stub).
//!
//! Go spawns this process and speaks newline-delimited JSON on stdin/stdout.
//! See `cmd/go/internal/cacheprog`. This module is intentionally separate from
//! the compiler-wrapper path: Go does not invoke sccache as `cc`.

use crate::config::default_disk_cache_dir;
use crate::errors::*;
use crate::util;
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

const ENV_CACHE_DIR: &str = "SCCACHE_GO_CACHEPROG_DIR";

/// Request from `go` → child (one JSON object per non-blank stdin line).
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Request {
    #[serde(rename = "ID")]
    pub id: i64,
    #[serde(rename = "Command")]
    pub command: String,
    #[serde(default, rename = "ActionID", with = "b64_bytes_opt")]
    pub action_id: Option<Vec<u8>>,
    /// Preferred body hash key (Go 1.24+).
    #[serde(default, rename = "OutputID", with = "b64_bytes_opt")]
    pub output_id: Option<Vec<u8>>,
    /// Deprecated alias still sent by Go 1.24; removed in later releases.
    #[serde(default, rename = "ObjectID", with = "b64_bytes_opt")]
    pub object_id: Option<Vec<u8>>,
    #[serde(default, rename = "BodySize")]
    pub body_size: i64,
}

impl Request {
    pub fn effective_output_id(&self) -> Option<&[u8]> {
        self.output_id
            .as_deref()
            .or(self.object_id.as_deref())
            .filter(|b| !b.is_empty())
    }
}

/// Response from child → `go`.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Response {
    #[serde(rename = "ID")]
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none", rename = "Err")]
    pub err: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "KnownCommands")]
    pub known_commands: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not", rename = "Miss")]
    pub miss: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "OutputID",
        with = "b64_bytes_opt"
    )]
    pub output_id: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "Size")]
    pub size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "DiskPath")]
    pub disk_path: Option<String>,
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
            disk_path: None,
        }
    }

    fn hit(id: i64, output_id: Vec<u8>, size: i64, disk_path: PathBuf) -> Self {
        Self {
            id,
            err: None,
            known_commands: None,
            miss: false,
            output_id: Some(output_id),
            size: Some(size),
            disk_path: Some(disk_path.to_string_lossy().into_owned()),
        }
    }

    fn put_ok(id: i64, disk_path: PathBuf) -> Self {
        Self {
            id,
            err: None,
            known_commands: None,
            miss: false,
            output_id: None,
            size: None,
            disk_path: Some(disk_path.to_string_lossy().into_owned()),
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
            disk_path: None,
        }
    }
}

mod b64_bytes_opt {
    use super::*;

    #[allow(clippy::ref_option)]
    pub fn serialize<S: Serializer>(
        value: &Option<Vec<u8>>,
        serializer: S,
    ) -> StdResult<S::Ok, S::Error> {
        match value {
            Some(bytes) => serializer.serialize_str(&B64.encode(bytes)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> StdResult<Option<Vec<u8>>, D::Error> {
        let opt = Option::<String>::deserialize(deserializer)?;
        match opt {
            None => Ok(None),
            Some(s) if s.is_empty() => Ok(None),
            Some(s) => B64
                .decode(s.as_bytes())
                .map(Some)
                .map_err(serde::de::Error::custom),
        }
    }
}

use std::result::Result as StdResult;

#[derive(Debug, Serialize, Deserialize)]
struct EntryMeta {
    output_id: String,
    size: i64,
}

/// Local on-disk store used by the debug handler (not the main sccache Storage trait).
pub struct LocalGoCache {
    dir: PathBuf,
}

impl LocalGoCache {
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating go-cacheprog dir {}", dir.display()))?;
        Ok(Self { dir })
    }

    fn paths_for(&self, action_id: &[u8]) -> (PathBuf, PathBuf) {
        let key = util::hex(action_id);
        let body = self.dir.join(&key);
        let meta = self.dir.join(format!("{key}.meta"));
        (body, meta)
    }

    pub fn get(&self, action_id: &[u8]) -> Result<Option<(Vec<u8>, i64, PathBuf)>> {
        let (body, meta_path) = self.paths_for(action_id);
        if !body.is_file() || !meta_path.is_file() {
            return Ok(None);
        }
        let meta: EntryMeta = serde_json::from_slice(&std::fs::read(&meta_path)?)?;
        let output_id = B64
            .decode(meta.output_id.as_bytes())
            .context("invalid OutputID in go-cacheprog meta")?;
        Ok(Some((output_id, meta.size, body.canonicalize()?)))
    }

    pub fn put(&self, action_id: &[u8], output_id: &[u8], body: &[u8]) -> Result<PathBuf> {
        let (body_path, meta_path) = self.paths_for(action_id);
        let tmp = body_path.with_extension("tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &body_path)?;
        let meta = EntryMeta {
            output_id: B64.encode(output_id),
            size: body.len() as i64,
        };
        std::fs::write(&meta_path, serde_json::to_vec(&meta)?)?;
        Ok(body_path.canonicalize()?)
    }
}

pub fn default_go_cacheprog_dir() -> PathBuf {
    std::env::var_os(ENV_CACHE_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(|| default_disk_cache_dir().join("go-cacheprog"))
}

/// Run the GOCACHEPROG loop until `close` or stdin EOF.
pub fn run(cache_dir: PathBuf) -> Result<()> {
    let cache = LocalGoCache::new(cache_dir)?;
    info!(
        "go-cacheprog: listening on stdin; local store {}",
        cache.dir.display()
    );

    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    write_response(&mut stdout, &Response::capabilities())?;

    loop {
        let Some(line) = read_nonblank_line(&mut stdin)? else {
            break;
        };
        let req: Request = serde_json::from_str(&line)
            .with_context(|| format!("parsing GOCACHEPROG request: {line}"))?;
        debug!(
            "go-cacheprog req id={} cmd={} action_id={} body_size={}",
            req.id,
            req.command,
            req.action_id
                .as_ref()
                .map(|a| util::hex(a))
                .unwrap_or_else(|| "-".into()),
            req.body_size
        );

        let resp = match req.command.as_str() {
            "get" => handle_get(&cache, &req)?,
            "put" => handle_put(&cache, &req, &mut stdin)?,
            "close" => {
                write_response(&mut stdout, &Response::close_ok(req.id))?;
                info!("go-cacheprog: close");
                break;
            }
            other => Response::error(req.id, format!("unsupported command {other}")),
        };

        debug!(
            "go-cacheprog resp id={} miss={} err={:?} disk_path={:?}",
            resp.id, resp.miss, resp.err, resp.disk_path
        );
        write_response(&mut stdout, &resp)?;
    }
    Ok(())
}

fn handle_get(cache: &LocalGoCache, req: &Request) -> Result<Response> {
    let Some(action_id) = req.action_id.as_deref() else {
        return Ok(Response::error(req.id, "get missing ActionID"));
    };
    match cache.get(action_id)? {
        Some((output_id, size, path)) => Ok(Response::hit(req.id, output_id, size, path)),
        None => Ok(Response::miss(req.id)),
    }
}

fn handle_put<R: BufRead>(cache: &LocalGoCache, req: &Request, stdin: &mut R) -> Result<Response> {
    let Some(action_id) = req.action_id.as_deref() else {
        return Ok(Response::error(req.id, "put missing ActionID"));
    };
    let Some(output_id) = req.effective_output_id() else {
        return Ok(Response::error(req.id, "put missing OutputID"));
    };

    let body = if req.body_size > 0 {
        let Some(body_line) = read_nonblank_line(stdin)? else {
            return Ok(Response::error(req.id, "put missing body"));
        };
        // Go writes: '"' + std base64 + '"' + '\n' (a JSON string of base64).
        let b64: String = serde_json::from_str(&body_line)
            .with_context(|| format!("parsing put body line for id {}", req.id))?;
        let bytes = B64
            .decode(b64.as_bytes())
            .context("decoding put body base64")?;
        if bytes.len() as i64 != req.body_size {
            return Ok(Response::error(
                req.id,
                format!(
                    "put body size mismatch: got {} expected {}",
                    bytes.len(),
                    req.body_size
                ),
            ));
        }
        bytes
    } else {
        Vec::new()
    };

    let path = cache.put(action_id, output_id, &body)?;
    Ok(Response::put_ok(req.id, path))
}

fn write_response(out: &mut impl Write, resp: &Response) -> Result<()> {
    serde_json::to_writer(&mut *out, resp)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

/// `go` writes an extra blank line after each JSON request (`json.Encoder` + `\n`).
fn read_nonblank_line(input: &mut impl BufRead) -> Result<Option<String>> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = input.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            if line.ends_with('\n') {
                line.pop();
                if line.ends_with('\r') {
                    line.pop();
                }
            }
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parse_get_request() {
        let line = r#"{"ID":1,"Command":"get","ActionID":"AAAA"}"#;
        // "AAAA" is valid base64 for 3 zero-ish bytes... actually AAAA = [0,0,0]
        let req: Request = serde_json::from_str(line).unwrap();
        assert_eq!(req.id, 1);
        assert_eq!(req.command, "get");
        assert_eq!(req.action_id.as_deref(), Some(&[0, 0, 0][..]));
    }

    #[test]
    fn parse_put_with_output_id() {
        let line = r#"{"ID":2,"Command":"put","ActionID":"AQID","OutputID":"BAUG","BodySize":3}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        assert_eq!(req.body_size, 3);
        assert_eq!(req.effective_output_id(), Some(&[4, 5, 6][..]));
    }

    #[test]
    fn parse_put_object_id_fallback() {
        let line = r#"{"ID":2,"Command":"put","ActionID":"AQID","ObjectID":"BAUG","BodySize":0}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        assert_eq!(req.effective_output_id(), Some(&[4, 5, 6][..]));
    }

    #[test]
    fn capabilities_json_shape() {
        let json = serde_json::to_string(&Response::capabilities()).unwrap();
        assert!(json.contains(r#""ID":0"#));
        assert!(json.contains("get"));
        assert!(json.contains("put"));
        assert!(json.contains("close"));
        assert!(!json.contains("Miss"));
    }

    #[test]
    fn skip_blank_lines_then_read_body() {
        // Mimic go's Encode+\n then body JSON string.
        let input = concat!(
            r#"{"ID":2,"Command":"put","ActionID":"AQID","OutputID":"BAUG","BodySize":3}"#,
            "\n\n",
            r#""AQID""#,
            "\n"
        );
        let mut cur = Cursor::new(input.as_bytes());
        let line = read_nonblank_line(&mut cur).unwrap().unwrap();
        let req: Request = serde_json::from_str(&line).unwrap();
        let resp = handle_put(
            &LocalGoCache::new(tempfile::tempdir().unwrap().into_path()).unwrap(),
            &req,
            &mut cur,
        )
        .unwrap();
        assert!(resp.disk_path.is_some());
        assert!(resp.err.is_none());
    }

    #[test]
    fn local_roundtrip_get_put() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LocalGoCache::new(dir.path().to_path_buf()).unwrap();
        let action = b"action-key-16b!";
        let output = b"output-key-16b!!";
        let body = b"hello-go-cache";
        let path = cache.put(action, output, body).unwrap();
        assert!(path.is_file());

        let (oid, size, got_path) = cache.get(action).unwrap().unwrap();
        assert_eq!(oid, output);
        assert_eq!(size, body.len() as i64);
        assert_eq!(std::fs::read(got_path).unwrap(), body);
        assert!(cache.get(b"missing").unwrap().is_none());
    }

    #[test]
    fn protocol_session_get_miss_put_get_hit() {
        let dir = tempfile::tempdir().unwrap();
        let cache = LocalGoCache::new(dir.path().to_path_buf()).unwrap();

        let get_miss: Request =
            serde_json::from_str(r#"{"ID":1,"Command":"get","ActionID":"AQIDBAUGBwg="}"#).unwrap();
        let r = handle_get(&cache, &get_miss).unwrap();
        assert!(r.miss);

        let put_line = concat!(
            r#"{"ID":2,"Command":"put","ActionID":"AQIDBAUGBwg=","OutputID":"CgsMDQ4PEBE=","BodySize":4}"#,
            "\n\n\"aGVsbw==\"\n"
        );
        let mut cur = Cursor::new(put_line.as_bytes());
        let line = read_nonblank_line(&mut cur).unwrap().unwrap();
        let put_req: Request = serde_json::from_str(&line).unwrap();
        let put_resp = handle_put(&cache, &put_req, &mut cur).unwrap();
        assert!(put_resp.disk_path.as_ref().unwrap().len() > 1);

        let r = handle_get(&cache, &get_miss).unwrap();
        assert!(!r.miss);
        assert_eq!(r.size, Some(4));
        assert_eq!(std::fs::read(r.disk_path.unwrap()).unwrap(), b"helo");
    }
}
