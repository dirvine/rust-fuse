//!
//! Raw communication channel to the FUSE kernel driver.
//!

use std::{os, str};
use std::ffi::{CString, CStr, OsStr};
use std::path::{PathBuf, Path};
use libc::{c_int, c_void, size_t};
use fuse::{fuse_args, fuse_mount_compat25, fuse_unmount_compat22};

// Libc provides iovec based I/O using readv and writev functions
#[allow(dead_code, non_camel_case_types)]
mod libc {
    use libc::{c_char, c_int, c_void, size_t, ssize_t};

    /// Iovec data structure for readv and writev calls.
    #[repr(C)]
    pub struct iovec {
        pub iov_base: *const c_void,
        pub iov_len: size_t,
    }

    extern "system" {
        /// Read data from fd into multiple buffers
        pub fn readv (fd: c_int, iov: *mut iovec, iovcnt: c_int) -> ssize_t;
        /// Write data from multiple buffers to fd
        pub fn writev (fd: c_int, iov: *const iovec, iovcnt: c_int) -> ssize_t;

        pub fn realpath (file_name: *const c_char, resolved_name: *mut c_char) -> *const c_char;

        pub fn unmount(dir: *const c_char, flags: c_int) -> c_int;
    }

    /// Max length for path names. 4096 should be reasonable safe (OS X uses 1024, Linux uses 4096)
    pub const PATH_MAX: usize = 4096;
}

/// Wrapper around libc's realpath.  Returns the errno value if the real path cannot be obtained.
/// FIXME: Use Rust's realpath method once available in std (see also https://github.com/mozilla/rust/issues/11857)
fn real_path (path: &CStr) -> Result<CString, i32> {
    let mut resolved = [0; libc::PATH_MAX];
    unsafe {
        if libc::realpath(path.as_ptr(), resolved.as_mut_ptr()).is_null() {
            Err(os::errno())
        } else {
            // FIXME: Build CString from &[c_char] in a more elegant way
            let cresolved = CStr::from_ptr(resolved.as_ptr());
            Ok(CString::new(cresolved.to_bytes()).unwrap())
        }
    }
}

/// Helper function to provide options as a fuse_args struct
/// (which contains an argc count and an argv pointer)
fn with_fuse_args<T, F: FnOnce(&fuse_args) -> T> (options: &[&OsStr], f: F) -> T {
    let mut args: Vec<CString> = vec![CString::new("rust-fuse").unwrap()];
    // FIXME: Convert &OsStr to CString without utf-8 restrictions and without copying
    args.extend(options.iter().map(|s| CString::new(s.to_str().unwrap()).unwrap() ));
    let argptrs: Vec<*const i8> = args.iter().map(|s| s.as_ptr()).collect();
    f(&fuse_args { argc: argptrs.len() as i32, argv: argptrs.as_ptr(), allocated: 0 })
}

/// A raw communication channel to the FUSE kernel driver
pub struct Channel {
    mountpoint: PathBuf,
    fd: c_int,
}

impl Channel {
    /// Create a new communication channel to the kernel driver by mounting the
    /// given path. The kernel driver will delegate filesystem operations of
    /// the given path to the channel. If the channel is dropped, the path is
    /// unmounted.
    pub fn new (mountpoint: &Path, options: &[&OsStr]) -> Result<Channel, i32> {
        // FIXME: Convert &Path to CStr without utf-8 restrictions and without copying
        let mnt = CString::new(mountpoint.to_str().unwrap()).unwrap();
        real_path(&mnt).and_then(|mnt| {
            with_fuse_args(options, |args| {
                let fd = unsafe { fuse_mount_compat25(mnt.as_ptr(), args) };
                if fd < 0 {
                    Err(os::errno())
                } else {
                    // FIXME: Convert CString to PathBuf without utf-8 restrictions and without copying
                    let mountpoint = PathBuf::new(str::from_utf8(mnt.as_bytes()).unwrap());
                    Ok(Channel { mountpoint: mountpoint, fd: fd })
                }
            })
        })
    }

    /// Return path of the mounted filesystem
    pub fn mountpoint (&self) -> &Path {
        &self.mountpoint
    }

    /// Receives data up to the capacity of the given buffer (can block).
    pub fn receive (&self, buffer: &mut Vec<u8>) -> Result<(), i32> {
        let rc = unsafe { ::libc::read(self.fd, buffer.as_ptr() as *mut c_void, buffer.capacity() as size_t) };
        if rc < 0 {
            Err(os::errno())
        } else {
            unsafe { buffer.set_len(rc as usize); }
            Ok(())
        }
    }

    /// Returns a sender object for this channel. The sender object can be
    /// used to send to the channel. Multiple sender objects can be used
    /// and they can safely be sent to other threads.
    pub fn sender (&self) -> ChannelSender {
        // Since write/writev syscalls are threadsafe, we can simply create
        // a sender by using the same fd and use it in other threads. Only
        // the channel closes the fd when dropped. If any sender is used after
        // dropping the channel, it'll return an EBADF error.
        ChannelSender { fd: self.fd }
    }
}

impl Drop for Channel {
    fn drop (&mut self) {
        // TODO: send ioctl FUSEDEVIOCSETDAEMONDEAD on OS X before closing the fd
        // Close the communication channel to the kernel driver
        // (closing it before unnmount prevents sync unmount deadlock)
        unsafe { ::libc::close(self.fd); }
        // Unmount this channel's mount point
        unmount(&self.mountpoint);
    }
}

#[derive(Copy)]
pub struct ChannelSender {
    fd: c_int,
}

impl ChannelSender {
    /// Send all data in the slice of slice of bytes in a single write (can block).
    pub fn send (&self, buffer: &[&[u8]]) -> Result<(), i32> {
        let iovecs: Vec<libc::iovec> = buffer.iter().map(|d| {
            libc::iovec { iov_base: d.as_ptr() as *const c_void, iov_len: d.len() as size_t }
        }).collect();
        let rc = unsafe { libc::writev(self.fd, iovecs.as_ptr(), iovecs.len() as c_int) };
        if rc < 0 {
            Err(os::errno())
        } else {
            Ok(())
        }
    }
}

/// Unmount an arbitrary mount point
pub fn unmount (mountpoint: &Path) {
    // On OS X, fuse_unmount_compat22 attempts to call realpath, which in turn calls into the filesystem.
    // If the filesystem returns an error, the unmount does not take place, with no indication of the error
    // available to the caller.  So we call unmount directly, which is what osxfuse does anyway, since
    // we already converted to the real path when we first mounted.
    // FIXME: Convert &Path to CStr without utf-8 restrictions and without copying
    let mnt = CString::new(mountpoint.to_str().unwrap()).unwrap();
    if cfg!(target_os = "macos") {
        unsafe { libc::unmount(mnt.as_ptr(), 0); }
    } else {
        unsafe { fuse_unmount_compat22(mnt.as_ptr()); }
    }
}


#[cfg(test)]
mod test {
    use super::with_fuse_args;
    use std::ffi::{CStr, OsStr};

    #[test]
    fn fuse_args () {
        with_fuse_args(&[OsStr::from_str("foo"), OsStr::from_str("bar")], |args| {
            assert!(args.argc == 3);
            assert_eq!(unsafe { CStr::from_ptr(*args.argv.offset(0)).to_bytes() }, b"rust-fuse");
            assert_eq!(unsafe { CStr::from_ptr(*args.argv.offset(1)).to_bytes() }, b"foo");
            assert_eq!(unsafe { CStr::from_ptr(*args.argv.offset(2)).to_bytes() }, b"bar");
        });
    }
}
