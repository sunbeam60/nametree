//! `NameTree`: the public API, descent with a path stack, and the structure changes (in the
//! `insert` and `remove` submodules).

use core::cell::RefCell;
use core::marker::PhantomData;

use crate::error::{CorruptKind, Error};
use crate::format::{
    self, hash_prefix, run_last, Value, LEAF_LEVEL, MAX_LEVEL, MAX_NAME_LEN, MAX_PAGE_SIZE, ROOT,
};
use crate::hasher::NameHasher;
use crate::iter::Iter;
use crate::pager::Pager;
use crate::store::PageStore;
use crate::{inner, leaf};

mod insert;
mod remove;

/// Result type used inside the tree, with the store's error type.
pub(crate) type Res<T, S> = Result<T, Error<<S as PageStore>::Error>>;

/// One directory's names as a B+ tree of fixed-size pages inside a [`PageStore`].
///
/// Each name maps to a fixed-size value `V`. Names are byte strings of 1 to 255 bytes, compared
/// byte-exact (case-sensitive, no Unicode normalisation).
///
/// The tree keeps no state between calls beyond reusable buffers, so it reads the store's page
/// count at the start of every call. It is not `Sync`: share it between threads by giving each
/// thread its own tree over its own store handle.
pub struct NameTree<S, H, V> {
    pub(crate) pager: RefCell<Pager<S>>,
    pub(crate) hasher: H,
    value: PhantomData<fn() -> V>,
}

impl<S: PageStore, H: NameHasher, V: Value> NameTree<S, H, V> {
    /// Opens a tree over `store`. A zero-length store is a valid empty tree; otherwise page 0
    /// is read and checked. There is no `create`: creating a tree is creating an empty store.
    pub fn open(store: S, hasher: H) -> Result<Self, Error<S::Error>> {
        let page_size = store.page_size();
        if !(format::min_page_size::<V>()..=MAX_PAGE_SIZE).contains(&page_size) {
            return Err(Error::PageSize);
        }
        let mut pager = Pager::new(store, format::validate_page::<V>);
        pager.begin()?;
        let root = if pager.count() > 0 {
            pager.read(ROOT).map(drop)
        } else {
            Ok(())
        };
        pager.discard();
        root?;
        Ok(Self {
            pager: RefCell::new(pager),
            hasher,
            value: PhantomData,
        })
    }

    /// The value stored under `name`, if any.
    pub fn get(&self, name: &[u8]) -> Result<Option<V>, Error<S::Error>> {
        check_name(name)?;
        let prefix = hash_prefix(self.hasher.hash(name));
        let mut pager = self.pager.borrow_mut();
        pager.begin()?;
        let mut ctx = Ctx::<S, V>::new(&mut pager);
        let mut path = Path::new();
        let found = ctx
            .find(prefix, name, &mut path)
            .and_then(|slot| match slot {
                Some(slot) => Ok(Some(leaf::value::<V>(ctx.pager.read(path.leaf())?, slot))),
                None => Ok(None),
            });
        pager.discard();
        found
    }

    /// Stores `value` under `name`, returning the previous value if the name existed.
    pub fn insert(&mut self, name: &[u8], value: V) -> Result<Option<V>, Error<S::Error>> {
        check_name(name)?;
        let prefix = hash_prefix(self.hasher.hash(name));
        self.mutate(|ctx| ctx.insert(prefix, name, &value))
    }

    /// Removes `name`, returning its value if it existed.
    pub fn remove(&mut self, name: &[u8]) -> Result<Option<V>, Error<S::Error>> {
        check_name(name)?;
        let prefix = hash_prefix(self.hasher.hash(name));
        self.mutate(|ctx| ctx.remove(prefix, name))
    }

    /// Renames `from` to `to` within this tree, keeping its value. Fails with
    /// [`Error::NotFound`] or [`Error::Exists`] before writing anything. Renaming a name to
    /// itself succeeds without writing.
    ///
    /// A rename between two trees is the caller's job: `remove` from one and `insert` into the
    /// other, inside one host transaction.
    pub fn rename(&mut self, from: &[u8], to: &[u8]) -> Result<(), Error<S::Error>> {
        check_name(from)?;
        check_name(to)?;
        let from_prefix = hash_prefix(self.hasher.hash(from));
        let to_prefix = hash_prefix(self.hasher.hash(to));
        self.mutate(|ctx| {
            let mut path = Path::new();
            if ctx.find(from_prefix, from, &mut path)?.is_none() {
                return Err(Error::NotFound);
            }
            if from == to {
                return Ok(());
            }
            if ctx.find(to_prefix, to, &mut path)?.is_some() {
                return Err(Error::Exists);
            }
            let value = ctx
                .remove(from_prefix, from)?
                .ok_or(Error::Corrupt(CorruptKind::KeyOrder))?;
            ctx.insert(to_prefix, to, &value).map(drop)
        })
    }

    /// `true` if the tree holds no names (its store is zero-length).
    pub fn is_empty(&self) -> Result<bool, Error<S::Error>> {
        let mut pager = self.pager.borrow_mut();
        pager.begin()?;
        let empty = pager.count() == 0;
        pager.discard();
        Ok(empty)
    }

    /// Iterates over every name in key order. Same as `iter_from(0)`.
    pub fn iter(&self) -> Iter<'_, S, H, V> {
        self.iter_from(0)
    }

    /// Iterates over the names whose cookie is greater than `cookie`, in cookie order.
    ///
    /// Each entry carries its cookie: an opaque, positive, non-zero 63-bit value. Resuming from
    /// any yielded cookie returns every name that existed throughout exactly once, whatever
    /// changed in between. Cookie 0 means the start.
    pub fn iter_from(&self, cookie: u64) -> Iter<'_, S, H, V> {
        Iter::new(self, cookie, accept_all::<V>)
    }

    /// Like [`iter_from`](Self::iter_from), but yields only names whose value passes `filter`.
    /// Name bytes are read only for names that pass.
    pub fn iter_filtered<F: FnMut(&V) -> bool>(
        &self,
        cookie: u64,
        filter: F,
    ) -> Iter<'_, S, H, V, F> {
        Iter::new(self, cookie, filter)
    }

    /// The store's page size in bytes.
    pub fn page_size(&self) -> usize {
        self.pager.borrow().page_size()
    }

    /// The hasher.
    pub fn hasher(&self) -> &H {
        &self.hasher
    }

    /// The underlying store. Changing it behind the tree's back can corrupt the tree.
    pub fn store_mut(&mut self) -> &mut S {
        self.pager.get_mut().store_mut()
    }

    /// Consumes the tree and returns the store.
    pub fn into_inner(self) -> S {
        self.pager.into_inner().into_store()
    }

    /// Runs one mutating call: everything it changes is buffered and written once at the end,
    /// and nothing is written if it fails.
    fn mutate<R>(&mut self, op: impl FnOnce(&mut Ctx<'_, S, V>) -> Res<R, S>) -> Res<R, S> {
        let pager = self.pager.get_mut();
        pager.begin()?;
        match op(&mut Ctx::new(pager)) {
            Ok(result) => {
                pager.flush()?;
                Ok(result)
            }
            Err(e) => {
                pager.discard();
                Err(e)
            }
        }
    }
}

fn accept_all<V>(_: &V) -> bool {
    true
}

fn check_name<E>(name: &[u8]) -> Result<(), Error<E>> {
    if name.is_empty() {
        Err(Error::NameEmpty)
    } else if name.len() > MAX_NAME_LEN {
        Err(Error::NameTooLong)
    } else {
        Ok(())
    }
}

/// Entries in a [`Path`], indexed by level (index 0 is unused).
const PATH_LEN: usize = MAX_LEVEL as usize + 1;

/// The pages and child positions visited on the way down from the root, indexed by level.
#[derive(Clone)]
pub(crate) struct Path {
    /// Page visited at each level.
    page: [u32; PATH_LEN],
    /// Child position taken at each inner level.
    pos: [usize; PATH_LEN],
    /// Level of the root.
    height: u32,
}

impl Path {
    pub(crate) fn new() -> Self {
        Self {
            page: [ROOT; PATH_LEN],
            pos: [0; PATH_LEN],
            height: 0,
        }
    }

    pub(crate) fn page(&self, level: u32) -> u32 {
        self.page[level as usize]
    }

    pub(crate) fn pos(&self, level: u32) -> usize {
        self.pos[level as usize]
    }

    pub(crate) fn leaf(&self) -> u32 {
        self.page(LEAF_LEVEL)
    }

    fn set(&mut self, level: u32, page: u32, pos: usize) {
        self.page[level as usize] = page;
        self.pos[level as usize] = pos;
    }

    /// Updates the path after page `from` moved to `to`.
    fn renumber(&mut self, from: u32, to: u32) {
        for page in &mut self.page[1..=self.height as usize] {
            if *page == from {
                *page = to;
            }
        }
    }
}

/// One call's view of the tree: the pager plus the value type.
pub(crate) struct Ctx<'a, S, V> {
    pub(crate) pager: &'a mut Pager<S>,
    value: PhantomData<fn() -> V>,
}

impl<'a, S: PageStore, V: Value> Ctx<'a, S, V> {
    pub(crate) fn new(pager: &'a mut Pager<S>) -> Self {
        Self {
            pager,
            value: PhantomData,
        }
    }

    /// Descends from the root to the leaf covering `key`, recording the path. The tree must
    /// not be empty.
    pub(crate) fn descend(&mut self, key: u64, path: &mut Path) -> Res<(), S> {
        self.descend_to(key, LEAF_LEVEL, path).map(drop)
    }

    /// Descends from the root towards `key` down to `level`, recording the path, and returns
    /// the page reached at `level`. Checks that every page has the level its parent implies.
    fn descend_to(&mut self, key: u64, level: u32, path: &mut Path) -> Res<u32, S> {
        let mut index = ROOT;
        let mut at = format::level(self.pager.read(ROOT)?);
        path.height = at;
        while at > level {
            let page = self.pager.read(index)?;
            if format::level(page) != at {
                return Err(Error::Corrupt(CorruptKind::LevelMismatch));
            }
            let (pos, child) = inner::route(page, key);
            path.set(at, index, pos);
            index = child;
            at -= 1;
        }
        path.set(at, index, 0);
        if format::level(self.pager.read(index)?) != at {
            return Err(Error::Corrupt(CorruptKind::LevelMismatch));
        }
        Ok(index)
    }

    /// Finds `name` (whose key prefix is `prefix`), returning its slot in leaf `path.leaf()`.
    pub(crate) fn find(
        &mut self,
        prefix: u64,
        name: &[u8],
        path: &mut Path,
    ) -> Res<Option<usize>, S> {
        if self.pager.count() == 0 {
            return Ok(None);
        }
        self.descend(run_last(prefix), path)?;
        let page = self.pager.read(path.leaf())?;
        Ok(leaf::find::<V>(page, prefix, name))
    }
}
