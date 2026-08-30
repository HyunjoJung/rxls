//! Bounded standard-I/O reader for newline-delimited MCP messages.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

/// Maximum bytes accepted in one newline-delimited JSON-RPC message.
pub const MAX_RPC_LINE_BYTES: usize = 1024 * 1024;

/// Async reader that rejects an input line once it crosses a byte limit.
///
/// `rmcp` frames standard-I/O messages with newlines. This wrapper prevents an
/// unterminated or oversized line from growing the transport buffer without a
/// bound.
#[derive(Debug)]
pub struct BoundedLineReader<R> {
    inner: R,
    limit: usize,
    current_line_bytes: usize,
}

impl<R> BoundedLineReader<R> {
    /// Wrap `inner` with the default MCP line limit.
    pub fn new(inner: R) -> Self {
        Self::with_limit(inner, MAX_RPC_LINE_BYTES)
    }

    /// Wrap `inner` with an explicit per-line byte limit.
    pub fn with_limit(inner: R, limit: usize) -> Self {
        Self {
            inner,
            limit,
            current_line_bytes: 0,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for BoundedLineReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut scratch = [0u8; 8192];
        let capacity = output.remaining().min(scratch.len());
        let mut scratch_buf = ReadBuf::new(&mut scratch[..capacity]);
        match Pin::new(&mut self.inner).poll_read(cx, &mut scratch_buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {
                let bytes = scratch_buf.filled();
                let mut line_bytes = self.current_line_bytes;
                for byte in bytes {
                    if *byte == b'\n' {
                        line_bytes = 0;
                    } else {
                        line_bytes = line_bytes.saturating_add(1);
                        if line_bytes > self.limit {
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "MCP JSON-RPC line exceeds the configured byte limit",
                            )));
                        }
                    }
                }
                self.current_line_bytes = line_bytes;
                output.put_slice(bytes);
                Poll::Ready(Ok(()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn accepts_multiple_bounded_lines() {
        let (mut writer, reader) = tokio::io::duplex(64);
        writer.write_all(b"1234\n12\n").await.unwrap();
        drop(writer);
        let mut reader = BoundedLineReader::with_limit(reader, 4);
        let mut output = Vec::new();
        reader.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, b"1234\n12\n");
    }

    #[tokio::test]
    async fn rejects_an_oversized_line() {
        let (mut writer, reader) = tokio::io::duplex(64);
        writer.write_all(b"12345\n").await.unwrap();
        drop(writer);
        let mut reader = BoundedLineReader::with_limit(reader, 4);
        let mut output = Vec::new();
        let error = reader.read_to_end(&mut output).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
