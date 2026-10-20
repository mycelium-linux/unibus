//! Validate one bounded range response before writing its bytes to disk.
use anyhow::{Result, bail, ensure};

pub const MAX_HEADER_BYTES: usize = 4096;

pub struct RangeFrame {
    header: Vec<u8>,
    size: u64,
    offset: u64,
    length: u64,
    remaining: Option<u64>,
}

impl RangeFrame {
    pub fn new(size: u64, offset: u64, length: u64) -> Result<Self> {
        ensure!(
            length > 0 && offset.checked_add(length).is_some_and(|end| end <= size),
            "invalid range"
        );
        Ok(Self {
            header: Vec::new(),
            size,
            offset,
            length,
            remaining: None,
        })
    }

    /// Return only validated payload bytes. Header fragments never reach disk.
    pub fn push<'a>(&mut self, chunk: &'a [u8]) -> Result<&'a [u8]> {
        let bytes = if self.remaining.is_none() {
            let newline = chunk.iter().position(|b| *b == b'\n');
            let end = newline.unwrap_or(chunk.len());
            ensure!(
                self.header.len() + end <= MAX_HEADER_BYTES,
                "response header exceeds bound"
            );
            self.header.extend_from_slice(&chunk[..end]);
            let Some(end) = newline else {
                return Ok(&chunk[..0]);
            };
            let header: serde_json::Value = serde_json::from_slice(&self.header)?;
            if header["ok"].as_bool() != Some(true) {
                bail!(
                    "{}",
                    header["error"]
                        .as_str()
                        .unwrap_or("file provider denied the range")
                );
            }
            ensure!(
                header["size"].as_u64() == Some(self.size)
                    && header["offset"].as_u64() == Some(self.offset)
                    && header["length"].as_u64() == Some(self.length),
                "provider range metadata differs"
            );
            self.remaining = Some(self.length);
            &chunk[end + 1..]
        } else {
            chunk
        };
        let remaining = self.remaining.as_mut().expect("header validated");
        ensure!(
            bytes.len() as u64 <= *remaining,
            "provider range exceeds expected length"
        );
        *remaining -= bytes.len() as u64;
        Ok(bytes)
    }

    pub fn finish(&self) -> Result<()> {
        ensure!(self.remaining == Some(0), "provider range ended early");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_header_and_binary_payload() {
        let mut frame = RangeFrame::new(10, 7, 3).unwrap();
        assert!(
            frame
                .push(br#"{"ok":true,"size":10,"offset":7,"length":3}"#)
                .unwrap()
                .is_empty()
        );
        assert_eq!(frame.push(b"\n\0\n").unwrap(), b"\0\n");
        assert_eq!(frame.push(b"\xff").unwrap(), b"\xff");
        frame.finish().unwrap();
        assert!(frame.push(b"extra").is_err());
    }
    #[test]
    fn rejects_wrong_metadata_short_data_and_error_payload() {
        let mut frame = RangeFrame::new(10, 7, 3).unwrap();
        assert!(
            frame
                .push(b"{\"ok\":true,\"size\":10,\"offset\":0,\"length\":3}\nabc")
                .is_err()
        );
        let mut frame = RangeFrame::new(10, 7, 3).unwrap();
        frame
            .push(b"{\"ok\":true,\"size\":10,\"offset\":7,\"length\":3}\na")
            .unwrap();
        assert!(frame.finish().is_err());
        let mut frame = RangeFrame::new(10, 7, 3).unwrap();
        assert!(
            frame
                .push(b"{\"ok\":false,\"error\":\"expired\"}\nabc")
                .is_err()
        );
    }
    #[test]
    fn rejects_oversized_header_and_integer_overflow() {
        let mut frame = RangeFrame::new(1, 0, 1).unwrap();
        assert!(frame.push(&vec![b'x'; MAX_HEADER_BYTES + 1]).is_err());
        assert!(RangeFrame::new(u64::MAX, u64::MAX, 1).is_err());
    }
}
