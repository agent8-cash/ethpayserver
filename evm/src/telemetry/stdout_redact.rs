//! Redaction for the process's own log stream.
//!
//! The Sentry hooks scrub what is sent to Sentry; they cannot scrub what the
//! process writes to stdout, which is what the container log keeps. Sentry
//! payloads are a known set of shapes. A log line is whatever any `tracing`
//! call anywhere (including dependencies) formats, so this does not try to
//! parse it: it redacts the *finished bytes* of each line with
//! [`redact_secrets`], the same rules the Sentry path uses.
//!
//! Cost and failure modes:
//! - A line that is not valid UTF-8 is converted lossily (invalid bytes become
//!   U+FFFD) before redaction, so nothing unredacted survives a parse failure.
//! - Redaction is synchronous on the emitting thread: every event pays one
//!   buffer copy plus a regex pass per rule (a cheap `is_match` first, so a
//!   line with nothing secret-shaped is not rewritten). There is no queue, so
//!   nothing is dropped or reordered under load; the price is latency on the
//!   thread that logs, proportional to line length.
//! - Rules err towards over-redaction, so a long run of lowercase words in a
//!   log line is replaced even if it is not a mnemonic.
//! - A writer buffers until `flush`/drop, so a secret split across several
//!   `write` calls is still seen whole. `fmt` writes one event per writer.

use std::io::{self, Write};

use tracing_subscriber::fmt::MakeWriter;

use super::redact_secrets;

/// A [`MakeWriter`] whose writers redact everything before it reaches `inner`.
pub struct RedactingMakeWriter<M> {
    inner: M,
}

impl<M> RedactingMakeWriter<M> {
    pub fn new(inner: M) -> Self {
        Self { inner }
    }
}

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for RedactingMakeWriter<M> {
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter::new(self.inner.make_writer())
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        RedactingWriter::new(self.inner.make_writer_for(meta))
    }
}

/// Buffers writes and emits them redacted on `flush` or drop.
pub struct RedactingWriter<W: Write> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: Write> RedactingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            buf: Vec::new(),
        }
    }

    fn emit(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&self.buf);
        let redacted = redact_secrets(&text);
        self.buf.clear();
        self.inner.write_all(redacted.as_bytes())
    }
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.emit()?;
        self.inner.flush()
    }
}

impl<W: Write> Drop for RedactingWriter<W> {
    fn drop(&mut self) {
        let _ = self.emit();
    }
}
