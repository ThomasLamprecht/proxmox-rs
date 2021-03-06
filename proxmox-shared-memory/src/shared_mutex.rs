use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};

use anyhow::{bail, Error};

use crate::raw_shared_mutex::RawSharedMutex;
use crate::Init;

#[derive(Debug)]
#[repr(C)]
pub struct SharedMutex<T: ?Sized> {
    magic: [u8; 8],
    inner: RawSharedMutex,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for SharedMutex<T> {}
unsafe impl<T: ?Sized + Send> Sync for SharedMutex<T> {}

// openssl::sha::sha256(b"Proxmox SharedMutex v1.0")[0..8];
pub const PROXMOX_SHARED_MUTEX_MAGIC_1_0: [u8; 8] = [124, 229, 154, 62, 248, 0, 154, 55];

impl<T: Init> Init for SharedMutex<T> {
    fn initialize(this: &mut MaybeUninit<SharedMutex<T>>) {
        let me = unsafe { &mut *this.as_mut_ptr() };

        me.magic = PROXMOX_SHARED_MUTEX_MAGIC_1_0;

        me.inner = RawSharedMutex::uninitialized();
        unsafe {
            me.inner.init();
        }

        let u: &mut MaybeUninit<T> = unsafe { std::mem::transmute(me.data.get_mut()) };
        Init::initialize(u);
    }

    fn check_type_magic(this: &MaybeUninit<Self>) -> Result<(), Error> {
        let me = unsafe { &*this.as_ptr() };
        if me.magic != PROXMOX_SHARED_MUTEX_MAGIC_1_0 {
            bail!("SharedMutex: wrong magic number");
        }
        Ok(())
    }
}

impl<T> SharedMutex<T> {
    pub fn lock(&self) -> SharedMutexGuard<'_, T> {
        unsafe {
            self.inner.lock();
            SharedMutexGuard::new(self)
        }
    }

    pub fn try_lock(&self) -> Option<SharedMutexGuard<'_, T>> {
        unsafe {
            if self.inner.try_lock() {
                Some(SharedMutexGuard::new(self))
            } else {
                None
            }
        }
    }

    pub fn unlock(guard: SharedMutexGuard<'_, T>) {
        drop(guard);
    }
}

pub struct SharedMutexGuard<'a, T: ?Sized + 'a> {
    lock: &'a SharedMutex<T>,

    _phantom_data: PhantomData<*const ()>, // make it !Send
}

unsafe impl<T: ?Sized + Sync> Sync for SharedMutexGuard<'_, T> {}

impl<'a, T: ?Sized> SharedMutexGuard<'a, T> {
    fn new(lock: &'a SharedMutex<T>) -> SharedMutexGuard<'a, T> {
        SharedMutexGuard {
            lock,
            _phantom_data: PhantomData,
        }
    }
}

impl<T: ?Sized> Deref for SharedMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for SharedMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for SharedMutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            self.lock.inner.unlock();
        }
    }
}
