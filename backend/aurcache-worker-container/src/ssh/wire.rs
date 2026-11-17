//! The SSH wire encoding (RFC 4251, section 5): what keys and agent
//! messages are made of.

/// Reads one value after another from a message.
pub struct Reader<'a> {
    rest: &'a [u8],
}

/// A message that ended early or held something it should not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Truncated;

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    pub fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }

    pub fn bytes(&mut self, len: usize) -> Result<&'a [u8], Truncated> {
        if self.rest.len() < len {
            return Err(Truncated);
        }
        let (taken, rest) = self.rest.split_at(len);
        self.rest = rest;
        Ok(taken)
    }

    pub fn byte(&mut self) -> Result<u8, Truncated> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u32(&mut self) -> Result<u32, Truncated> {
        let bytes = self.bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub fn string(&mut self) -> Result<&'a [u8], Truncated> {
        let len = usize::try_from(self.u32()?).map_err(|_| Truncated)?;
        self.bytes(len)
    }

    /// A non-negative `mpint`, as its magnitude: big-endian, no leading
    /// zero byte.
    pub fn mpint(&mut self) -> Result<&'a [u8], Truncated> {
        let bytes = self.string()?;
        if bytes.first().is_some_and(|first| first & 0x80 != 0) {
            // Negative: nothing in a key is.
            return Err(Truncated);
        }
        let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
        Ok(&bytes[start..])
    }
}

/// Builds a message one value after another.
#[derive(Default)]
pub struct Writer {
    out: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn byte(&mut self, byte: u8) -> &mut Self {
        self.out.push(byte);
        self
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.out.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn string(&mut self, bytes: &[u8]) -> &mut Self {
        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        self.u32(len);
        self.out.extend_from_slice(bytes);
        self
    }

    /// A non-negative integer given as its big-endian magnitude.
    pub fn mpint(&mut self, magnitude: &[u8]) -> &mut Self {
        let start = magnitude
            .iter()
            .position(|&b| b != 0)
            .unwrap_or(magnitude.len());
        let magnitude = &magnitude[start..];
        if magnitude.first().is_some_and(|first| first & 0x80 != 0) {
            let mut padded = Vec::with_capacity(magnitude.len() + 1);
            padded.push(0);
            padded.extend_from_slice(magnitude);
            self.string(&padded)
        } else {
            self.string(magnitude)
        }
    }

    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4251's own examples.
    #[test]
    fn mpints_are_written_as_the_rfc_says() {
        let written = |magnitude: &[u8]| Writer::new().mpint(magnitude).finish();
        assert_eq!(written(&[]), [0, 0, 0, 0]);
        assert_eq!(written(&[0, 0]), [0, 0, 0, 0]);
        assert_eq!(
            written(&[0x09, 0xa3, 0x78, 0xf9, 0xb2, 0xe3, 0x32, 0xa7]),
            [0, 0, 0, 8, 0x09, 0xa3, 0x78, 0xf9, 0xb2, 0xe3, 0x32, 0xa7]
        );
        assert_eq!(written(&[0x80]), [0, 0, 0, 2, 0, 0x80]);
    }

    #[test]
    fn what_is_written_reads_back() {
        let bytes = Writer::new()
            .byte(13)
            .string(b"key")
            .mpint(&[0xff, 1])
            .u32(4)
            .finish();
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.byte(), Ok(13));
        assert_eq!(reader.string(), Ok(&b"key"[..]));
        assert_eq!(reader.mpint(), Ok(&[0xff, 1][..]));
        assert_eq!(reader.u32(), Ok(4));
        assert!(reader.is_empty());
        assert_eq!(reader.u32(), Err(Truncated));
    }

    #[test]
    fn a_string_longer_than_its_message_is_truncated() {
        let mut reader = Reader::new(&[0, 0, 0, 9, 1, 2]);
        assert_eq!(reader.string(), Err(Truncated));
        let mut negative = Reader::new(&[0, 0, 0, 1, 0x80]);
        assert_eq!(negative.mpint(), Err(Truncated));
    }
}
