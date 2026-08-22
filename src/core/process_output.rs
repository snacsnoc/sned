//! Bounded child-process output capture helpers.

use std::io::{self, Read};

pub const READ_CHUNK_BYTES: usize = 32 * 1024;
pub const MAX_CONFIGURED_OUTPUT_LIMIT: usize = 64 * 1024 * 1024;

pub fn configured_output_limit(env_var: &str, default: usize) -> usize {
    std::env::var(env_var)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| value > 0 && value <= MAX_CONFIGURED_OUTPUT_LIMIT)
        .unwrap_or(default)
}

#[derive(Debug, Default)]
pub struct CapturedOutput {
    bytes: Vec<u8>,
    total_bytes: u64,
}

impl CapturedOutput {
    pub fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(READ_CHUNK_BYTES)),
            total_bytes: 0,
        }
    }

    pub fn is_truncated(&self, limit: usize) -> bool {
        self.total_bytes > limit as u64
    }

    pub fn retained_len(&self) -> usize {
        self.bytes.len()
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn display(&self, limit: usize, stream: &str) -> String {
        let mut text = String::from_utf8_lossy(&self.bytes).into_owned();
        if let Some(marker) = self.truncation_marker(limit, stream) {
            text.push_str("\n\n");
            text.push_str(&marker);
        }
        text
    }

    pub fn truncation_marker(&self, limit: usize, stream: &str) -> Option<String> {
        self.is_truncated(limit).then(|| {
            format!(
                "({stream} output truncated after retaining {} of {} bytes.)",
                self.bytes.len(),
                self.total_bytes
            )
        })
    }

    pub fn into_display_bytes(self, limit: usize, stream: &str) -> Vec<u8> {
        self.display(limit, stream).into_bytes()
    }

    fn push(&mut self, chunk: &[u8], limit: usize) -> usize {
        self.total_bytes = self.total_bytes.saturating_add(chunk.len() as u64);
        let retained = chunk.len().min(limit.saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&chunk[..retained]);
        retained
    }
}

pub async fn capture_async<R>(mut reader: R, limit: usize) -> io::Result<CapturedOutput>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut captured = CapturedOutput::new(limit);
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(captured);
        }
        captured.push(&chunk[..read], limit);
    }
}

pub fn capture_sync<R, F>(
    mut reader: R,
    limit: usize,
    mut on_retained_chunk: F,
) -> io::Result<CapturedOutput>
where
    R: Read,
    F: FnMut(&[u8]),
{
    let mut captured = CapturedOutput::new(limit);
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(captured);
        }
        let retained = captured.push(&chunk[..read], limit);
        if retained > 0 {
            on_retained_chunk(&chunk[..retained]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn async_capture_retains_a_limit_and_drains() {
        use tokio::io::AsyncWriteExt;

        let (mut writer, reader) = tokio::io::duplex(64);
        let writer_task = tokio::spawn(async move {
            writer.write_all(&vec![b'x'; 8 * 1024]).await.unwrap();
            writer.shutdown().await.unwrap();
        });

        let output = capture_async(reader, 1024).await.unwrap();
        writer_task.await.unwrap();
        assert!(output.is_truncated(1024));
        assert!(
            output
                .display(1024, "stdout")
                .contains("retaining 1024 of 8192 bytes")
        );
    }

    #[test]
    fn sync_capture_retains_a_limit_and_reports_retained_chunks() {
        let input = vec![b'x'; 8 * 1024];
        let mut callback_bytes = 0usize;
        let output = capture_sync(Cursor::new(input), 1024, |chunk| {
            callback_bytes += chunk.len();
        })
        .unwrap();

        assert_eq!(callback_bytes, 1024);
        assert!(output.is_truncated(1024));
        assert!(
            output
                .display(1024, "stderr")
                .contains("retaining 1024 of 8192 bytes")
        );
    }
}
