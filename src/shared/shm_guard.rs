//! Safety net for client shm pools that shrink underneath us.
//!
//! A pool is a file the client owns. Nothing stops it calling `ftruncate` to
//! make that file smaller after the compositor has mapped it, and reading a
//! mapped page that no longer has a file behind it raises `SIGBUS`. Because
//! the compositor reads client buffers directly rather than copying them, that
//! read can happen deep inside the GL driver on the backend thread — so an
//! unhandled `SIGBUS` takes down the compositor and every client with it. Any
//! client could do it, by accident or on purpose.
//!
//! Three defenses, cheapest first:
//!
//! 1. [`prepare_pool_file`] refuses to map a pool larger than the file behind
//!    it, so a client that simply declares the wrong size gets an error rather
//!    than a landmine.
//! 2. The same function tries to seal the file against shrinking. Clients using
//!    libwayland's shm helpers hand over a `memfd` that accepts the seal, which
//!    makes truncation impossible for the life of the pool.
//! 3. For anything left — a file that cannot be sealed, shrunk anyway — a
//!    `SIGBUS` handler maps a page of zeroes over the hole so the faulting read
//!    retries and succeeds. The client sees black where its buffer used to be,
//!    which is the correct outcome for a client that broke its own promise.

use std::os::unix::io::RawFd;
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{info, warn};

/// How many pool mappings the `SIGBUS` net can cover at once.
///
/// A client normally has one or two pools. Beyond this a mapping still works,
/// it is simply not covered, so the cap trades a fixed table for never
/// allocating or locking inside a signal handler.
const MAX_GUARDED: usize = 512;

/// Marks a slot as claimed but not yet filled in. Never matches an address,
/// because a slot only matches once its real base has been published.
const CLAIMING: usize = usize::MAX;

/// Fallback page size
const FALLBACK_PAGE_SIZE: usize = 4096;

/// A fixed registry of guarded memory slots
static REGISTRY: [Slot; MAX_GUARDED] = [const {
    Slot {
        base: AtomicUsize::new(0),
        len: AtomicUsize::new(0),
    }
}; MAX_GUARDED];

/// Pages patched by the handler. Read from the compositor, which does the
/// logging: a signal handler cannot.
static PATCHED: AtomicUsize = AtomicUsize::new(0);

/// Installs the handler exactly once, however many pools are mapped.
///
/// `sigaction` is process-wide, so the handler belongs to the process rather
/// than to any one mapping; the thousandth pool has nothing to install.
static INSTALL: Once = Once::new();

/// The system page size, read once when the handler is installed.
///
/// Read here rather than in the handler so the fault path does as little as
/// possible, and so the question of whether `sysconf` may be called from a
/// signal handler never has to be answered.
static PAGE_SIZE: AtomicUsize = AtomicUsize::new(0);

/// One covered mapping.
struct Slot {
    /// Where the mapping starts — and, through two sentinel values, whether
    /// this slot holds one at all.
    ///
    /// `0` is free, [`CLAIMING`] is taken but not yet filled in, and anything
    /// else is a real base address. That is the whole of the lock-free
    /// protocol: `register` claims a slot by moving `base` off `0`, and only
    /// the final store of a real address publishes it to the handler.
    base: AtomicUsize,
    /// Length of the mapping in bytes.
    ///
    /// Meaningful only once `base` holds a real address. `register` writes it
    /// first and `base` second, so a slot caught mid-claim is never read as a
    /// range — the handler sees the sentinel and skips it.
    len: AtomicUsize,
}

/// Why a pool file cannot back the pool the client asked for.
#[derive(Debug)]
pub enum PoolFileError {
    /// The file could not be inspected at all.
    Unreadable,
    /// The client declared a pool larger than the file behind it.
    TooSmall { declared: u64, actual: u64 },
}

impl std::fmt::Display for PoolFileError {
    /// The message a client's failure is logged with. `Debug` is derived
    /// separately and reports the variant; this reports the numbers.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable => write!(f, "the pool file could not be inspected"),
            Self::TooSmall { declared, actual } => write!(
                f,
                "the client declared {declared} bytes but the file holds {actual}"
            ),
        }
    }
}

/// How many pages have been replaced with zeroes after a `SIGBUS`.
///
/// Non-zero means some client shrank a pool it had already committed from, and
/// whatever it was showing there is now black.
pub fn patched_pages() -> usize {
    PATCHED.load(Ordering::Relaxed)
}

/// Cover a mapping with the `SIGBUS` net. Returns the slot to release later.
pub fn register(base: *mut libc::c_void, len: usize) -> Option<usize> {
    INSTALL.call_once(install_handler);

    let base = base as usize;
    for (index, slot) in REGISTRY.iter().enumerate() {
        if slot
            .base
            .compare_exchange(0, CLAIMING, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            // Length first, then base: the handler reads base and only trusts
            // the length once it has seen a real one.
            slot.len.store(len, Ordering::Release);
            slot.base.store(base, Ordering::Release);
            return Some(index);
        }
    }
    warn!("shm guard table full; a pool mapping is unprotected against truncation");
    None
}

/// Release a slot, once its mapping is being torn down.
///
/// The caller unmaps *after* this returns, which leaves an instant where the
/// mapping is live but uncovered. That is safe for the one reason that matters:
/// a `PoolMapping` is dropped only when its last `Arc` goes, so by then nobody
/// holds it and nobody can fault on it.
pub fn unregister(index: usize) {
    let Some(slot) = REGISTRY.get(index) else {
        return;
    };
    // Base first, so the handler stops matching before the length goes.
    slot.base.store(0, Ordering::Release);
    slot.len.store(0, Ordering::Release);
}

/// Checks whether or not an address is covered by the guarded regions
fn covers(addr: usize) -> bool {
    REGISTRY.iter().any(|slot| {
        let base = slot.base.load(Ordering::Acquire);
        if base == 0 || base == CLAIMING {
            return false;
        }
        let len = slot.len.load(Ordering::Acquire);
        addr >= base && addr < base.saturating_add(len)
    })
}

/// The system page size, as cached at install time.
fn page_size() -> usize {
    match PAGE_SIZE.load(Ordering::Acquire) {
        0 => FALLBACK_PAGE_SIZE,
        size => size,
    }
}

/// Ask the system for its page size. Called once, off the fault path.
fn read_page_size() -> usize {
    // SAFETY: `sysconf` takes no pointers and cannot fail meaningfully here.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(size).unwrap_or(FALLBACK_PAGE_SIZE).max(1)
}

/// Replace the unreadable page with zeroes and let the read retry.
///
/// The patch is permanent. A private anonymous page goes down over what was a
/// shared mapping, so that page is holed for the mapping's life — if the client
/// grows its file back, the region stays black.
///
/// An address that is not ours is left alone: the default handler goes back in
/// and the signal is re-raised, so unrelated `SIGBUS` bugs still crash the way
/// they would have. A net that swallowed them would hide real defects.
///
/// Everything here is async-signal-safe: atomic loads, `mmap`, `signal`, and
/// `raise`. No allocation, no locks, no logging.
extern "C" fn on_sigbus(_signal: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    if !info.is_null() {
        // SAFETY: the kernel hands us a valid `siginfo_t` for a SIGBUS.
        let addr = unsafe { (*info).si_addr() } as usize;
        if covers(addr) {
            let page = page_size();
            let aligned = addr & !(page - 1);
            // SAFETY: `aligned` is a page-aligned address inside a mapping we
            // own, so replacing that one page cannot disturb anything else.
            let result = unsafe {
                libc::mmap(
                    aligned as *mut libc::c_void,
                    page,
                    libc::PROT_READ,
                    libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if result != libc::MAP_FAILED {
                PATCHED.fetch_add(1, Ordering::Relaxed);
                // Returning retries the faulting instruction, which now reads
                // zeroes instead of faulting again.
                return;
            }
        }
    }

    // Not one of ours, or beyond repair. Die exactly as we would have without
    // a handler, rather than looping on the same fault forever.
    // SAFETY: both calls are async-signal-safe.
    unsafe {
        libc::signal(libc::SIGBUS, libc::SIG_DFL);
        libc::raise(libc::SIGBUS);
    }
}

/// Put the `SIGBUS` handler in place, and cache what it will need.
///
/// This takes `SIGBUS` for the whole process and does not chain: the previous
/// handler is discarded rather than saved, because `oldact` is null. Nothing
/// else here wants the signal, but that assumption lives in this line.
fn install_handler() {
    // Before the handler goes in, not after: once `sigaction` returns the
    // handler can run, and it must never find this unset.
    PAGE_SIZE.store(read_page_size(), Ordering::Release);

    // SAFETY: `action` is fully initialised before use, and the handler is
    // async-signal-safe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        // `sa_sigaction` is a raw address; the cast has to go through a
        // pointer rather than straight from the function item.
        action.sa_sigaction = on_sigbus as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&raw mut action.sa_mask);
        if libc::sigaction(libc::SIGBUS, &raw const action, std::ptr::null_mut()) != 0 {
            warn!("could not install SIGBUS handler; a truncated pool will be fatal");
        }
    }
}

/// Check a pool file is big enough, and stop it shrinking if the kernel lets us.
///
/// Sealing is best-effort: it succeeds for a `memfd` created with
/// `MFD_ALLOW_SEALING`, which is what libwayland's shm helpers produce, and
/// fails harmlessly for anything else. A pool that ends up sealed can never
/// trigger the handler above, because it can never shrink.
pub fn prepare_pool_file(fd: RawFd, size: u32) -> Result<(), PoolFileError> {
    // SAFETY: `fstat` only writes through the pointer it is given.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &raw mut stat) } != 0 {
        return Err(PoolFileError::Unreadable);
    }
    let actual = stat.st_size.max(0).cast_unsigned();
    if u64::from(size) > actual {
        return Err(PoolFileError::TooSmall {
            declared: u64::from(size),
            actual,
        });
    }

    // SAFETY: `fcntl` with these commands takes an int and touches no memory.
    unsafe {
        libc::fcntl(fd, libc::F_ADD_SEALS, libc::F_SEAL_SHRINK);
        let seals = libc::fcntl(fd, libc::F_GET_SEALS);
        if seals < 0 || seals & libc::F_SEAL_SHRINK == 0 {
            info!(
                "shm pool fd cannot be sealed against shrinking; relying on the SIGBUS net instead"
            );
        }
    }
    Ok(())
}
