use std::path::Path;
use std::os::unix::io::AsRawFd;
use std::mem::MaybeUninit;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::FromRawFd;
use std::ffi::{CStr, CString};

use anyhow::{bail, format_err, Error};
use nix::fcntl::OFlag;
use nix::sys::mman::{MapFlags, ProtFlags};
use nix::sys::stat::Mode;
use nix::errno::Errno;

use proxmox_sys::fs::CreateOptions;
use proxmox_sys::mmap::Mmap;
use proxmox_sys::error::SysError;

mod raw_shared_mutex;

mod shared_mutex;
pub use shared_mutex::*;

/// Data inside SharedMemory need to implement this trait
///
/// IMPORTANT: Please use #[repr(C)] for all types implementing this
pub trait Init: Sized {
    /// Make sure the data struicture is initialized. This is called
    /// after mapping into shared memory. The caller makes sure that
    /// no other process run this at the same time.
    fn initialize(this: &mut MaybeUninit<Self>);

    /// Check if the data has the correct format
    fn check_type_magic(_this: &MaybeUninit<Self>) -> Result<(), Error> { Ok(()) }
}

/// Memory mapped shared memory region
///
/// This allows access to same memory region for multiple
/// processes. You should only use atomic types from 'std::sync::atomic', or
/// protect the data with [SharedMutex].
///
/// SizeOf(T) needs to be a multiple of 4096 (the page size).
pub struct SharedMemory<T> {
    mmap: Mmap<T>,
}

const fn up_to_page_size(n: usize) ->  usize {
    // FIXME: use sysconf(_SC_PAGE_SIZE)
    (n + 4095) & !4095
}

fn mmap_file<T: Init>(file: &mut File, initialize: bool) -> Result<Mmap<T>, Error> {
    // map it as MaybeUninit
    let mut mmap: Mmap<MaybeUninit<T>> = unsafe {
        Mmap::map_fd(
            file.as_raw_fd(),
            0, 1,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_SHARED | MapFlags::MAP_NORESERVE | MapFlags::MAP_POPULATE,
        )?
    };

    if initialize {
        Init::initialize(&mut mmap[0]);
    }

    match Init::check_type_magic(&mut mmap[0]) {
        Ok(()) => (),
        Err(err) => bail!("detected wrong types in mmaped files: {}", err),
    }

    Ok(unsafe { std::mem::transmute(mmap) })
}

impl <T: Sized + Init> SharedMemory<T> {

    pub fn open(path: &Path, options: CreateOptions) -> Result<Self, Error> {

        let size = std::mem::size_of::<T>();
        let up_size = up_to_page_size(size);

        if size != up_size {
            bail!("SharedMemory::open {:?} failed - data size {} is not a multiple of 4096", path, size);
        }

        let mmap = Self::open_shmem(path, options)?;

        Ok(Self { mmap })
    }

    pub fn open_shmem<P: AsRef<Path>>(
        path: P,
        options: CreateOptions,
    ) -> Result<Mmap<T>, Error> {
        let path = path.as_ref();

        let dir_name = path
            .parent()
            .ok_or_else(|| format_err!("bad path {:?}", path))?
            .to_owned();

        if !cfg!(test) {
            let statfs = nix::sys::statfs::statfs(&dir_name)?;
            if statfs.filesystem_type() != nix::sys::statfs::TMPFS_MAGIC {
                bail!("path {:?} is not on tmpfs", dir_name);
            }
        }

        let oflag = OFlag::O_RDWR | OFlag::O_CLOEXEC;

        // Try to open existing file
        match nix::fcntl::open(path, oflag, Mode::empty()) {
            Ok(fd) => {
                let mut file = unsafe { File::from_raw_fd(fd) };
                let mmap = mmap_file(&mut file, false)?;
                return Ok(mmap);
            }
            Err(err) => {
                if err.not_found() {
                    // fall true -  try to create the file
                } else {
                    bail!("open {:?} failed - {}", path, err);
                }
            }
        }

        // create temporary file using O_TMPFILE
        let mut file = match nix::fcntl::open(&dir_name, oflag | OFlag::O_TMPFILE, Mode::empty()) {
            Ok(fd) => {
                let mut file = unsafe { File::from_raw_fd(fd) };
                options.apply_to(&mut file, &dir_name)?;
                file
            }
            Err(err) => {
                bail!("open tmpfile in {:?} failed - {}", dir_name, err);
            }
        };

        let size = std::mem::size_of::<T>();
        let size = up_to_page_size(size);

        nix::unistd::ftruncate(file.as_raw_fd(), size as i64)?;

        // link the file into place:
        let proc_path = format!("/proc/self/fd/{}\0", file.as_raw_fd());
        let proc_path = unsafe { CStr::from_bytes_with_nul_unchecked(proc_path.as_bytes()) };

        let mmap = mmap_file(&mut file, true)?;

        let res = {
            let path = CString::new(path.as_os_str().as_bytes())?;
            Errno::result(unsafe {
                libc::linkat(
                    -1,
                    proc_path.as_ptr(),
                    libc::AT_FDCWD,
                    path.as_ptr(),
                    libc::AT_SYMLINK_FOLLOW,
                )
            })
        };

        drop(file); // no longer required

        match res {
            Ok(_rc) => return Ok(mmap),
            // if someone else was faster, open the existing file:
            Err(nix::Error::Sys(Errno::EEXIST)) =>  {
                // if opening fails again now, we'll just error...
                match nix::fcntl::open(path, oflag, Mode::empty()) {
                    Ok(fd) => {
                        let mut file = unsafe { File::from_raw_fd(fd) };
                        let mmap = mmap_file(&mut file, false)?;
                        return Ok(mmap);
                    }
                    Err(err) => bail!("open {:?} failed - {}", path, err),
                };
            }
            Err(err) =>  return Err(err.into()),
        }
    }

    pub fn data(&self) -> &T {
        &self.mmap[0]
    }

    pub fn data_mut(&mut self) -> &mut T {
        &mut self.mmap[0]
    }

}

/// Helper to initialize nested data
pub unsafe fn initialize_subtype<T: Init>(this: &mut T) {
    let data: &mut MaybeUninit<T> = std::mem::transmute(this);
    Init::initialize(data);
}

/// Helper to call 'check_type_magic' for nested data
pub unsafe fn check_subtype<T: Init>(this: &T) -> Result<(), Error> {
    let data: &MaybeUninit<T> = std::mem::transmute(this);
    Init::check_type_magic(data)
}

#[cfg(test)]
mod test {

    use super::*;

    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::thread::spawn;
    use proxmox_sys::fs::create_path;

    #[derive(Debug)]
    #[repr(C)]
    struct TestData {
        count: u64,
        value1: u64,
        value2: u64,
    }

    impl Init for TestData {

        fn initialize(this: &mut MaybeUninit<Self>) {
            this.write(Self {
                count: 0,
                value1: 0xffff_ffff_ffff_0000,
                value2: 0x0000_ffff_ffff_ffff,
            });
        }
    }

    struct SingleMutexData {
        data: SharedMutex<TestData>,
        _padding: [u8; 4096 - 64 - 8],
    }

    impl Init for SingleMutexData {
        fn initialize(this: &mut MaybeUninit<Self>) {
            unsafe {
                let me = &mut *this.as_mut_ptr();
                initialize_subtype(&mut me.data);
            }
        }

        fn check_type_magic(this: &MaybeUninit<Self>) -> Result<(), Error> {
            unsafe {
                let me = &*this.as_ptr();
                check_subtype(&me.data)
            }
        }
    }

    #[test]
    fn test_shared_memory_mutex() -> Result<(), Error> {

        create_path("../target/testdata/", None, None)?;

        let shared: SharedMemory<SingleMutexData> =
            SharedMemory::open(Path::new("../target/testdata/test1.shm"), CreateOptions::new())?;

        let shared = Arc::new(shared);

        let start = shared.data().data.lock().count;

        let threads: Vec<_> = (0..100)
            .map(|_| {
                let shared = shared.clone();
                spawn(move || {
                    let mut guard = shared.data().data.lock();
                    println!("DATA {:?}", *guard);
                    guard.count += 1;
                    println!("DATA {:?}", *guard);
                })
            })
            .collect();

        for thread in threads {
            thread.join().unwrap();
        }

        let end = shared.data().data.lock().count;

        assert_eq!(end-start, 100);

        Ok(())
    }

    #[derive(Debug)]
    #[repr(C)]
    struct MultiMutexData {
        acount: AtomicU64,
        block1: SharedMutex<TestData>,
        block2: SharedMutex<TestData>,
        padding: [u8; 4096 - 136 - 16],
    }

    impl Init for MultiMutexData {
        fn initialize(this: &mut MaybeUninit<Self>) {
            unsafe {
                let me = &mut *this.as_mut_ptr();
                initialize_subtype(&mut me.block1);
                initialize_subtype(&mut me.block2);
            }
        }

        fn check_type_magic(this: &MaybeUninit<Self>) -> Result<(), Error> {
            unsafe {
                let me = &*this.as_ptr();
                check_subtype(&me.block1)?;
                check_subtype(&me.block2)?;
                Ok(())
            }
        }
    }

    #[test]
    fn test_shared_memory_multi_mutex() -> Result<(), Error> {

        create_path("../target/testdata/", None, None)?;

        let shared: SharedMemory<MultiMutexData> =
            SharedMemory::open(Path::new("../target/testdata/test2.shm"), CreateOptions::new())?;

                let shared = Arc::new(shared);

        let start1 = shared.data().block1.lock().count;
        let start2 = shared.data().block2.lock().count;

        let threads: Vec<_> = (0..100)
            .map(|_| {
                let shared = shared.clone();
                spawn(move || {
                    let mut guard = shared.data().block1.lock();
                    println!("BLOCK1 {:?}", *guard);
                    guard.count += 1;
                    let mut guard = shared.data().block2.lock();
                    println!("BLOCK2 {:?}", *guard);
                    guard.count += 2;
                })
            })
            .collect();

        for thread in threads {
            thread.join().unwrap();
        }

        let end1 = shared.data().block1.lock().count;
        assert_eq!(end1-start1, 100);

        let end2 = shared.data().block2.lock().count;
        assert_eq!(end2-start2, 200);

        Ok(())
    }

}
