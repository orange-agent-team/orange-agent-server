//! Audited native boundary. Every descriptor is borrowed for the call, native
//! allocations are owned by RAII guards, and ACL buffers are aligned/bounded.
use std::{
    io,
    os::fd::{AsFd, AsRawFd},
};
unsafe extern "C" {
    fn acl_get_fd(fd: libc::c_int) -> *mut libc::c_void;
    fn acl_size(acl: *mut libc::c_void) -> libc::ssize_t;
    fn acl_copy_ext(
        buffer: *mut libc::c_void,
        acl: *mut libc::c_void,
        length: libc::ssize_t,
    ) -> libc::ssize_t;
    fn acl_free(value: *mut libc::c_void) -> libc::c_int;
}
struct Acl(*mut libc::c_void);
impl Drop for Acl {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns one successful acl_get_fd allocation.
        unsafe {
            acl_free(self.0);
        }
    }
}
struct CopyState(rustix::fs::copyfile_state_t);
impl Drop for CopyState {
    fn drop(&mut self) {
        // SAFETY: allocated once, never exposed or freed before this drop.
        let _ = unsafe { rustix::fs::copyfile_state_free(self.0) };
    }
}
pub(super) fn acl_bytes(file: &impl AsFd) -> io::Result<Vec<u8>> {
    let fd = file.as_fd();
    // SAFETY: fd is live for this call; a non-null result is uniquely owned below.
    let raw = unsafe { acl_get_fd(fd.as_raw_fd()) };
    if raw.is_null() {
        let error = io::Error::last_os_error();
        // Darwin's fd API propagates ENOENT for an absent FILESEC_ACL
        // property (Libc gen/filesec.c), not a missing pathname. Recheck the
        // borrowed descriptor; permission, unsupported-FS and other errors
        // must remain errors. Never apply this rule to path-based APIs.
        if error.raw_os_error() == Some(libc::ENOENT) {
            rustix::fs::fstat(fd)?;
            return Ok(Vec::new());
        }
        return Err(error);
    }
    let acl = Acl(raw);
    // SAFETY: acl remains allocated and is not concurrently accessed.
    let size = unsafe { acl_size(acl.0) };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    let size = usize::try_from(size).map_err(|_| io::Error::other("ACL size"))?;
    if size > 64 * 1024 {
        return Err(io::Error::other("ACL size limit"));
    }
    let words = size.div_ceil(std::mem::size_of::<libc::c_long>()).max(1);
    let mut buffer = vec![0 as libc::c_long; words];
    let capacity = buffer.len() * std::mem::size_of::<libc::c_long>();
    // SAFETY: aligned initialized allocation has capacity bytes; ACL lives
    // throughout the call. C only writes this buffer, with no retained pointer.
    let copied =
        unsafe { acl_copy_ext(buffer.as_mut_ptr().cast(), acl.0, capacity as libc::ssize_t) };
    if copied < 0 {
        return Err(io::Error::last_os_error());
    }
    let copied = usize::try_from(copied).map_err(|_| io::Error::other("ACL copied size"))?;
    if copied > capacity {
        return Err(io::Error::other("ACL copy exceeded capacity"));
    }
    let mut bytes: Vec<u8> = buffer
        .into_iter()
        .flat_map(libc::c_long::to_ne_bytes)
        .collect();
    bytes.truncate(copied);
    Ok(bytes)
}
pub(super) fn copy_security(source: &impl AsFd, target: &impl AsFd) -> io::Result<()> {
    let source = source.as_fd();
    let target = target.as_fd();
    let state = CopyState(rustix::fs::copyfile_state_alloc()?);
    // SAFETY: state was allocated above and stays live through the call; both
    // descriptors are borrowed live objects. Only documented metadata flags.
    unsafe {
        rustix::fs::fcopyfile(
            source,
            target,
            state.0,
            rustix::fs::CopyfileFlags::SECURITY | rustix::fs::CopyfileFlags::XATTR,
        )
    }?;
    Ok(())
}
