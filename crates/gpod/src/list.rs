use std::marker::PhantomData;
use std::os::raw::c_void;

/// Walks a `GList` owned by a database, yielding each node's `data`.
pub(crate) struct ListIter<'db> {
    node: *const glib_sys::GList,
    _db: PhantomData<&'db ()>,
}

impl<'db> ListIter<'db> {
    /// `owner` is the struct holding `head`; borrowing it ties the iterator's
    /// lifetime to the database so it cannot outlive it or a mutation of it.
    ///
    /// # Safety
    /// `head` must be null or a valid `GList` owned by `owner`.
    pub(crate) unsafe fn new<T: ?Sized>(head: *const glib_sys::GList, _owner: &'db T) -> Self {
        ListIter {
            node: head,
            _db: PhantomData,
        }
    }
}

impl Iterator for ListIter<'_> {
    type Item = *mut c_void;

    fn next(&mut self) -> Option<Self::Item> {
        if self.node.is_null() {
            return None;
        }
        unsafe {
            let data = (*self.node).data;
            self.node = (*self.node).next;
            Some(data)
        }
    }
}

/// # Safety
/// Same as [`ListIter::new`].
pub(crate) unsafe fn len<T: ?Sized>(head: *const glib_sys::GList, owner: &T) -> usize {
    unsafe { ListIter::new(head, owner) }.count()
}
