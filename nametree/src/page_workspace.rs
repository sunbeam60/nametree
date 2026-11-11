//! The page workspace: a thin layer over a [`PageStore`] for the duration of one public call.
//!
//! Every page the call touches is held in a working buffer. Modified pages are written once,
//! when the call finishes, so a page changed twice (compaction, rename) costs one write, and a
//! call that fails before finishing writes nothing at all. New pages are appended at the end
//! and freed pages are removed by moving the last page into their place, so the store stays
//! dense. There is no read cache across calls: the store is expected to provide one.

use alloc::vec;
use alloc::vec::Vec;

use crate::error::{CorruptKind, Error};
use crate::store::PageStore;

/// Checks a page freshly read from the store, given the store's page count.
pub(crate) type Validator = fn(&[u8], u32) -> Result<(), CorruptKind>;

/// A page touched by the current call.
struct Work {
    index: u32,
    dirty: bool,
    buf: Vec<u8>,
}

pub(crate) struct PageWorkspace<S> {
    store: S,
    page_size: usize,
    validate: Validator,
    /// Page count of the store when the call began.
    stored: u32,
    /// Page count the store will have once the call's changes are written.
    count: u32,
    work: Vec<Work>,
    /// Page buffers kept for reuse, so steady-state calls allocate nothing.
    spare: Vec<Vec<u8>>,
}

type Res<T, S> = Result<T, Error<<S as PageStore>::Error>>;

impl<S: PageStore> PageWorkspace<S> {
    pub(crate) fn new(store: S, validate: Validator) -> Self {
        let page_size = store.page_size();
        Self {
            store,
            page_size,
            validate,
            stored: 0,
            count: 0,
            work: Vec::new(),
            spare: Vec::new(),
        }
    }

    pub(crate) fn page_size(&self) -> usize {
        self.page_size
    }

    /// Pages in use, including pages appended and excluding pages freed by this call.
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    pub(crate) fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    pub(crate) fn into_store(self) -> S {
        self.store
    }

    /// Starts a call: drops any leftover working pages and reads the store's page count.
    pub(crate) fn begin(&mut self) -> Res<(), S> {
        self.release();
        let count = self.store.page_count().map_err(Error::Store)?;
        self.stored = count;
        self.count = count;
        Ok(())
    }

    /// Ends a call without writing anything.
    pub(crate) fn discard(&mut self) {
        self.release();
        self.count = self.stored;
    }

    /// Ends a call: writes every modified page once, in page order (so appended pages arrive
    /// in sequence), then truncates if the call freed pages.
    pub(crate) fn flush(&mut self) -> Res<(), S> {
        let result = self.write_back();
        self.release();
        result
    }

    fn write_back(&mut self) -> Res<(), S> {
        self.work.sort_unstable_by_key(|work| work.index);
        for work in self.work.iter().filter(|w| w.dirty && w.index < self.count) {
            self.store
                .write_page(work.index, &work.buf)
                .map_err(Error::Store)?;
        }
        if self.count < self.stored {
            self.store.truncate(self.count).map_err(Error::Store)?;
        }
        self.stored = self.count;
        Ok(())
    }

    fn release(&mut self) {
        let spare = &mut self.spare;
        spare.extend(self.work.drain(..).map(|work| work.buf));
    }

    /// A page-sized buffer with unspecified contents.
    pub(crate) fn take_buf(&mut self) -> Vec<u8> {
        self.spare.pop().unwrap_or_else(|| vec![0; self.page_size])
    }

    /// Returns a buffer from [`take_buf`](Self::take_buf) for reuse.
    pub(crate) fn give_buf(&mut self, buf: Vec<u8>) {
        self.spare.push(buf);
    }

    fn find(&self, index: u32) -> Option<usize> {
        self.work.iter().position(|work| work.index == index)
    }

    /// Makes page `index` part of the call's working set, reading and validating it if needed.
    fn load(&mut self, index: u32) -> Res<usize, S> {
        if let Some(slot) = self.find(index) {
            return Ok(slot);
        }
        let mut buf = self.take_buf();
        if let Err(e) = self.read_detached(index, &mut buf) {
            self.give_buf(buf);
            return Err(e);
        }
        self.work.push(Work {
            index,
            dirty: false,
            buf,
        });
        Ok(self.work.len() - 1)
    }

    /// Page `index` as the call currently sees it.
    pub(crate) fn read(&mut self, index: u32) -> Res<&[u8], S> {
        let slot = self.load(index)?;
        Ok(&self.work[slot].buf)
    }

    /// Page `index` for modification; it will be written when the call finishes.
    pub(crate) fn write(&mut self, index: u32) -> Res<&mut [u8], S> {
        let slot = self.load(index)?;
        let work = &mut self.work[slot];
        work.dirty = true;
        Ok(&mut work.buf)
    }

    /// Copies page `index`, as the call currently sees it, into `dst`.
    pub(crate) fn copy_into(&mut self, index: u32, dst: &mut [u8]) -> Res<(), S> {
        dst.copy_from_slice(self.read(index)?);
        Ok(())
    }

    /// Overwrites page `to` with the contents of page `from`.
    pub(crate) fn copy_page(&mut self, from: u32, to: u32) -> Res<(), S> {
        let mut buf = self.take_buf();
        self.copy_into(from, &mut buf)?;
        self.write(to)?.copy_from_slice(&buf);
        self.give_buf(buf);
        Ok(())
    }

    /// Reads and validates page `index` straight from the store into `dst`, bypassing the
    /// working set. For iterators and checks, which run between calls.
    pub(crate) fn read_detached(&self, index: u32, dst: &mut [u8]) -> Res<(), S> {
        if index >= self.count {
            return Err(Error::Corrupt(CorruptKind::PageIndex));
        }
        self.store.read_page(index, dst).map_err(Error::Store)?;
        (self.validate)(dst, self.count).map_err(Error::Corrupt)
    }

    /// Appends a zeroed page and returns its index.
    pub(crate) fn append(&mut self) -> Res<u32, S> {
        let index = self.count;
        // A u32 page count cannot grow further; no real directory gets anywhere near this.
        self.count = index
            .checked_add(1)
            .ok_or(Error::Corrupt(CorruptKind::PageIndex))?;
        let mut buf = self.take_buf();
        buf.fill(0);
        self.work.push(Work {
            index,
            dirty: true,
            buf,
        });
        Ok(index)
    }

    /// Frees page `to` by moving the last page into it. The caller repoints every reference to
    /// the old last page.
    pub(crate) fn move_last(&mut self, to: u32) -> Res<(), S> {
        let last = self.count - 1;
        self.forget(to);
        let slot = self.load(last)?;
        let work = &mut self.work[slot];
        work.index = to;
        work.dirty = true;
        self.count = last;
        Ok(())
    }

    /// Frees the last page.
    pub(crate) fn drop_last(&mut self) {
        self.count -= 1;
        self.forget(self.count);
    }

    /// Frees every page: the tree becomes a zero-length store.
    pub(crate) fn drop_all(&mut self) {
        self.release();
        self.count = 0;
    }

    /// Drops page `index` from the working set without writing it.
    fn forget(&mut self, index: u32) {
        if let Some(slot) = self.find(index) {
            let work = self.work.swap_remove(slot);
            self.give_buf(work.buf);
        }
    }
}
