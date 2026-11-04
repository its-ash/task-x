//! Line streaming for files too large to load whole: memory stays near one buffer and one line.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Largest file a stream reads.
pub const MAX_BYTES: u64 = 4 << 30;
/// Bytes of one line kept; the rest of a longer line is skipped (it still counts as one line).
pub const MAX_LINE_BYTES: usize = 16 << 20;
const BUF_BYTES: usize = 256 << 10;
const SNIFF_BYTES: usize = 8 << 10;
const CANCEL_EVERY: usize = 4_096;

/// True when the first bytes hold no NUL.
pub fn is_text(path: &Path) -> io::Result<bool> {
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    File::open(path)?.take(SNIFF_BYTES as u64).read_to_end(&mut head)?;
    Ok(!head.contains(&0))
}

/// Calls `f` with each line (1-based number, text without the line end; bad UTF-8 is replaced)
/// until `f` returns false or the file ends. Returns the number of lines read.
pub fn each_line(path: &Path, cancel: &CancellationToken, mut f: impl FnMut(usize, &str) -> bool) -> io::Result<usize> {
    let mut r = BufReader::with_capacity(BUF_BYTES, File::open(path)?);
    let mut line = Vec::new();
    let (mut n, mut open) = (0usize, false);
    let emit = |line: &[u8]| String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(line)).into_owned();
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            if open {
                n += 1;
                f(n, &emit(&line));
            }
            return Ok(n);
        }
        open = true;
        let (take, ends) = chunk.iter().position(|&b| b == b'\n').map_or((chunk.len(), false), |i| (i, true));
        let room = MAX_LINE_BYTES.saturating_sub(line.len());
        line.extend_from_slice(&chunk[..take.min(room)]);
        r.consume(take + usize::from(ends));
        if !ends {
            continue;
        }
        n += 1;
        open = false;
        if !f(n, &emit(&line)) {
            return Ok(n);
        }
        line.clear();
        if n % CANCEL_EVERY == 0 && cancel.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
    }
}
