//! On some targets like wasm there's no threads, so no need to generate
//! thread locals and we can instead just use plain statics!

use crate::cell::RefCell;
use crate::cell::{Cell, UnsafeCell};
use crate::ptr;

#[doc(hidden)]
#[allow_internal_unstable(thread_local_internals)]
#[allow_internal_unsafe]
#[unstable(feature = "thread_local_internals", issue = "none")]
#[rustc_macro_transparency = "semitransparent"]
pub macro thread_local_inner {
    // used to generate the `LocalKey` value for const-initialized thread locals
    (@key $t:ty, const $init:expr) => {{
        const __INIT: $t = $init;

        // NOTE: Please update the shadowing test in `tests/thread.rs` if these types are renamed.
        unsafe {
            $crate::thread::LocalKey::new(|_| {
                static VAL: $crate::thread::local_impl::EagerStorage<$t> =
                    $crate::thread::local_impl::EagerStorage { value: __INIT };
                &VAL.value
            })
        }
    }},

    // used to generate the `LocalKey` value for `thread_local!`
    (@key $t:ty, $init:expr) => {{
        #[inline]
        fn __init() -> $t { $init }

        unsafe {
            use $crate::thread::LocalKey;
            use $crate::thread::local_impl::LazyStorage;

            LocalKey::new(|init| {
                static VAL: LazyStorage<$t> = LazyStorage::new();
                VAL.get(init, __init)
            })
        }
    }},
    ($(#[$attr:meta])* $vis:vis $name:ident, $t:ty, $($init:tt)*) => {
        $(#[$attr])* $vis const $name: $crate::thread::LocalKey<$t> =
            $crate::thread::local_impl::thread_local_inner!(@key $t, $($init)*);
    },
}

#[allow(missing_debug_implementations)]
pub struct EagerStorage<T> {
    pub value: T,
}

// SAFETY: the target doesn't have threads.
unsafe impl<T> Sync for EagerStorage<T> {}

#[allow(missing_debug_implementations)]
pub struct LazyStorage<T> {
    value: UnsafeCell<Option<T>>,
}

impl<T> LazyStorage<T> {
    pub const fn new() -> LazyStorage<T> {
        LazyStorage { value: UnsafeCell::new(None) }
    }

    /// Gets a pointer to the TLS value, potentially initializing it with the
    /// provided parameters.
    ///
    /// The resulting pointer may not be used after reentrant inialialization
    /// has occurred.
    #[inline]
    pub fn get(&'static self, i: Option<&mut Option<T>>, f: impl FnOnce() -> T) -> *const T {
        let value = unsafe { &*self.value.get() };
        match value {
            Some(v) => v,
            None => self.initialize(i, f),
        }
    }

    #[cold]
    fn initialize(&'static self, i: Option<&mut Option<T>>, f: impl FnOnce() -> T) -> *const T {
        unsafe {
            register_dtor(|| Self::destroy_value(&self.value));
        }

        let value = i.and_then(Option::take).unwrap_or_else(f);
        // Destroy the old value, after updating the TLS variable as the
        // destructor might reference it.
        // FIXME(#110897): maybe panic on recursive initialization.
        unsafe {
            self.value.get().replace(Some(value));
        }
        // SAFETY: we just set this to `Some`.
        unsafe { (*self.value.get()).as_ref().unwrap_unchecked() }
    }

    /// Destroy contained value.
    /// 
    /// Returns whether a value was contained.
    fn destroy_value(value: &UnsafeCell<Option<T>>) -> bool {
        unsafe { value.get().replace(None).is_some() }
    }
}

// SAFETY: the target doesn't have threads.
unsafe impl<T> Sync for LazyStorage<T> {}

#[rustc_macro_transparency = "semitransparent"]
pub(crate) macro local_pointer {
    () => {},
    ($vis:vis static $name:ident; $($rest:tt)*) => {
        $vis static $name: $crate::sys::thread_local::LocalPointer = $crate::sys::thread_local::LocalPointer::__new();
        $crate::sys::thread_local::local_pointer! { $($rest)* }
    },
}

pub(crate) struct LocalPointer {
    p: Cell<*mut ()>,
}

impl LocalPointer {
    pub const fn __new() -> LocalPointer {
        LocalPointer { p: Cell::new(ptr::null_mut()) }
    }

    pub fn get(&self) -> *mut () {
        self.p.get()
    }

    pub fn set(&self, p: *mut ()) {
        self.p.set(p)
    }
}

// SAFETY: the target doesn't have threads.
unsafe impl Sync for LocalPointer {}

struct Dtors (RefCell<Vec<Box<dyn FnOnce() -> bool + 'static>>>);
// SAFETY: the target doesn't have threads.
unsafe impl Sync for Dtors {}

/// List of destructors to run at process exit.
static DTORS: Dtors = Dtors(RefCell::new(Vec::new()));

/// Should only be called once per key, otherwise loops or breaks may occur in
/// the linked list.
unsafe fn register_dtor(dtor: impl FnOnce() -> bool + 'static) {
    //crate::sys::thread_local::guard::enable_process();

    DTORS.0.borrow_mut().push(Box::new(dtor));
}


/// This will and must only be run by the destructor callback in [`guard`].
pub unsafe fn run_dtors() {
    for _ in 0..5 {
        let mut any_run = false;

        // Use acquire ordering to observe key initialization.
        let mut cur = DTORS.load(Ordering::Acquire);
        while !cur.is_null() {
            let key = unsafe { (*cur).key.load(Ordering::Acquire) };
            let dtor = unsafe { (*cur).dtor.unwrap() };
            cur = unsafe { (*cur).next.load(Ordering::Relaxed) };

            // In LazyKey::init, we register the dtor before setting `key`.
            // So if one thread's `run_dtors` races with another thread executing `init` on the same
            // `LazyKey`, we can encounter a key of KEY_SENTVAL here. That means this key was never
            // initialized in this thread so we can safely skip it.
            if key == KEY_SENTVAL {
                continue;
            }
            // If this is non-zero, then via the `Acquire` load above we synchronized with
            // everything relevant for this key. (It's not clear that this is needed, since the
            // release-acquire pair on DTORS also establishes synchronization, but better safe than
            // sorry.)

            let ptr = unsafe { super::get(key as _) };
            if !ptr.is_null() {
                unsafe {
                    super::set(key as _, crate::ptr::null_mut());
                    dtor(ptr as *mut _);
                    any_run = true;
                }
            }
        }

        if !any_run {
            break;
        }
    }
}
