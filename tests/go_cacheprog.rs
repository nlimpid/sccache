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

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

static SCCACHE_BIN: LazyLock<&Path> = LazyLock::new(|| Path::new(env!("CARGO_BIN_EXE_sccache")));

#[test]
fn go_cacheprog_capabilities_on_eof() -> Result<()> {
    let dir = tempfile::tempdir().context("tempdir")?;
    let output = Command::new(SCCACHE_BIN.as_os_str())
        .arg("--go-cacheprog")
        .env("SCCACHE_GO_CACHEPROG_DIR", dir.path())
        .env_remove("SCCACHE_LOG")
        .stdin(Stdio::null())
        .output()
        .context("spawn sccache --go-cacheprog")?;
    if !output.status.success() {
        bail!(
            "sccache --go-cacheprog exited {}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8(output.stdout).context("stdout utf-8")?;
    let line = stdout.lines().next().context("missing capabilities")?;
    assert!(line.contains(r#""ID":0"#), "{line}");
    assert!(
        line.contains("get") && line.contains("put") && line.contains("close"),
        "{line}"
    );
    assert!(!line.contains("Miss"), "{line}");
    Ok(())
}

#[test]
fn go_build_hits_on_second_run() -> Result<()> {
    match go_version() {
        GoVersion::TooOldOrMissing(reason) => {
            eprintln!("skipping go cacheprog end-to-end: {reason}");
            Ok(())
        }
        GoVersion::AtLeast124 => go_build_twice(),
    }
}

enum GoVersion {
    AtLeast124,
    TooOldOrMissing(String),
}

fn go_version() -> GoVersion {
    let output = match Command::new("go").arg("version").output() {
        Ok(output) => output,
        Err(err) => return GoVersion::TooOldOrMissing(format!("go not on PATH ({err})")),
    };
    if !output.status.success() {
        return GoVersion::TooOldOrMissing(format!(
            "go version failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(version) = text
        .split_whitespace()
        .find(|word| word.starts_with("go1."))
    else {
        // `devel` builds are newer than the latest release.
        if text.contains("devel") {
            return GoVersion::AtLeast124;
        }
        return GoVersion::TooOldOrMissing(format!("unrecognized `go version` output: {text}"));
    };
    let numbers = version.trim_start_matches("go");
    let mut parts = numbers.split(['.', '-']);
    let major: u32 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let minor: u32 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    if major > 1 || (major == 1 && minor >= 24) {
        GoVersion::AtLeast124
    } else {
        GoVersion::TooOldOrMissing(format!("need Go >= 1.24, found {version}"))
    }
}

fn go_build_twice() -> Result<()> {
    let root = tempfile::tempdir().context("module dir")?;
    let store = root.path().join("prog-store");
    let gocache = root.path().join("gocache");
    fs::create_dir(&gocache)?;
    fs::write(
        root.path().join("go.mod"),
        "module example.com/sccachegocp\n\ngo 1.24\n",
    )?;
    fs::write(
        root.path().join("main.go"),
        r#"package main

import (
	"crypto/sha256"
	"encoding/json"
	"fmt"
)

func main() {
	sum := sha256.Sum256([]byte("sccache"))
	buf, err := json.Marshal(sum)
	if err != nil {
		panic(err)
	}
	fmt.Println(string(buf))
}
"#,
    )?;

    let prog = gocacheprog_value(SCCACHE_BIN.as_ref());
    let first = go_build(root.path(), &store, &gocache, &prog, "out1")?;
    assert!(
        first.status.success(),
        "first go build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let written = action_records(&store)?;
    assert!(
        !written.is_empty(),
        "first build wrote no action records under {}",
        store.display()
    );
    assert!(root.path().join("out1").is_file());

    let second = go_build(root.path(), &store, &gocache, &prog, "out2")?;
    assert!(
        second.status.success(),
        "second go build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    // A rewritten `-a` keeps its name, so a count would not catch a second put.
    // The timestamp is the last field and changes on every write; the action
    // id, output id, and size before it must not.
    let again = action_records(&store)?;
    assert_eq!(
        record_identities(&again),
        record_identities(&written),
        "second build changed action records\nstderr:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(root.path().join("out2").is_file());
    assert_eq!(
        fs::read(root.path().join("out1"))?,
        fs::read(root.path().join("out2"))?
    );
    Ok(())
}

/// Contents of every `v1` action record, keyed by path.
fn action_records(store: &Path) -> Result<std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>> {
    let mut records = std::collections::BTreeMap::new();
    if !store.exists() {
        return Ok(records);
    }
    for shard in fs::read_dir(store).with_context(|| format!("reading {}", store.display()))? {
        let shard = shard?;
        if !shard.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(shard.path())? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_string_lossy().ends_with("-a") {
                records.insert(entry.path(), fs::read(entry.path())?);
            }
        }
    }
    Ok(records)
}

/// The record without its timestamp, which is the last space-separated field.
fn record_identities(
    records: &std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    records
        .iter()
        .map(|(path, bytes)| {
            let end = bytes
                .iter()
                .rposition(|byte| *byte == b' ')
                .unwrap_or(bytes.len());
            (path.clone(), bytes[..end].to_vec())
        })
        .collect()
}

fn gocacheprog_value(bin: &Path) -> String {
    let path = bin.display().to_string();
    if path.contains([' ', '\t']) {
        format!("\"{path}\" --go-cacheprog")
    } else {
        format!("{path} --go-cacheprog")
    }
}

fn go_build(
    module: &Path,
    store: &Path,
    gocache: &Path,
    prog: &str,
    out_name: &str,
) -> Result<std::process::Output> {
    Command::new("go")
        .args(["build", "-o", out_name, "."])
        .current_dir(module)
        .env("GOCACHEPROG", prog)
        .env("SCCACHE_GO_CACHEPROG_DIR", store)
        .env("SCCACHE_LOG", "debug")
        .env("GOCACHE", gocache)
        .env("CGO_ENABLED", "0")
        .env("GOTOOLCHAIN", "local")
        .env("GOPROXY", "off")
        .env("GOSUMDB", "off")
        .env("GO111MODULE", "on")
        .env("GOFLAGS", "")
        .output()
        .context("go build")
}
