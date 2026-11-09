//! The page store abstraction and the two shipped implementations.

use alloc::vec::Vec;
use core::fmt;

/// A sequence of fixed-size pages, with page N at byte offset N × page size.
///
/// The tree keeps the store dense: its length is always exactly the pages in use, and a
/// zero-length store is a valid empty tree.
pub trait PageStore {
    /// The store's error type, returned inside [`Error::Store`](crate::Error::Store).
    type Error;

    /// Page size in bytes. Must not change for the life of the store.
    fn page_size(&self) -> usize;

    /// Number of pages in the store.
    fn page_count(&self) -> Result<u32, Self::Error>;

    /// Reads page `index` (always below `page_count`) into `buf`, which is one page long.
    fn read_page(&self, index: u32, buf: &mut [u8]) -> Result<(), Self::Error>;

    /// Writes one page. `index` is at most `page_count`; writing at `page_count` appends a page.
    fn write_page(&mut self, index: u32, buf: &[u8]) -> Result<(), Self::Error>;

    /// Shrinks the store to `count` pages (never more than `page_count`).
    fn truncate(&mut self, count: u32) -> Result<(), Self::Error>;
}

/// Errors from [`MemStore`]. Each one means the caller broke the [`PageStore`] contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemStoreError {
    /// A page index or count past the end of the store.
    OutOfBounds,
    /// The byte length is not a whole number of pages.
    PartialPage,
    /// A buffer that is not exactly one page long.
    BufferSize,
}

impl fmt::Display for MemStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            MemStoreError::OutOfBounds => "page index out of bounds",
            MemStoreError::PartialPage => "store length is not a whole number of pages",
            MemStoreError::BufferSize => "buffer is not one page long",
        })
    }
}

impl core::error::Error for MemStoreError {}

/// A page store held in memory.
///
/// Atomic by construction, provided a call that returns an error is treated as poisoning the
/// tree: discard it and reopen from a copy of the store taken before the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemStore {
    page_size: usize,
    bytes: Vec<u8>,
}

impl MemStore {
    /// An empty store (an empty tree) with the given page size.
    pub fn new(page_size: usize) -> Self {
        Self::from_bytes(page_size, Vec::new())
    }

    /// A store over existing bytes, such as a snapshot taken with [`as_bytes`](Self::as_bytes).
    pub fn from_bytes(page_size: usize, bytes: Vec<u8>) -> Self {
        Self { page_size, bytes }
    }

    /// The store's bytes: page N at offset N × page size.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the store and returns its bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Byte range of page `index`, if the page exists.
    fn page_range(&self, index: u32) -> Option<core::ops::Range<usize>> {
        let start = (index as usize).checked_mul(self.page_size)?;
        let end = start.checked_add(self.page_size)?;
        (end <= self.bytes.len()).then_some(start..end)
    }
}

impl PageStore for MemStore {
    type Error = MemStoreError;

    fn page_size(&self) -> usize {
        self.page_size
    }

    fn page_count(&self) -> Result<u32, MemStoreError> {
        if self.page_size == 0 || !self.bytes.len().is_multiple_of(self.page_size) {
            return Err(MemStoreError::PartialPage);
        }
        u32::try_from(self.bytes.len() / self.page_size).map_err(|_| MemStoreError::OutOfBounds)
    }

    fn read_page(&self, index: u32, buf: &mut [u8]) -> Result<(), MemStoreError> {
        if buf.len() != self.page_size {
            return Err(MemStoreError::BufferSize);
        }
        let range = self.page_range(index).ok_or(MemStoreError::OutOfBounds)?;
        buf.copy_from_slice(&self.bytes[range]);
        Ok(())
    }

    fn write_page(&mut self, index: u32, buf: &[u8]) -> Result<(), MemStoreError> {
        if buf.len() != self.page_size {
            return Err(MemStoreError::BufferSize);
        }
        if let Some(range) = self.page_range(index) {
            self.bytes[range].copy_from_slice(buf);
            Ok(())
        } else if index == self.page_count()? {
            self.bytes.extend_from_slice(buf);
            Ok(())
        } else {
            Err(MemStoreError::OutOfBounds)
        }
    }

    fn truncate(&mut self, count: u32) -> Result<(), MemStoreError> {
        if count > self.page_count()? {
            return Err(MemStoreError::OutOfBounds);
        }
        self.bytes.truncate(count as usize * self.page_size);
        Ok(())
    }
}

#[cfg(feature = "std")]
pub use file::FileStore;

#[cfg(feature = "std")]
mod file {
    use std::fs::File;
    use std::io;

    use super::PageStore;

    /// A page store over a file, using positioned reads and writes plus `set_len`.
    ///
    /// Not crash-safe: a call that writes several pages can be torn by a crash, and nothing is
    /// synced. Use it for tests and non-durable data only.
    #[derive(Debug)]
    pub struct FileStore {
        file: File,
        page_size: usize,
    }

    impl FileStore {
        /// Wraps an open file. A zero-length file is an empty tree.
        pub fn new(file: File, page_size: usize) -> Self {
            Self { file, page_size }
        }

        /// The underlying file, for example to call `sync_all`.
        pub fn file(&self) -> &File {
            &self.file
        }

        /// Consumes the store and returns the file.
        pub fn into_file(self) -> File {
            self.file
        }

        fn offset(&self, index: u32) -> u64 {
            u64::from(index) * self.page_size as u64
        }
    }

    impl PageStore for FileStore {
        type Error = io::Error;

        fn page_size(&self) -> usize {
            self.page_size
        }

        fn page_count(&self) -> io::Result<u32> {
            let len = self.file.metadata()?.len();
            let page_size = self.page_size as u64;
            if page_size == 0 || !len.is_multiple_of(page_size) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file length is not a whole number of pages",
                ));
            }
            u32::try_from(len / page_size).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "file holds too many pages")
            })
        }

        fn read_page(&self, index: u32, buf: &mut [u8]) -> io::Result<()> {
            read_exact_at(&self.file, buf, self.offset(index))
        }

        fn write_page(&mut self, index: u32, buf: &[u8]) -> io::Result<()> {
            write_all_at(&self.file, buf, self.offset(index))
        }

        fn truncate(&mut self, count: u32) -> io::Result<()> {
            self.file.set_len(self.offset(count))
        }
    }

    #[cfg(unix)]
    fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
    }

    #[cfg(unix)]
    fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
    }

    #[cfg(windows)]
    fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            match file.seek_read(buf, offset) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            match file.seek_write(buf, offset) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = file;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(buf)
    }

    #[cfg(not(any(unix, windows)))]
    fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = file;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 16;

    #[test]
    fn mem_store_appends_overwrites_and_truncates() {
        let mut store = MemStore::new(PAGE);
        assert_eq!(store.page_count(), Ok(0));
        store.write_page(0, &[1; PAGE]).unwrap();
        store.write_page(1, &[2; PAGE]).unwrap();
        assert_eq!(
            store.write_page(3, &[3; PAGE]),
            Err(MemStoreError::OutOfBounds)
        );
        store.write_page(0, &[4; PAGE]).unwrap();
        assert_eq!(store.page_count(), Ok(2));
        let mut buf = [0; PAGE];
        store.read_page(0, &mut buf).unwrap();
        assert_eq!(buf, [4; PAGE]);
        assert_eq!(
            store.read_page(2, &mut buf),
            Err(MemStoreError::OutOfBounds)
        );
        store.truncate(1).unwrap();
        assert_eq!(store.page_count(), Ok(1));
        assert_eq!(store.truncate(2), Err(MemStoreError::OutOfBounds));
    }

    #[test]
    fn mem_store_rejects_partial_pages_and_bad_buffers() {
        let store = MemStore::from_bytes(PAGE, alloc::vec![0; PAGE + 1]);
        assert_eq!(store.page_count(), Err(MemStoreError::PartialPage));
        let mut store = MemStore::new(PAGE);
        assert_eq!(store.write_page(0, &[0; 3]), Err(MemStoreError::BufferSize));
    }
}
