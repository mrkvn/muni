//! Test-only fake `parakeet-sidecar`: speaks the wire protocol without ONNX or
//! the 583 MB model, so the `ParakeetClient` can be exercised in CI. Mirrors
//! the framing in `crates/parakeet-sidecar` and `src/parakeet.rs`: writes a
//! `READY` frame, then replies to each request frame with a canned response
//! (`infer_ms = 7`, text `"fake transcript"`).
//!
//! If argv[1] (the "model dir" path) contains the substring `oneshot`, the
//! process exits after a single response — used to drive the client's
//! crash → lazy-respawn path deterministically.

use std::io::{self, Read, Write};

fn read_frame(reader: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match reader.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    reader.read_exact(&mut buf)?;
    Ok(Some(buf))
}

fn write_frame(writer: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    writer.write_all(&(payload.len() as u32).to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()
}

fn main() -> io::Result<()> {
    let oneshot = std::env::args()
        .nth(1)
        .is_some_and(|a| a.contains("oneshot"));

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    write_frame(&mut writer, b"READY")?;

    while read_frame(&mut reader)?.is_some() {
        let mut response = 7u32.to_le_bytes().to_vec();
        response.extend_from_slice(b"fake transcript");
        write_frame(&mut writer, &response)?;
        if oneshot {
            break;
        }
    }
    Ok(())
}
