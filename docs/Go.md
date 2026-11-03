# Go

Go 1.24 and later can store the build cache in an external program named by `GOCACHEPROG`. `sccache --go-cacheprog` speaks that protocol.

Go starts the program, writes JSON requests to its stdin, and reads JSON responses from its stdout. sccache writes each cached object into a local directory and returns a filesystem path, which Go opens directly. This does not start the sccache server and does not use a configured storage backend. It exists to inspect the protocol and the get/put traffic from a real `go` build before a remote backend is wired up.

```bash
export SCCACHE_GO_CACHEPROG_DIR="$HOME/.cache/sccache-go"
export GOCACHEPROG="sccache --go-cacheprog"
go build ./...
```

`SCCACHE_GO_CACHEPROG_DIR` selects the store. When it is unset, sccache uses a sibling of the ordinary disk cache: `~/.cache/sccache-go-cacheprog` on Linux, `~/Library/Caches/Mozilla.sccache-go-cacheprog` on macOS, and `%LOCALAPPDATA%\Mozilla\sccache-go-cacheprog` on Windows. The store is kept out of the disk cache directory because that cache deletes files under its root that it did not write.

`SCCACHE_LOG=debug` logs each request and response on stderr. Go forwards the program's stderr, so the lines show up in the build output. `SCCACHE_LOG=trace` also logs the raw request JSON. `SCCACHE_LOG=info` logs the store path and a one-line summary when Go closes the program. Object bodies are not logged.

The layout matches Go's own `GOCACHE`: `<hh>/<action hex>-a` is one action record (`v1`, the action id, the output id, the size, and the time), and `<hh>/<output hex>-d` is the body. A body is written once and left in place if a later put names the same output, so a path Go already received keeps its bytes. The store is not trimmed, and `go clean -cache` does not delete it: the protocol has no delete command. Paths returned to Go stay valid at least until Go sends `close` at the end of the process. A get that cannot read the store is reported as a miss. A put that cannot write it is reported as an error. Either way the program keeps running; only a request that is not valid JSON exits it.

`get`, `put`, and `close` are the supported commands. A `put` may name the output hash `OutputID`, or `ObjectID`, which is the name Go used before 1.24. If both are present, `OutputID` is the one that is stored.
