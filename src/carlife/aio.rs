//! Linux AIO, the little of it a bulk endpoint needs.
//!
//! A read on a FunctionFS endpoint queues one `usb_request` and completes when
//! the host's transfer ends. `read(2)` holds the endpoint's mutex for the whole
//! of it and reuses a single request, so only one request can be outstanding,
//! and between one read returning and the next being queued there is nothing
//! for the endpoint to receive into.
//!
//! That gap costs a packet. The controller takes a packet into its receive fifo
//! whether or not a request is queued, acknowledges it to the host, and then
//! flushes the fifo when the next request is queued. `io_submit` takes the
//! mutex only to queue and allocates a request per submission, so several can
//! be outstanding at once and the queue never runs dry. `gadget` keeps a few
//! that way.
//!
//! None of this is in `libc`, so the syscalls and the two structures are
//! declared here. The layout is `uapi/linux/aio_abi.h` and is asserted at
//! compile time; a wrong one shows up as `EBADF`.

use std::io;
use std::mem::{offset_of, size_of};
use std::os::unix::io::RawFd;

/// `IOCB_CMD_PREAD`, from `uapi/linux/aio_abi.h`.
const PREAD: u16 = 0;

/// One submitted request, as the kernel reads it.
///
/// `aio_key` and `aio_rw_flags` share one word of the header, which is why
/// `aio_lio_opcode` is at sixteen and not twelve.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Iocb {
    aio_data: u64,
    aio_key: u32,
    aio_rw_flags: u32,
    aio_lio_opcode: u16,
    aio_reqprio: i16,
    aio_fildes: u32,
    aio_buf: u64,
    aio_nbytes: u64,
    aio_offset: i64,
    aio_reserved2: u64,
    aio_flags: u32,
    aio_resfd: u32,
}

/// One finished request, as the kernel reports it.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct IoEvent {
    /// Whatever the `iocb` put in `aio_data`.
    pub data: u64,
    /// The `iocb` itself, as the kernel saw it.
    pub obj: u64,
    /// Bytes read, or the negative error.
    pub res: i64,
    pub res2: i64,
}

const _: () = {
    assert!(size_of::<Iocb>() == 64);
    assert!(offset_of!(Iocb, aio_lio_opcode) == 16);
    assert!(offset_of!(Iocb, aio_fildes) == 20);
    assert!(offset_of!(Iocb, aio_buf) == 24);
    assert!(size_of::<IoEvent>() == 32);
};

/// A set of requests the kernel keeps for one file.
///
/// The handle is `aio_context_t`, which is `unsigned long` — one word on every
/// architecture. It is held as `c_ulong` and passed to `syscall` the same way,
/// and so are the counts: `syscall` is variadic, and a `u64` argument takes two
/// registers on a 32-bit target, which pushes every argument after it into the
/// wrong one. The kernel then reads a count out of a pointer and answers
/// `EINVAL`.
pub struct Context(libc::c_ulong);

impl Context {
    /// Makes room for `depth` requests in flight at once.
    pub fn new(depth: usize) -> io::Result<Self> {
        let mut ctx: libc::c_ulong = 0;
        checked(unsafe {
            libc::syscall(
                libc::SYS_io_setup,
                depth as libc::c_int,
                &mut ctx as *mut libc::c_ulong,
            )
        })?;
        Ok(Self(ctx))
    }

    /// Queues one read of `buf` from `fd`, and returns without waiting.
    ///
    /// The bytes arrive through `wait`, tagged with `data`. `buf` belongs to the
    /// kernel until then and must not be touched.
    pub fn submit(&self, fd: RawFd, buf: &mut [u8], data: u64) -> io::Result<()> {
        let mut cb = Iocb {
            aio_data: data,
            aio_lio_opcode: PREAD,
            aio_fildes: fd as u32,
            aio_buf: buf.as_mut_ptr() as u64,
            aio_nbytes: buf.len() as u64,
            // An endpoint has no offset, and its driver ignores this.
            aio_offset: 0,
            ..Iocb::default()
        };
        let mut list = [&mut cb as *mut Iocb];
        let queued = checked(unsafe {
            libc::syscall(
                libc::SYS_io_submit,
                self.0,
                1 as libc::c_long,
                list.as_mut_ptr(),
            )
        })?;
        if queued != 1 {
            return Err(io::Error::other("io_submit queued nothing"));
        }
        Ok(())
    }

    /// Waits until at least one request has finished.
    pub fn wait(&self, events: &mut [IoEvent]) -> io::Result<usize> {
        let done = checked(unsafe {
            libc::syscall(
                libc::SYS_io_getevents,
                self.0,
                1 as libc::c_long,
                events.len() as libc::c_long,
                events.as_mut_ptr(),
                std::ptr::null::<libc::timespec>(),
            )
        })?;
        Ok(done as usize)
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // Waits for whatever is still queued. A host that is gone has had them
        // cancelled.
        unsafe { libc::syscall(libc::SYS_io_destroy, self.0) };
    }
}

/// A syscall's return value, as a result. A failure is negative with `errno`
/// set, which is what `last_os_error` reads.
fn checked(rc: libc::c_long) -> io::Result<libc::c_long> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::io::AsRawFd;

    /// A context, or `None` where the kernel has no AIO at all.
    ///
    /// `ENOSYS` is a kernel built without `CONFIG_AIO`, or an emulator that
    /// does not forward the syscall — neither says anything about this code.
    fn context(depth: usize) -> Option<Context> {
        match Context::new(depth) {
            Ok(ctx) => Some(ctx),
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => None,
            Err(e) => panic!("a context: {e}"),
        }
    }

    /// The structures are only right if the kernel agrees, and the only way to
    /// ask is to make it read one.
    #[test]
    fn the_kernel_reads_what_this_lays_out() {
        let Some(ctx) = context(4) else { return };

        let path = std::env::temp_dir().join("cp2cl-aio-test");
        std::fs::write(&path, b"aio reads this").unwrap();
        let file = std::fs::File::open(&path).unwrap();

        let mut buf = [0u8; 8];
        ctx.submit(file.as_raw_fd(), &mut buf, 7)
            .expect("a queued read");

        let mut events = [IoEvent::default(); 4];
        let done = ctx.wait(&mut events).expect("a finished read");
        assert_eq!(done, 1);
        assert_eq!(events[0].data, 7);
        assert_eq!(events[0].res, 8);
        assert_eq!(&buf, b"aio read");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_an_error_rather_than_a_hang() {
        let Some(ctx) = context(4) else { return };

        let mut buf = [0u8; 8];
        let error = ctx.submit(-1, &mut buf, 0).expect_err("a refused submit");
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }
}
