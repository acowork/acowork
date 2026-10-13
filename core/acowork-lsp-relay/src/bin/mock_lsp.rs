//! Byte-exact stdin → stdout echo — the test stand-in for a real LSP server.
//!
//! `acowork-lsp-relay`'s process pool needs a child process it can pipe
//! JSON-RPC frames into and read the same bytes back (`src/pool.rs` frames
//! both directions with `Content-Length`, so a pure copier is enough to drive
//! the relay tasks). Until now that fixture was the POSIX `cat` — a program
//! that does not exist on a plain Windows install, so every pool test failed
//! with "program not found" while Linux CI stayed green. This binary is the
//! portable equivalent.
//!
//! It is a **test fixture**, not a product binary: no Gateway, Node or Runtime
//! code spawns it, and it is exercised by `tests/pool_process.rs` through
//! `env!("CARGO_BIN_EXE_mock_lsp")`.
//!
//! Usage: `mock_lsp [--exit-now] [anything else…]`
//!
//! - `--exit-now` — exit immediately without reading stdin. Models an LSP
//!   server that dies on its own, which is how the pool's stale-entry path
//!   gets exercised (the old `echo` invocation).
//! - Any other argument is ignored on purpose: the pool keys entries by
//!   `(command, args, workspace)`, and the test needs two invocations that
//!   differ only by argument to still run.

use std::io::{self, Read, Write};

fn main() -> io::Result<()> {
    if std::env::args().any(|arg| arg == "--exit-now") {
        return Ok(());
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let mut buf = [0u8; 4096];

    loop {
        match input.read(&mut buf) {
            // EOF: the relay closed our stdin (or it was killed), so there is
            // nothing left to echo.
            Ok(0) => return Ok(()),
            Ok(n) => {
                output.write_all(&buf[..n])?;
                // Flushing per chunk is the whole point: otherwise the echo
                // waits for a full buffer and the pool's stdout reader blocks
                // until the process exits.
                output.flush()?;
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // Broken pipe means the pool went away mid-write — nothing to do.
            Err(ref e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}
