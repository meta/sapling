/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::sync::OnceLock;

use parking_lot::RawRwLock;
use parking_lot::lock_api::GuardNoSend;
use parking_lot::lock_api::RawRwLock as RawRwLockTrait;

pub struct WrappedRwLock {
    inner: RawRwLock,
    pub(crate) on_unlock_exclusive: OnceLock<Box<dyn Fn() + Send + Sync + 'static>>,
}

unsafe impl RawRwLockTrait for WrappedRwLock {
    // https://rust-lang.github.io/rust-clippy/master/index.html#declare_interior_mutable_const
    // > Consts are copied everywhere they are referenced, i.e., every time you refer to the
    // > const a fresh instance of the Cell or Mutex or AtomicXxxx will be created, which defeats
    // > the whole purpose of using these types in the first place.
    //
    // We actually want this behavior. Every time `INIT` should give us a fresh new lock that
    // has an empty `on_unlock_exclusive`.
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self {
        inner: RawRwLock::INIT,
        on_unlock_exclusive: OnceLock::new(),
    };

    type SharedGuard = (<RawRwLock as RawRwLockTrait>::SharedGuard, GuardNoSend);
    type ExclusiveGuard = (<RawRwLock as RawRwLockTrait>::ExclusiveGuard, GuardNoSend);

    fn lock_shared(&self) -> Self::SharedGuard {
        (self.inner.lock_shared(), GuardNoSend::new())
    }

    fn try_lock_shared(&self) -> Option<Self::SharedGuard> {
        Some((self.inner.try_lock_shared()?, GuardNoSend::new()))
    }

    unsafe fn unlock_shared(&self, guard: Self::SharedGuard) {
        unsafe { self.inner.unlock_shared(guard.0) }
    }

    fn lock_exclusive(&self) -> Self::ExclusiveGuard {
        (self.inner.lock_exclusive(), GuardNoSend::new())
    }

    fn try_lock_exclusive(&self) -> Option<Self::ExclusiveGuard> {
        Some((self.inner.try_lock_exclusive()?, GuardNoSend::new()))
    }

    unsafe fn unlock_exclusive(&self, guard: Self::ExclusiveGuard) {
        unsafe {
            self.inner.unlock_exclusive(guard.0);
            if let Some(f) = self.on_unlock_exclusive.get() {
                f()
            }
        }
    }

    fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }

    fn is_locked_exclusive(&self) -> bool {
        self.inner.is_locked_exclusive()
    }
}

/// Wrapped `RwLock` that does something extra on unlock_exclusive.
pub type RwLock<T> = parking_lot::lock_api::RwLock<WrappedRwLock, T>;
