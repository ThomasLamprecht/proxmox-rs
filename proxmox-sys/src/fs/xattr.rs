//! Wrapper functions for the libc xattr calls

use std::ffi::CStr;
use std::os::unix::io::RawFd;

use nix::errno::Errno;

use proxmox_io::vec;
use proxmox_lang::c_str;

/// `"security.capability"` as a CStr to avoid typos.
///
/// This cannot be `const` until `const_cstr_unchecked` is stable.
#[inline]
pub fn xattr_name_fcaps() -> &'static CStr {
    c_str!("security.capability")
}

/// `"system.posix_acl_access"` as a CStr to avoid typos.
///
/// This cannot be `const` until `const_cstr_unchecked` is stable.
#[inline]
pub fn xattr_acl_access() -> &'static CStr {
    c_str!("system.posix_acl_access")
}

/// `"system.posix_acl_default"` as a CStr to avoid typos.
///
/// This cannot be `const` until `const_cstr_unchecked` is stable.
#[inline]
pub fn xattr_acl_default() -> &'static CStr {
    c_str!("system.posix_acl_default")
}

/// Result of `flistxattr`, allows iterating over the attributes as a list of `&CStr`s.
///
/// Listing xattrs produces a list separated by zeroes, inherently making them available as `&CStr`
/// already, so we make use of this fact and reflect this in the interface.
pub struct ListXAttr {
    data: Vec<u8>,
}

impl ListXAttr {
    fn new(data: Vec<u8>) -> Self {
        Self { data }
    }
}

impl<'a> IntoIterator for &'a ListXAttr {
    type Item = &'a CStr;
    type IntoIter = ListXAttrIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        ListXAttrIter {
            data: &self.data,
            at: 0,
        }
    }
}

/// Iterator over the extended attribute entries in a `ListXAttr`.
pub struct ListXAttrIter<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Iterator for ListXAttrIter<'a> {
    type Item = &'a CStr;

    fn next(&mut self) -> Option<&'a CStr> {
        let data = &self.data[self.at..];
        let next = data.iter().position(|b| *b == 0)? + 1;
        self.at += next;
        Some(unsafe { CStr::from_bytes_with_nul_unchecked(&data[..next]) })
    }
}

/// Return a list of extended attributes accessible as an iterator over items of type `&CStr`.
pub fn flistxattr(fd: RawFd) -> Result<ListXAttr, nix::errno::Errno> {
    // Initial buffer size for the attribute list, if content does not fit
    // it gets dynamically increased until big enough.
    let mut size = 256;
    let mut buffer = vec::undefined(size);
    let mut bytes =
        unsafe { libc::flistxattr(fd, buffer.as_mut_ptr() as *mut libc::c_char, buffer.len()) };
    while bytes < 0 {
        let err = Errno::last();
        match err {
            Errno::ERANGE => {
                // Buffer was not big enough to fit the list, retry with double the size
                size = size.checked_mul(2).ok_or(Errno::ENOMEM)?;
            }
            _ => return Err(err),
        }
        // Retry to read the list with new buffer
        buffer.resize(size, 0);
        bytes =
            unsafe { libc::flistxattr(fd, buffer.as_mut_ptr() as *mut libc::c_char, buffer.len()) };
    }
    buffer.truncate(bytes as usize);

    Ok(ListXAttr::new(buffer))
}

/// Get an extended attribute by name.
///
/// Extended attributes may not contain zeroes, which we enforce in the API by using a `&CStr`
/// type.
pub fn fgetxattr(fd: RawFd, name: &CStr) -> Result<Vec<u8>, nix::errno::Errno> {
    let mut size = 256;
    let mut buffer = vec::undefined(size);
    let mut bytes = unsafe {
        libc::fgetxattr(
            fd,
            name.as_ptr(),
            buffer.as_mut_ptr() as *mut core::ffi::c_void,
            buffer.len(),
        )
    };
    while bytes < 0 {
        let err = Errno::last();
        match err {
            Errno::ERANGE => {
                // Buffer was not big enough to fit the value, retry with double the size
                size = size.checked_mul(2).ok_or(Errno::ENOMEM)?;
            }
            _ => return Err(err),
        }
        buffer.resize(size, 0);
        bytes = unsafe {
            libc::fgetxattr(
                fd,
                name.as_ptr() as *const libc::c_char,
                buffer.as_mut_ptr() as *mut core::ffi::c_void,
                buffer.len(),
            )
        };
    }
    buffer.resize(bytes as usize, 0);

    Ok(buffer)
}

/// Set an extended attribute on a file descriptor.
pub fn fsetxattr(fd: RawFd, name: &CStr, data: &[u8]) -> Result<(), nix::errno::Errno> {
    let result = unsafe {
        libc::fsetxattr(
            fd,
            name.as_ptr(),
            data.as_ptr() as *const libc::c_void,
            data.len(),
            0,
        )
    };
    if result < 0 {
        return Err(Errno::last());
    }

    Ok(())
}

pub fn fsetxattr_fcaps(fd: RawFd, fcaps: &[u8]) -> Result<(), nix::errno::Errno> {
    // TODO casync checks and removes capabilities if they are set
    fsetxattr(fd, xattr_name_fcaps(), fcaps)
}

pub fn is_security_capability(name: &CStr) -> bool {
    name.to_bytes() == xattr_name_fcaps().to_bytes()
}

pub fn is_acl(name: &CStr) -> bool {
    name.to_bytes() == xattr_acl_access().to_bytes()
        || name.to_bytes() == xattr_acl_default().to_bytes()
}

/// Check if the passed name buffer starts with a valid xattr namespace prefix
/// and is within the length limit of 255 bytes
pub fn is_valid_xattr_name(c_name: &CStr) -> bool {
    let name = c_name.to_bytes();
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    if name.starts_with(b"user.") || name.starts_with(b"trusted.") {
        return true;
    }
    // samba saves windows ACLs there
    if name == b"security.NTACL" {
        return true;
    }
    is_security_capability(c_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::CString;

    use proxmox_lang::c_str;

    #[test]
    fn test_is_valid_xattr_name() {
        let too_long = CString::new(vec![b'a'; 265]).unwrap();

        assert!(!is_valid_xattr_name(&too_long));
        assert!(!is_valid_xattr_name(c_str!("system.attr")));
        assert!(is_valid_xattr_name(c_str!("user.attr")));
        assert!(is_valid_xattr_name(c_str!("trusted.attr")));
        assert!(is_valid_xattr_name(super::xattr_name_fcaps()));
    }
}
