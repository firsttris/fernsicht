//! Minimal little-endian cursor helpers.

use crate::{DecodeError, Kind, MAGIC, VERSION};

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let (head, rest) = self
            .buf
            .split_first_chunk::<N>()
            .ok_or(DecodeError::Truncated)?;
        self.buf = rest;
        Ok(*head)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take()?))
    }

    pub(crate) fn rest(self) -> &'a [u8] {
        self.buf
    }
}

/// Writes into a buffer whose length was checked by the caller; panics on
/// overflow, which would be a bug in the fixed `LEN` constants.
pub(crate) struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn put(&mut self, bytes: &[u8]) {
        self.buf[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
        self.pos += bytes.len();
    }

    pub(crate) fn prefix(&mut self, kind: Kind, flags: u8) {
        self.put(&[MAGIC, VERSION, kind as u8, flags]);
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.put(&[v]);
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.put(&v.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.put(&v.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.put(&v.to_le_bytes());
    }

    pub(crate) fn bytes(&mut self, v: &[u8]) {
        self.put(v);
    }
}
