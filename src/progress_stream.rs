// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright © 2022 Adrian <adrian.eddy at gmail>

use std::io::{ Read, Write, Seek, Result, SeekFrom };

pub struct ProgressStream<R: Read + Write + Seek, C: FnMut(usize)> {
    inner: R,
    callback: C,
    total: usize
}
impl<R: Read + Write + Seek, C: FnMut(usize)> ProgressStream<R, C> {
    pub fn new(inner: R, callback: C) -> Self {
        Self { inner, callback, total: 0 }
    }
}
impl<R: Read + Write + Seek, C: FnMut(usize)> Read for ProgressStream<R, C> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let read = self.inner.read(buf)?;
        self.total += read;
        (self.callback)(self.total);
        Ok(read)
    }
}
impl<R: Read + Write + Seek, C: FnMut(usize)> Seek for ProgressStream<R, C> {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> { self.inner.seek(pos) }
}
impl<R: Read + Write + Seek, C: FnMut(usize)> Write for ProgressStream<R, C> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let written = self.inner.write(buf)?;
        self.total += written;
        (self.callback)(self.total);
        Ok(written)
    }
    fn flush(&mut self) -> Result<()> { self.inner.flush() }
}
