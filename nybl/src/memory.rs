//! Allocation-owned memory accounting for Nybl execution.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::rc::Rc;
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::rc::Rc;

use core::cell::Cell;
#[cfg(any(feature = "std", not(feature = "no_std")))]
use core::cell::RefCell;

/// One sandbox's persistent memory account. Allocation receipts retain their
/// owner, so values outliving an operation remain charged to the instance that
/// created them and release that charge when their backing allocation drops.
#[doc(hidden)]
#[derive(Debug)]
pub struct MemoryAccount {
    used: Cell<usize>,
    limit: usize,
}

impl MemoryAccount {
    #[doc(hidden)]
    pub fn __new(limit: usize) -> Rc<Self> {
        Rc::new(Self {
            used: Cell::new(0),
            limit,
        })
    }

    fn alloc(&self, bytes: usize) {
        self.used.set(self.used.get().saturating_add(bytes));
    }

    fn dealloc(&self, bytes: usize) {
        self.used.set(self.used.get().saturating_sub(bytes));
    }

    #[doc(hidden)]
    pub fn __exceeded(&self) -> bool {
        self.used.get() > self.limit
    }

    #[doc(hidden)]
    pub fn __would_exceed(&self, bytes: usize) -> bool {
        self.used.get().saturating_add(bytes) > self.limit
    }

    #[doc(hidden)]
    pub fn __used(&self) -> usize {
        self.used.get()
    }
}

/// Explicit allocation owner for one engine operation or persistent instance.
///
/// Runtime engines carry this value through their evaluator/VM/AOT context.
/// It deliberately contains no process-global state, so independent no-std
/// executions cannot race or charge one another's accounts.
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct MemoryContext {
    account: Option<Rc<MemoryAccount>>,
}

impl MemoryContext {
    /// Create a tracked context with an independent byte ceiling.
    #[doc(hidden)]
    pub fn __new(limit: usize) -> Self {
        Self {
            account: Some(MemoryAccount::__new(limit)),
        }
    }

    /// Create an untracked host/compatibility context.
    #[doc(hidden)]
    pub const fn __untracked() -> Self {
        Self { account: None }
    }

    /// Compatibility context used by legacy public constructors. Current
    /// engines never call this method.
    #[doc(hidden)]
    pub fn __legacy_current() -> Self {
        #[cfg(any(feature = "std", not(feature = "no_std")))]
        {
            Self {
                account: legacy_active(),
            }
        }
        #[cfg(all(feature = "no_std", not(feature = "std")))]
        {
            Self::__untracked()
        }
    }

    /// Reuse an existing persistent account.
    #[doc(hidden)]
    pub fn __from_account(account: &Rc<MemoryAccount>) -> Self {
        Self {
            account: Some(Rc::clone(account)),
        }
    }

    #[doc(hidden)]
    pub fn __account(&self) -> Option<&Rc<MemoryAccount>> {
        self.account.as_ref()
    }

    #[doc(hidden)]
    pub fn __exceeded(&self) -> bool {
        self.account
            .as_ref()
            .is_some_and(|account| account.__exceeded())
    }

    #[doc(hidden)]
    pub fn __would_exceed(&self, bytes: usize) -> bool {
        self.account
            .as_ref()
            .is_some_and(|account| account.__would_exceed(bytes))
    }

    #[doc(hidden)]
    pub fn __used(&self) -> usize {
        self.account.as_ref().map_or(0, |account| account.__used())
    }

    /// Bytes still available at this instant, or `None` for an untracked
    /// context. A tracked account already over budget reports zero.
    #[doc(hidden)]
    pub fn __available(&self) -> Option<usize> {
        self.account
            .as_ref()
            .map(|account| account.limit.saturating_sub(account.__used()))
    }
}

#[cfg(any(feature = "std", not(feature = "no_std")))]
thread_local! {
    // Compatibility-only ambient account. Runtime engines never enter this
    // stack; they pass MemoryContext explicitly.
    static LEGACY_ACTIVE: RefCell<Option<Rc<MemoryAccount>>> = const { RefCell::new(None) };
}

#[cfg(any(feature = "std", not(feature = "no_std")))]
fn replace_legacy_active(next: Option<Rc<MemoryAccount>>) -> Option<Rc<MemoryAccount>> {
    LEGACY_ACTIVE.with(|active| core::mem::replace(&mut *active.borrow_mut(), next))
}

#[cfg(any(feature = "std", not(feature = "no_std")))]
fn legacy_active() -> Option<Rc<MemoryAccount>> {
    LEGACY_ACTIVE.with(|active| active.borrow().clone())
}

/// Panic-safe legacy active-account stack entry.
///
/// This compatibility API exists only with `std`; runtime engines use
/// [`MemoryContext`] directly.
#[cfg(any(feature = "std", not(feature = "no_std")))]
#[doc(hidden)]
pub struct ActiveMemoryGuard(Option<Rc<MemoryAccount>>);

#[cfg(any(feature = "std", not(feature = "no_std")))]
impl ActiveMemoryGuard {
    #[doc(hidden)]
    pub fn __activate(account: &Rc<MemoryAccount>) -> Self {
        Self(replace_legacy_active(Some(Rc::clone(account))))
    }

    #[doc(hidden)]
    pub fn __suspend() -> Self {
        Self(replace_legacy_active(None))
    }

    #[doc(hidden)]
    pub fn __activate_new_if_none(limit: usize) -> Self {
        let next = legacy_active().or_else(|| Some(MemoryAccount::__new(limit)));
        Self(replace_legacy_active(next))
    }
}

#[cfg(any(feature = "std", not(feature = "no_std")))]
impl Drop for ActiveMemoryGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        replace_legacy_active(previous);
    }
}

/// Owner receipt embedded in each tracked backing allocation.
#[derive(Debug)]
pub(crate) struct MemoryReceipt {
    owner: Option<Rc<MemoryAccount>>,
    bytes: usize,
}

impl MemoryReceipt {
    pub(crate) fn new_in(context: &MemoryContext, bytes: usize) -> Self {
        let owner = context.account.clone();
        if let Some(account) = &owner {
            account.alloc(bytes);
        }
        Self { owner, bytes }
    }

    pub(crate) fn resize(&mut self, bytes: usize) {
        if let Some(account) = &self.owner {
            if bytes > self.bytes {
                account.alloc(bytes - self.bytes);
            } else {
                account.dealloc(self.bytes - bytes);
            }
        }
        self.bytes = bytes;
    }

    pub(crate) fn owner_matches(&self, context: &MemoryContext) -> bool {
        match (&self.owner, &context.account) {
            (None, None) => true,
            (Some(owner), Some(active)) => Rc::ptr_eq(owner, active),
            _ => false,
        }
    }
}

impl Drop for MemoryReceipt {
    fn drop(&mut self) {
        if let Some(account) = &self.owner {
            account.dealloc(self.bytes);
        }
    }
}

/// Legacy one-shot compatibility: install a fresh account until another
/// one-shot run replaces it. Runtime engines do not call this API.
#[cfg(any(feature = "std", not(feature = "no_std")))]
pub fn nybl_memory_init(limit: usize) {
    replace_legacy_active(Some(MemoryAccount::__new(limit)));
}

/// Legacy mutation hook. New owned backings use allocation receipts directly.
#[cfg(any(feature = "std", not(feature = "no_std")))]
pub fn nybl_alloc(bytes: usize) {
    if let Some(account) = legacy_active() {
        account.alloc(bytes);
    }
}

/// Legacy mutation hook paired with [`nybl_alloc`].
#[cfg(any(feature = "std", not(feature = "no_std")))]
pub fn nybl_dealloc(bytes: usize) {
    if let Some(account) = legacy_active() {
        account.dealloc(bytes);
    }
}

#[cfg(any(feature = "std", not(feature = "no_std")))]
pub fn nybl_memory_exceeded() -> bool {
    legacy_active().is_some_and(|account| account.__exceeded())
}

#[cfg(any(feature = "std", not(feature = "no_std")))]
pub fn nybl_would_exceed(bytes: usize) -> bool {
    legacy_active().is_some_and(|account| account.__would_exceed(bytes))
}

#[cfg(all(test, any(feature = "std", not(feature = "no_std"))))]
pub(crate) fn nybl_memory_used() -> usize {
    legacy_active().map_or(0, |account| account.__used())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;

    #[test]
    fn value_layout_stays_compact_with_allocation_receipts() {
        assert_eq!(
            core::mem::size_of::<crate::value::NyblStr>(),
            core::mem::size_of::<usize>()
        );
        assert!(core::mem::size_of::<Value>() <= 32);
    }

    #[test]
    fn explicit_contexts_are_independent() {
        let first = MemoryContext::__new(1024 * 1024);
        let second = MemoryContext::__new(1024 * 1024);
        let first_receipt = MemoryReceipt::new_in(&first, 17);
        let second_receipt = MemoryReceipt::new_in(&second, 29);
        assert_eq!(first.__used(), 17);
        assert_eq!(second.__used(), 29);
        drop(first_receipt);
        assert_eq!(first.__used(), 0);
        assert_eq!(second.__used(), 29);
        drop(second_receipt);
        assert_eq!(second.__used(), 0);
    }

    #[cfg(any(feature = "std", not(feature = "no_std")))]
    #[test]
    fn explicit_value_accounts_stay_isolated_across_threads() {
        use std::sync::{Arc, Barrier};

        let ready = Arc::new(Barrier::new(2));
        let finish = Arc::new(Barrier::new(2));
        let handles = [17, 53].map(|len| {
            let ready = Arc::clone(&ready);
            let finish = Arc::clone(&finish);
            std::thread::spawn(move || {
                let memory = MemoryContext::__new(1024 * 1024);
                let mut value = Value::__try_new_array_in(
                    vec![Value::__new_str_in("x".repeat(len), &memory)],
                    1,
                    &memory,
                )
                .expect("test value fits");
                ready.wait();
                let before = memory.__used();
                let Value::Array(array) = &mut value else {
                    unreachable!("constructed an array");
                };
                array
                    .__try_push_in(Value::Int(1), 1, &memory)
                    .expect("test push fits");
                assert!(memory.__used() >= before);
                finish.wait();
                drop(value);
                assert_eq!(memory.__used(), 0);
                before
            })
        });

        let used = handles.map(|handle| {
            handle
                .join()
                .expect("explicit memory-account worker panicked")
        });
        assert_ne!(used[0], used[1]);
    }

    #[cfg(any(feature = "std", not(feature = "no_std")))]
    #[test]
    fn legacy_ambient_accounts_remain_thread_local() {
        use std::sync::{Arc, Barrier};

        let ready = Arc::new(Barrier::new(2));
        let finish = Arc::new(Barrier::new(2));
        let handles = [17, 29].map(|bytes| {
            let ready = Arc::clone(&ready);
            let finish = Arc::clone(&finish);
            std::thread::spawn(move || {
                nybl_memory_init(1024);
                ready.wait();
                nybl_alloc(bytes);
                finish.wait();
                assert_eq!(nybl_memory_used(), bytes);
                nybl_dealloc(bytes);
                assert_eq!(nybl_memory_used(), 0);
            })
        });

        for handle in handles {
            handle.join().expect("memory-account worker panicked");
        }
    }
}
